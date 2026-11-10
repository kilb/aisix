import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  awaitWindowHeadroom,
  EtcdClient,
  SeedClient,
  slsLogsFor,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E (R-031): an answer an output MASK rule refuses because the match sits
// in a tool call's object key is paid for like one it lets through.
//
// A mask cannot rename a key the tool's schema names, so the gateway refuses
// the answer instead of forwarding it. That refusal happens after the
// upstream answered and billed, exactly like a block: the request's usage row
// has to carry the answer's tokens, cache counters, tier and cost, and the
// key's token budget has to be spent, or a caller who keeps tripping the rule
// spends the operator's upstream money with nothing recorded and nothing
// limiting it.
//
// Every buffered surface a client SDK uses is driven, each with its own
// models and keys: Messages relayed from a native Anthropic upstream,
// Messages translated from a Chat Completions upstream, Chat Completions, and
// Responses. A clean answer of the same shape is the control each refused one
// is compared with.

const CREDENTIAL_REF = "mock";
const LOGSTORE = "output-mask-unrewritable-billing";
const SECRET = "alice@example.com";
const TRIGGER = "LEAK-THE-ADDRESS";
// Low enough that one answer exhausts it, high enough that a request is
// admitted before any answer was committed against it.
const TPM = 900;
const COST = {
  input_per_1k: 1,
  output_per_1k: 1,
  cached_input_per_1k: 0.1,
  cache_write_per_1k: 2,
};

const hash = (s: string) => createHash("sha256").update(s).digest("hex");

type Json = Record<string, unknown>;
type Endpoint = "chat" | "messages" | "responses";
type Upstream = "openai" | "anthropic";

interface Cell {
  name: string;
  endpoint: Endpoint;
  upstream: Upstream;
  /** What the upstream is asked for: the mock counts its calls per model. */
  upstreamModel: string;
  billingKey: string;
  tpmKey: string;
}

const CELLS: Cell[] = (
  [
    ["messages", "anthropic", "claude-3-5-haiku-20241022"],
    ["messages", "openai", "gpt-msg-cell"],
    ["chat", "openai", "gpt-chat-cell"],
    ["responses", "openai", "gpt-resp-cell"],
  ] as Array<[Endpoint, Upstream, string]>
).map(([endpoint, upstream, upstreamModel]) => {
  const name = `unrw-${endpoint}-${upstream}`;
  return {
    name,
    endpoint,
    upstream,
    upstreamModel,
    billingKey: `sk-${name}-billing`,
    tpmKey: `sk-${name}-tpm`,
  };
});

/** Every string in a request body, so the trigger is found whatever the protocol. */
function textOf(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textOf).join(" ");
  if (value && typeof value === "object") return Object.values(value).map(textOf).join(" ");
  return "";
}

/** An upstream that answers with one tool call. Its arguments object is keyed
 * by the secret when the prompt carries the trigger, and by a clean key
 * otherwise; everything else, the usage included, is the same. */
