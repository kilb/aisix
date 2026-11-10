import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E (R-070): a tool call's `arguments` is a JSON document inside a string,
// and it reaches the upstream exactly as written. A document may name one key
// twice; a parser that keeps the last value then reads `"safe"` out of
// `{"x":"alice@example.com","x":"safe"}` while the bytes carrying the address
// go upstream all the same. An enforcing PII rule, mask or block, has to judge
// the bytes the gateway actually sends.
//
// So: an address in any value of a repeated key is refused before the
// upstream is called, whether the rule masks or blocks, and whether the
// address is written plainly or behind a JSON escape. A document that repeats
// a key and matches nothing goes out byte for byte, and a single-valued key is
// masked as before. Chat Completions and Responses requests carry the string
// in the request; a Chat Completions answer carries it in the response.

const SECRET = "alice@example.com";
const ESCAPED = "alice\\u0040example.com";
const REPEATED_SECRET = `{"x":"${SECRET}","x":"safe"}`;
const REPEATED_SECRET_LAST = `{"x":"safe","x":"${SECRET}"}`;
const REPEATED_ESCAPED = `{"x":"${ESCAPED}","x":"safe"}`;
const REPEATED_CLEAN = `{"x":"a","x":"b"}`;
const UNIQUE_SECRET = `{"to":"${SECRET}"}`;
const TOOL_OUT = "answer-with-a-tool-call";
const MASK_KEY = "sk-repeated-key-mask";
const BLOCK_KEY = "sk-repeated-key-block";

const hash = (s: string) => createHash("sha256").update(s).digest("hex");
type Json = Record<string, unknown>;

/** An OpenAI upstream that keeps every raw request body it is sent. A chat
 * request whose last message asks for it is answered with a tool call whose
 * arguments repeat a key and carry the secret. */
