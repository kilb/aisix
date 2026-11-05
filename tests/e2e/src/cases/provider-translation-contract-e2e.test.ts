import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  eventStream,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: fields that have a place on the upstream's wire reach it, values with
// no place there do not get it refused, and what the upstream answers comes
// back to the caller.
//
// - Anthropic takes `service_tier` as `auto` or `standard_only`
//   (https://platform.claude.com/docs/en/api/messages), and its SDK types the
//   cache counters as nullable.
// - A Converse stream ends a tool call with no parameters without any input,
//   and reports `model_context_window_exceeded`
//   (https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html).
// - A chat upstream refuses by `refusal` with `content: null`.

const CALLER = "sk-provider-translation-contract";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const CLAUDE = "contract-claude";
const VERTEX_CLAUDE = "contract-vertex-claude";
const COMPAT = "contract-compat";
const AZURE = "contract-azure";
const NOVA = "contract-nova";
const GEMINI = "contract-gemini";

interface Sent {
  path: string;
  body: Record<string, any>;
}

const claudeAnswer = {
  id: "msg_1",
  type: "message",
  role: "assistant",
  model: "claude",
  content: [{ type: "text", text: "Paris." }],
  stop_reason: "end_turn",
  usage: { input_tokens: 3, output_tokens: 2, cache_creation_input_tokens: null, cache_read_input_tokens: null },
};

const refusal = "I can't help with that.";

function converse(prompt: string): Buffer {
  const usage: [string, unknown] = [
    "metadata",
    { usage: { inputTokens: 3, outputTokens: 2, totalTokens: 5 }, metrics: { latencyMs: 1 } },
  ];
  if (prompt.includes("what time")) {
    return eventStream([
      ["messageStart", { role: "assistant" }],
      ["contentBlockStart", { contentBlockIndex: 0, start: { toolUse: { toolUseId: "t1", name: "now" } } }],
      ["contentBlockStop", { contentBlockIndex: 0 }],
      ["messageStop", { stopReason: "tool_use" }],
      usage,
    ]);
  }
  return eventStream([
    ["messageStart", { role: "assistant" }],
    ["contentBlockDelta", { contentBlockIndex: 0, delta: { text: "cut" } }],
    ["contentBlockStop", { contentBlockIndex: 0 }],
    [
      "messageStop",
      { stopReason: "model_context_window_exceeded", additionalModelResponseFields: { stop_sequence: "END" } },
    ],
    usage,
  ]);
}