async function startMock(): Promise<{
  baseUrl: string;
  calls: Map<string, number>;
  close(): Promise<void>;
}> {
  const calls = new Map<string, number>();
  const server: Server = createServer((req, res: ServerResponse) => {
    res.on("error", () => {});
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = (req.url ?? "/").split("?")[0];
      let body: Json = {};
      try {
        body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Json;
      } catch {
        body = {};
      }
      const model = String(body.model ?? "");
      calls.set(model, (calls.get(model) ?? 0) + 1);
      const leak = textOf(body.messages ?? body.input).includes(TRIGGER);
      const args = { [leak ? SECRET : "recipient"]: "hi" };
      const json = (payload: unknown) => {
        res.statusCode = 200;
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(payload));
      };

      if (path === "/v1/messages") {
        return json({
          id: `msg_${randomUUID()}`,
          type: "message",
          role: "assistant",
          model,
          content: [{ type: "tool_use", id: "toolu_1", name: "send", input: args }],
          stop_reason: "tool_use",
          usage: {
            input_tokens: 500,
            output_tokens: 500,
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 40,
            service_tier: "priority",
          },
        });
      }
      if (path === "/v1/chat/completions") {
        return json({
          id: `chatcmpl-${randomUUID()}`,
          object: "chat.completion",
          created: 0,
          model,
          service_tier: "priority",
          choices: [
            {
              index: 0,
              message: {
                role: "assistant",
                content: null,
                tool_calls: [
                  {
                    id: "call_1",
                    type: "function",
                    function: { name: "send", arguments: JSON.stringify(args) },
                  },
                ],
              },
              finish_reason: "tool_calls",
            },
          ],
          usage: {
            prompt_tokens: 500,
            completion_tokens: 500,
            total_tokens: 1000,
            prompt_tokens_details: { cached_tokens: 40 },
          },
        });
      }
      if (path === "/v1/responses") {
        return json({
          id: `resp_${randomUUID()}`,
          object: "response",
          created_at: 0,
          model,
          status: "completed",
          service_tier: "priority",
          output: [
            {
              type: "function_call",
              id: "fc_1",
              call_id: "call_1",
              name: "send",
              arguments: JSON.stringify(args),
              status: "completed",
            },
          ],
          usage: {
            input_tokens: 500,
            output_tokens: 500,
            total_tokens: 1000,
            input_tokens_details: { cached_tokens: 40 },
          },
        });
      }
      res.statusCode = 404;
      res.end(JSON.stringify({ error: { message: `no route for ${path}` } }));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    calls,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

// What a usage row bills and how it accounts for it. Each is compared between
// the refused answer and the clean one.
const BILLED_FIELDS = [
  "cost_usd",
  "prompt_tokens",
  "completion_tokens",
  "total_tokens",
  "cached_prompt_tokens",
  "cache_read_tokens",
  "cache_creation_tokens",
  "service_tier",
];

describe("an answer an output mask refuses over a tool key is billed like one it delivers", () => {
  let app: SpawnedApp | undefined;
  let mock: Awaited<ReturnType<typeof startMock>> | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    mock = await startMock();
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-output-mask-unrewritable-billing",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });

    const openai = await seed.createProviderKey({
      display_name: "unrw-openai",
      secret: "sk-mock",
      api_base: `${mock.baseUrl}/v1`,
    });
    const anthropic = await seed.createProviderKey({
      display_name: "unrw-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: mock.baseUrl,
    });
    const rule = await seed.createGuardrail(
      {
        name: "unrw-output-mask",
        enabled: true,
        kind: "pii",
        hook_point: "output",
        detectors: [{ type: "email", action: "mask" }],
      },
      { attach: false },
    );
    for (const cell of CELLS) {
      const model = await seed.createModel({
        display_name: cell.name,
        provider: cell.upstream,
        model_name: cell.upstreamModel,
        provider_key_id: cell.upstream === "anthropic" ? anthropic.id : openai.id,
        cost: COST,
      });
      await seed.attachGuardrailToModel(rule.id as string, model.id as string);
    }

    // Keys last: once the last one authenticates, the rest is in the snapshot.
    for (const cell of CELLS) {
      await seed.createApiKey({ key_hash: hash(cell.billingKey), allowed_models: [cell.name] });
      await seed.createApiKey({
        key_hash: hash(cell.tpmKey),
        allowed_models: [cell.name],
        rate_limit: { tpm: TPM },
      });
    }
    const last = CELLS[CELLS.length - 1];
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${last.tpmKey}` },
      });
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await mock?.close();
    await sls?.close();
  });

  const send = async (cell: Cell, key: string, text: string) => {
    const path = {
      chat: "/v1/chat/completions",
      messages: "/v1/messages",
      responses: "/v1/responses",
    }[cell.endpoint];
    const body: Json =
      cell.endpoint === "responses"
        ? { model: cell.name, input: text }
        : cell.endpoint === "messages"
          ? { model: cell.name, max_tokens: 64, messages: [{ role: "user", content: text }] }
          : { model: cell.name, messages: [{ role: "user", content: text }] };
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "x-api-key": key,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    return {
      status: res.status,
      body: await res.text(),
      requestId: res.headers.get("x-aisix-request-id") ?? "",
    };
  };

  /** The one usage row `requestId` records. */
  const rowOf = async (requestId: string) => {
    expect(requestId, "the response names its request").not.toBe("");
    await waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      `the usage row of ${requestId}`,
      20_000,
    );
    const rows = slsLogsFor(sls!, LOGSTORE).filter((log) => log.get("request_id") === requestId);
    expect(rows, `one usage row for ${requestId}`).toHaveLength(1);
    return rows[0];
  };

  for (const cell of CELLS) {
    test(`${cell.name}: the refused answer bills what the delivered one does`, async (ctx) => {
      if (!etcdReachable) ctx.skip();

      // The control: the same answer with a clean key goes through.
      const clean = await send(cell, cell.billingKey, "send it");
      expect(clean.status, clean.body).toBe(200);
      expect(clean.body).toContain("recipient");

      // The answer whose tool key the mask matches is refused, and the key
      // never reaches the caller.
      const refused = await send(cell, cell.billingKey, `send it ${TRIGGER}`);
      expect(refused.status, refused.body).toBe(422);
      expect(refused.body).not.toContain(SECRET);

      const cleanRow = await rowOf(clean.requestId);
      const refusedRow = await rowOf(refused.requestId);
      expect(Number(cleanRow.get("cost_usd") ?? "0"), "the control bills").toBeGreaterThan(0);
      expect(cleanRow.get("prompt_tokens")).toBe("500");
      expect(cleanRow.get("completion_tokens")).toBe("500");
      for (const field of BILLED_FIELDS) {
        expect(
          refusedRow.get(field),
          `${field}: the refused answer was paid upstream but its row does not bill it`,
        ).toBe(cleanRow.get(field));
      }
      expect(refusedRow.get("status_code")).toBe("422");
      expect(refusedRow.get("guardrail_blocked")).toBe("true");
    });

    test(`${cell.name}: the refused answer spends the key's token budget`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      await awaitWindowHeadroom();

      const first = await send(cell, cell.tpmKey, `send it ${TRIGGER}`);
      expect(first.status, first.body).toBe(422);
      const calls = mock!.calls.get(cell.upstreamModel) ?? 0;

      // One answer is over the budget, so the next request is turned away
      // before the upstream is asked again.
      const second = await send(cell, cell.tpmKey, `send it ${TRIGGER}`);
      expect(second.status, second.body).toBe(429);
      expect(mock!.calls.get(cell.upstreamModel) ?? 0, "no further upstream call").toBe(calls);
    });
  }
});