async function startMock(): Promise<{ baseUrl: string; bodies: string[]; close(): Promise<void> }> {
  const bodies: string[] = [];
  const server: Server = createServer((req, res) => {
    res.on("error", () => {});
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf8");
      bodies.push(raw);
      const path = (req.url ?? "/").split("?")[0];
      let body: Json = {};
      try {
        body = JSON.parse(raw) as Json;
      } catch {
        body = {};
      }
      const model = String(body.model ?? "");
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      if (path === "/v1/responses") {
        res.end(
          JSON.stringify({
            id: `resp_${randomUUID()}`,
            object: "response",
            created_at: 0,
            model,
            status: "completed",
            output: [
              {
                type: "message",
                id: "msg_1",
                role: "assistant",
                status: "completed",
                content: [{ type: "output_text", text: "ok", annotations: [] }],
              },
            ],
            usage: { input_tokens: 5, output_tokens: 1, total_tokens: 6 },
          }),
        );
        return;
      }
      const messages = (body.messages as Json[] | undefined) ?? [];
      const last = messages[messages.length - 1];
      const toolOut = typeof last?.content === "string" && last.content.includes(TOOL_OUT);
      const message = toolOut
        ? {
            role: "assistant",
            content: null,
            tool_calls: [
              {
                id: "call_out",
                type: "function",
                function: { name: "send", arguments: REPEATED_SECRET },
              },
            ],
          }
        : { role: "assistant", content: "ok" };
      res.end(
        JSON.stringify({
          id: `chatcmpl-${randomUUID()}`,
          object: "chat.completion",
          created: 0,
          model,
          choices: [{ index: 0, message, finish_reason: toolOut ? "tool_calls" : "stop" }],
          usage: { prompt_tokens: 5, completion_tokens: 1, total_tokens: 6 },
        }),
      );
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    bodies,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

/** A chat history whose earlier assistant turn called a tool with `args`. */
function chatWithToolCall(model: string, args: string): Json {
  return {
    model,
    messages: [
      { role: "user", content: "send the note" },
      {
        role: "assistant",
        content: null,
        tool_calls: [{ id: "call_1", type: "function", function: { name: "send", arguments: args } }],
      },
      { role: "tool", tool_call_id: "call_1", content: "sent" },
      { role: "user", content: "thanks" },
    ],
  };
}

describe("an enforcing PII rule judges every value of a repeated tool-argument key", () => {
  let app: SpawnedApp | undefined;
  let mock: Awaited<ReturnType<typeof startMock>> | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    mock = await startMock();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "repeated-key-openai",
      secret: "sk-mock",
      api_base: `${mock.baseUrl}/v1`,
    });
    const mask = await seed.createGuardrail(
      {
        name: "repeated-key-mask",
        enabled: true,
        kind: "pii",
        hook_point: "both",
        detectors: [{ type: "email", action: "mask" }],
      },
      { attach: false },
    );
    const block = await seed.createGuardrail(
      {
        name: "repeated-key-block",
        enabled: true,
        kind: "pii",
        hook_point: "both",
        detectors: [{ type: "email", action: "block" }],
      },
      { attach: false },
    );
    for (const [name, rule] of [
      ["repeated-key-mask", mask],
      ["repeated-key-block", block],
    ] as const) {
      const model = await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
      await seed.attachGuardrailToModel(rule.id as string, model.id as string);
    }
    // Keys last: once the last one authenticates, the rest is in the snapshot.
    await seed.createApiKey({ key_hash: hash(MASK_KEY), allowed_models: ["repeated-key-mask"] });
    await seed.createApiKey({ key_hash: hash(BLOCK_KEY), allowed_models: ["repeated-key-block"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${BLOCK_KEY}` },
      });
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await mock?.close();
  });

  const post = async (path: string, key: string, body: Json) => {
    const sentBefore = mock!.bodies.length;
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return {
      status: res.status,
      body: await res.text(),
      /** The raw bodies the upstream received for this request. */
      upstream: () => mock!.bodies.slice(sentBefore),
    };
  };

  const expectRefusedBeforeUpstream = (res: Awaited<ReturnType<typeof post>>) => {
    for (const raw of res.upstream()) {
      expect(raw, "the address reached the upstream").not.toContain(SECRET);
      expect(raw, "the escaped address reached the upstream").not.toContain(ESCAPED);
    }
    expect(res.status, res.body).toBe(422);
    expect(res.upstream(), "a refused request is not sent").toHaveLength(0);
  };

  for (const [rule, key, model] of [
    ["mask", MASK_KEY, "repeated-key-mask"],
    ["block", BLOCK_KEY, "repeated-key-block"],
  ] as const) {
    for (const [what, args] of [
      ["the first of two values", REPEATED_SECRET],
      ["the last of two values", REPEATED_SECRET_LAST],
      ["a JSON-escaped value", REPEATED_ESCAPED],
    ] as const) {
      test(`chat request, ${rule} rule: an address in ${what} of a repeated key is refused`, async (ctx) => {
        if (!etcdReachable) ctx.skip();
        const res = await post("/v1/chat/completions", key, chatWithToolCall(model, args));
        expectRefusedBeforeUpstream(res);
      });
    }

    test(`responses request, ${rule} rule: an address in a repeated key is refused`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      const res = await post("/v1/responses", key, {
        model,
        input: [
          { type: "message", role: "user", content: "send the note" },
          { type: "function_call", call_id: "call_1", name: "send", arguments: REPEATED_ESCAPED },
          { type: "function_call_output", call_id: "call_1", output: "sent" },
          { type: "message", role: "user", content: "thanks" },
        ],
      });
      expectRefusedBeforeUpstream(res);
    });

    test(`chat answer, ${rule} rule: an address in a repeated key never reaches the caller`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      const res = await post("/v1/chat/completions", key, {
        model,
        messages: [{ role: "user", content: TOOL_OUT }],
      });
      expect(res.body, "the address reached the caller").not.toContain(SECRET);
      expect(res.status, res.body).toBe(422);
    });
  }

  test("a repeated key that matches nothing is forwarded byte for byte", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await post(
      "/v1/chat/completions",
      MASK_KEY,
      chatWithToolCall("repeated-key-mask", REPEATED_CLEAN),
    );
    expect(res.status, res.body).toBe(200);
    const sent = res.upstream();
    expect(sent).toHaveLength(1);
    const forwarded = JSON.parse(sent[0]) as { messages: Array<{ tool_calls?: Json[] }> };
    const call = forwarded.messages[1].tool_calls?.[0] as { function: { arguments: string } };
    expect(call.function.arguments).toBe(REPEATED_CLEAN);
  });

  test("a single-valued key is masked as before", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await post(
      "/v1/chat/completions",
      MASK_KEY,
      chatWithToolCall("repeated-key-mask", UNIQUE_SECRET),
    );
    expect(res.status, res.body).toBe(200);
    const sent = res.upstream();
    expect(sent).toHaveLength(1);
    expect(sent[0]).not.toContain(SECRET);
    const forwarded = JSON.parse(sent[0]) as { messages: Array<{ tool_calls?: Json[] }> };
    const call = forwarded.messages[1].tool_calls?.[0] as { function: { arguments: string } };
    expect(JSON.parse(call.function.arguments)).toEqual({ to: "[EMAIL_REDACTED]" });
  });
});