describe("request fields and answers follow the upstream's contract", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  const sent: Sent[] = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = createServer((req, res) => {
      const parts: Buffer[] = [];
      req.on("data", (c: Buffer) => parts.push(c));
      req.on("end", () => {
        const path = decodeURIComponent(req.url ?? "");
        const raw = Buffer.concat(parts).toString("utf8");
        const body = raw ? JSON.parse(raw) : {};
        sent.push({ path, body });
        const json = (value: unknown, status = 200) => {
          res.writeHead(status, { "content-type": "application/json" });
          res.end(JSON.stringify(value));
        };
        if (path.endsWith("/v1/messages") || path.includes(":rawPredict")) {
          // The Messages API takes these two values and refuses any other.
          const tier = body.service_tier;
          if (tier !== undefined && !["auto", "standard_only"].includes(tier)) {
            return json({ type: "error", error: { type: "invalid_request_error", message: "service_tier" } }, 400);
          }
          if (tier !== undefined && path.includes(":rawPredict")) {
            return json({ type: "error", error: { type: "invalid_request_error", message: "service_tier" } }, 400);
          }
          return json(claudeAnswer);
        }
        if (path.includes(":generateContent")) {
          return json({ error: { code: 503, message: "the model is overloaded", status: "UNAVAILABLE" } });
        }
        if (path.includes("/converse-stream")) {
          res.writeHead(200, { "content-type": "application/vnd.amazon.eventstream" });
          return res.end(converse(JSON.stringify(body.messages)));
        }
        if (body.stream) {
          const frame = (delta: unknown, finish: string | null) =>
            `data: ${JSON.stringify({ id: "c", model: "m", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
          res.writeHead(200, { "content-type": "text/event-stream" });
          return res.end(`${frame({ role: "assistant", refusal }, null)}${frame({}, "stop")}data: [DONE]\n\n`);
        }
        return json({
          id: "c",
          model: "m",
          choices: [{ index: 0, message: { role: "assistant", content: null, refusal }, finish_reason: "stop" }],
          usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 },
        });
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const addr = upstream.address();
    if (addr === null || typeof addr === "string") throw new Error("no port");
    const baseUrl = `http://127.0.0.1:${addr.port}`;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const vertex = await seed.createProviderKey({
      display_name: "contract-vertex-pk",
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "t", project: "p", region: "us-east5" }),
      api_base: baseUrl,
    });
    const pks = [
      [CLAUDE, "anthropic", "claude-sonnet-4-5", { provider: "anthropic", adapter: "anthropic", secret: "k" }],
      [COMPAT, "openai", "some-model", { secret: "k" }],
      [AZURE, "azure-openai", "gpt4o-prod", { provider: "azure-openai", adapter: "azure-openai", secret: "k" }],
      [
        NOVA,
        "bedrock",
        "amazon.nova-lite-v1:0",
        {
          provider: "bedrock",
          adapter: "bedrock",
          secret: JSON.stringify({ access_key_id: "AKIA", secret_access_key: "s", region: "us-west-2" }),
        },
      ],
    ] as const;
    for (const [display_name, provider, model_name, pk] of pks) {
      const created = await seed.createProviderKey({
        display_name: `${display_name}-pk`,
        api_base: display_name === COMPAT ? `${baseUrl}/v1` : baseUrl,
        ...pk,
      } as Parameters<typeof seed.createProviderKey>[0]);
      await seed.createModel({ display_name, provider, model_name, provider_key_id: created.id });
    }
    await seed.createModel({
      display_name: VERTEX_CLAUDE,
      provider: "google",
      model_name: "claude-sonnet-4-5",
      provider_key_id: vertex.id,
    });
    await seed.createModel({
      display_name: GEMINI,
      provider: "google",
      model_name: "gemini-2.5-flash",
      provider_key_id: vertex.id,
    });
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["*"] });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await new Promise<void>((resolve) => (upstream ? upstream.close(() => resolve()) : resolve()));
  });

  async function post(route: string, body: unknown) {
    const res = await fetch(`${app!.proxyUrl}${route}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, raw: await res.text() };
  }

  const chat = (model: string, extra: Record<string, unknown> = {}, text = "hi") =>
    post("/v1/chat/completions", { model, messages: [{ role: "user", content: text }], ...extra });

  function chunks(raw: string): any[] {
    return raw
      .split("\n\n")
      .map((frame) => frame.split("\n").find((l) => l.startsWith("data: "))?.slice(6))
      .filter((d): d is string => d !== undefined && d !== "[DONE]")
      .map((d) => JSON.parse(d));
  }

  test("every OpenAI service_tier is answered on Claude, and maps where it can", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const [tier, expected] of [
      ["default", "standard_only"],
      ["auto", "auto"],
      ["flex", undefined],
      ["priority", undefined],
    ] as const) {
      const out = await chat(CLAUDE, { service_tier: tier });
      expect(out.status, `${tier}: ${out.raw}`).toBe(200);
      expect(sent.at(-1)!.body.service_tier, tier).toBe(expected);
    }
  });

  test("Claude on Vertex AI answers whatever service_tier the caller sends", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await chat(VERTEX_CLAUDE, { service_tier: "auto" });
    expect(out.status, out.raw).toBe(200);
  });

  test("a Claude answer with null cache counters reaches the caller", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await chat(CLAUDE);
    expect(out.status, out.raw).toBe(200);
    expect(JSON.parse(out.raw).choices[0].message.content).toBe("Paris.");
  });

  test("a Gemini 200 that reports an error is a failure, not an empty answer", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await chat(GEMINI);
    expect(out.status, out.raw).toBeGreaterThanOrEqual(500);
    expect(out.raw).toContain("the model is overloaded");
  });

  test("a refusal reaches a Responses caller, buffered and streamed", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const buffered = await post("/v1/responses", { model: AZURE, input: "hi" });
    expect(buffered.status, buffered.raw).toBe(200);
    expect(JSON.parse(buffered.raw).output[0].content).toEqual([{ type: "refusal", refusal }]);

    const streamed = await post("/v1/responses", { model: AZURE, input: "hi", stream: true });
    expect(streamed.status, streamed.raw).toBe(200);
    const events = chunks(streamed.raw);
    expect(events.some((e) => e.type === "response.refusal.delta" && e.delta === refusal), streamed.raw).toBe(true);
    const completed = events.find((e) => e.type === "response.completed");
    expect(completed.response.output[0].content).toEqual([{ type: "refusal", refusal }]);
  });

  test("a /v1/messages document reaches a chat upstream in the part that takes it", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await post("/v1/messages", {
      model: COMPAT,
      max_tokens: 16,
      messages: [
        {
          role: "user",
          content: [
            { type: "document", source: { type: "text", media_type: "text/plain", data: "the contract" } },
            { type: "document", title: "a.pdf", source: { type: "base64", media_type: "application/pdf", data: "JVBERi0=" } },
            { type: "text", text: "summarize" },
          ],
        },
      ],
    });
    expect(out.status, out.raw).toBe(200);
    const parts = sent.at(-1)!.body.messages.at(-1).content;
    expect(parts[0]).toEqual({ type: "text", text: "the contract" });
    expect(parts[1]).toEqual({
      type: "file",
      file: { filename: "a.pdf", file_data: "data:application/pdf;base64,JVBERi0=" },
    });
  });

  test("a streamed Converse tool call with no parameters has JSON arguments", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await chat(
      NOVA,
      {
        stream: true,
        tools: [{ type: "function", function: { name: "now", parameters: { type: "object", properties: {} } } }],
      },
      "what time is it?",
    );
    expect(out.status, out.raw).toBe(200);
    const args = chunks(out.raw)
      .flatMap((c) => c.choices?.[0]?.delta?.tool_calls ?? [])
      .map((t: any) => t.function.arguments ?? "")
      .join("");
    expect(JSON.parse(args)).toEqual({});
  });

  test("a Converse stream cut at the context window finishes as length, with the fields asked for", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await chat(NOVA, { stream: true, additionalModelResponseFieldPaths: ["/stop_sequence"] });
    expect(out.status, out.raw).toBe(200);
    const finish = chunks(out.raw).find((c) => c.choices?.[0]?.finish_reason != null);
    expect(finish.choices[0].finish_reason).toBe("length");
    expect(finish.choices[0].delta.additionalModelResponseFields).toEqual({ stop_sequence: "END" });
  });
});
