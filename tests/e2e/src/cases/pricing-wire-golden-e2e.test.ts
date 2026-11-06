import { createHash, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockControlPlane,
  waitConfigPropagation,
  type MockControlPlane,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: the usage rows a managed gateway delivers to its control plane, one
// request per provider family, captured as the golden input of the control
// plane's pricing engine (`crates/aisix-pricing/tests/golden.rs`).
//
// The mock upstream answers each provider's surface with fixed usage in that
// provider's documented shape. The rows are checked against that usage, then
// reduced to the keys the engine reads plus `guardrail_blocked` and
// `cost_usd`, with the ids and timestamps that change on every run replaced
// by stable names, and compared with the committed files.
// `UPDATE_PRICING_GOLDEN=1` rewrites the files instead. Keeping only those
// keys means an unrelated new field cannot churn the files, while a renamed
// priced field fails here.
//
// One upstream call bills identically whichever inbound protocol or delivery
// addressed it, so a stream and a cross-protocol call record the same row as
// the native call, apart from the row's own id and operation.
//
// Every model carries the same `cost`, which is the default rate of the
// engine's golden card, so each row's `cost_usd` is the gateway's own spend
// figure for the price the engine applies.
//
// Usage shapes:
// - Chat Completions: https://platform.openai.com/docs/api-reference/chat/object
//   (`prompt_tokens_details.cached_tokens`, `completion_tokens_details.reasoning_tokens`,
//   `service_tier`) and the final `stream_options.include_usage` chunk.
// - Responses: https://platform.openai.com/docs/api-reference/responses/object
//   (`input_tokens_details.cached_tokens`, `output_tokens_details.reasoning_tokens`),
//   carried on `response.completed` when streamed.
// - Embeddings: https://platform.openai.com/docs/api-reference/embeddings/object
// - Messages: https://docs.anthropic.com/en/api/messages (`cache_creation`,
//   `server_tool_use.web_search_requests`); a stream carries the input side on
//   `message_start` and the cumulative counts on `message_delta`
//   (https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/web-search-tool).
// - Count tokens: https://docs.anthropic.com/en/api/messages-count-tokens
// - Gemini: https://ai.google.dev/api/generate-content#UsageMetadata
// - Bedrock Converse: https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_TokenUsage.html
//   and API_runtime_CacheDetail.html.

const CALLER = "sk-pricing-wire-golden";
const CACHE_CALLER = "sk-pricing-wire-golden-cached";
const hash = (key: string) => createHash("sha256").update(key).digest("hex");

const GOLDEN_DIR = fileURLToPath(
  new URL("../../../../crates/aisix-pricing/tests/golden/wire/", import.meta.url),
);
const UPDATING = process.env.UPDATE_PRICING_GOLDEN === "1";
const GOLDEN_INSTANT = "2026-11-06T00:00:00.000Z";

/** Model.cost: the golden card's `default` base, per 1,000 units. */
const COST = {
  input_per_1k: 0.001,
  output_per_1k: 0.002,
  cached_input_per_1k: 0.0001,
  cache_write_per_1k: 0.00125,
};

type Provider = "openai" | "anthropic" | "google" | "bedrock";

/** The families that address a model of their own. */
const NATIVE = {
  "openai-chat": { provider: "openai", upstreamModel: "gpt-5-mini" },
  "openai-responses": { provider: "openai", upstreamModel: "gpt-5" },
  "openai-embeddings": { provider: "openai", upstreamModel: "text-embedding-3-small" },
  "anthropic-messages": { provider: "anthropic", upstreamModel: "claude-sonnet-4-5-20250929" },
  "anthropic-count-tokens": { provider: "anthropic", upstreamModel: "claude-haiku-4-5-20251001" },
  "gemini-chat": { provider: "google", upstreamModel: "gemini-2.5-flash" },
  "gemini-cached": { provider: "google", upstreamModel: "gemini-2.5-pro" },
  // A model Bedrock answers through Converse; Claude's chat goes to InvokeModel.
  "bedrock-converse-chat": { provider: "bedrock", upstreamModel: "us.amazon.nova-pro-v1:0" },
} as const satisfies Record<string, { provider: Provider; upstreamModel: string }>;
type Native = keyof typeof NATIVE;

const modelName = (native: Native) => `golden-${native}`;

interface Family {
  name: string;
  native: Native;
  path: string;
  body: Record<string, unknown>;
  /** Sent first under a cache policy, so the family's own request is a hit. */
  cached?: boolean;
}

const MESSAGES = [{ role: "user", content: "golden" }];
const chat = (native: Native, stream = false) => ({
  model: modelName(native),
  messages: MESSAGES,
  ...(stream ? { stream: true, stream_options: { include_usage: true } } : {}),
});
const messages = (native: Native, stream = false) => ({
  model: modelName(native),
  max_tokens: 64,
  messages: MESSAGES,
  ...(stream ? { stream: true } : {}),
});
const responses = (native: Native, stream = false) => ({
  model: modelName(native),
  input: "golden",
  ...(stream ? { stream: true } : {}),
});

const FAMILIES: Family[] = [
  { name: "openai-chat", native: "openai-chat", path: "/v1/chat/completions", body: chat("openai-chat") },
  {
    name: "openai-chat-stream",
    native: "openai-chat",
    path: "/v1/chat/completions",
    body: chat("openai-chat", true),
  },
  { name: "openai-via-messages", native: "openai-chat", path: "/v1/messages", body: messages("openai-chat") },
  {
    name: "openai-responses",
    native: "openai-responses",
    path: "/v1/responses",
    body: responses("openai-responses"),
  },
  {
    name: "openai-responses-stream",
    native: "openai-responses",
    path: "/v1/responses",
    body: responses("openai-responses", true),
  },
  {
    name: "anthropic-messages",
    native: "anthropic-messages",
    path: "/v1/messages",
    body: messages("anthropic-messages"),
  },
  {
    name: "anthropic-messages-stream",
    native: "anthropic-messages",
    path: "/v1/messages",
    body: messages("anthropic-messages", true),
  },
  {
    name: "anthropic-via-chat",
    native: "anthropic-messages",
    path: "/v1/chat/completions",
    body: chat("anthropic-messages"),
  },
  { name: "gemini-chat", native: "gemini-chat", path: "/v1/chat/completions", body: chat("gemini-chat") },
  { name: "gemini-cached", native: "gemini-cached", path: "/v1/chat/completions", body: chat("gemini-cached") },
  {
    name: "bedrock-converse-chat",
    native: "bedrock-converse-chat",
    path: "/v1/chat/completions",
    body: chat("bedrock-converse-chat"),
  },
  {
    name: "openai-embeddings",
    native: "openai-embeddings",
    path: "/v1/embeddings",
    body: { model: modelName("openai-embeddings"), input: "golden" },
  },
  {
    name: "anthropic-count-tokens",
    native: "anthropic-count-tokens",
    path: "/v1/messages/count_tokens",
    body: { model: modelName("anthropic-count-tokens"), messages: MESSAGES },
  },
  {
    name: "cache-hit-messages",
    native: "anthropic-messages",
    path: "/v1/messages",
    body: messages("anthropic-messages"),
    cached: true,
  },
  {
    name: "cache-hit-chat",
    native: "openai-chat",
    path: "/v1/chat/completions",
    body: chat("openai-chat"),
    cached: true,
  },
];

/** The keys the pricing engine's `BillableUsage` reads off a row. */
const BILLABLE_KEYS = [
  "event_id",
  "occurred_at",
  "team_id",
  "requested_model_id",
  "operation",
  "status_code",
  "cache_status",
  "prompt_tokens",
  "completion_tokens",
  "cached_prompt_tokens",
  "cache_write_tokens",
  "reasoning_tokens",
  "total_tokens",
  "cache_creation_tokens",
  "cache_creation_5m_tokens",
  "cache_creation_1h_tokens",
  "cache_read_tokens",
  "prompt_audio_tokens",
  "prompt_image_tokens",
  "completion_audio_tokens",
  "completion_image_tokens",
  "web_search_requests",
  "usage_estimated",
  "service_tier",
  "requested_service_tier",
  "is_batch",
  "upstream_provider",
  "upstream_model",
  "provider_key_id",
];
/** What a golden row keeps: the billable keys, and the gateway's own verdicts. */
const KEPT_KEYS = [...BILLABLE_KEYS, "guardrail_blocked", "cost_usd"].sort();

type Row = Record<string, unknown>;

// ---- the mock upstream: every provider's surface, answered with fixed usage ----

const OPENAI_CHAT_USAGE = {
  prompt_tokens: 1000,
  completion_tokens: 200,
  total_tokens: 1200,
  prompt_tokens_details: { cached_tokens: 800 },
  completion_tokens_details: { reasoning_tokens: 50 },
};

function openAiChat(model: unknown): unknown {
  return {
    id: "chatcmpl-golden",
    object: "chat.completion",
    created: 1,
    model,
    service_tier: "flex",
    choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
    usage: OPENAI_CHAT_USAGE,
  };
}

function openAiChatStream(model: unknown): string {
  const chunk = (choices: unknown[], usage?: unknown) => ({
    id: "chatcmpl-golden",
    object: "chat.completion.chunk",
    created: 1,
    model,
    service_tier: "flex",
    choices,
    ...(usage === undefined ? {} : { usage }),
  });
  return [
    chunk([{ index: 0, delta: { role: "assistant", content: "ok" }, finish_reason: null }]),
    chunk([{ index: 0, delta: {}, finish_reason: "stop" }]),
    chunk([], OPENAI_CHAT_USAGE),
  ]
    .map((c) => `data: ${JSON.stringify(c)}\n\n`)
    .concat("data: [DONE]\n\n")
    .join("");
}

function openAiResponse(model: unknown, status: "in_progress" | "completed"): Record<string, unknown> {
  return {
    id: "resp_golden",
    object: "response",
    created_at: 1,
    model,
    status,
    output:
      status === "completed"
        ? [
            {
              type: "message",
              id: "msg_golden",
              status: "completed",
              role: "assistant",
              content: [{ type: "output_text", text: "ok", annotations: [] }],
            },
          ]
        : [],
    usage:
      status === "completed"
        ? {
            input_tokens: 2000,
            input_tokens_details: { cached_tokens: 1500 },
            output_tokens: 300,
            output_tokens_details: { reasoning_tokens: 120 },
            total_tokens: 2300,
          }
        : null,
  };
}

const typedSse = (events: Array<Record<string, unknown>>) =>
  events.map((e) => `event: ${String(e.type)}\ndata: ${JSON.stringify(e)}\n\n`).join("");

function openAiResponseStream(model: unknown): string {
  return typedSse([
    { type: "response.created", sequence_number: 0, response: openAiResponse(model, "in_progress") },
    {
      type: "response.output_text.delta",
      sequence_number: 1,
      item_id: "msg_golden",
      output_index: 0,
      content_index: 0,
      delta: "ok",
    },
    { type: "response.completed", sequence_number: 2, response: openAiResponse(model, "completed") },
  ]);
}

const EMBEDDINGS = {
  object: "list",
  data: [{ object: "embedding", index: 0, embedding: [0.1, 0.2, 0.3] }],
  model: "text-embedding-3-small",
  usage: { prompt_tokens: 8, total_tokens: 8 },
};

const ANTHROPIC_INPUT_USAGE = {
  input_tokens: 37,
  cache_read_input_tokens: 9,
  cache_creation_input_tokens: 4,
};

function anthropicMessage(model: unknown): unknown {
  return {
    id: "msg_golden",
    type: "message",
    role: "assistant",
    model,
    content: [{ type: "text", text: "ok" }],
    stop_reason: "end_turn",
    stop_sequence: null,
    usage: {
      ...ANTHROPIC_INPUT_USAGE,
      cache_creation: { ephemeral_5m_input_tokens: 1, ephemeral_1h_input_tokens: 3 },
      output_tokens: 52,
      server_tool_use: { web_search_requests: 2 },
    },
  };
}

function anthropicStream(model: unknown): string {
  return typedSse([
    {
      type: "message_start",
      message: {
        id: "msg_golden",
        type: "message",
        role: "assistant",
        model,
        content: [],
        stop_reason: null,
        stop_sequence: null,
        usage: {
          ...ANTHROPIC_INPUT_USAGE,
          cache_creation: { ephemeral_5m_input_tokens: 1, ephemeral_1h_input_tokens: 3 },
          output_tokens: 1,
        },
      },
    },
    { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
    { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "ok" } },
    { type: "content_block_stop", index: 0 },
    {
      type: "message_delta",
      delta: { stop_reason: "end_turn", stop_sequence: null },
      usage: { ...ANTHROPIC_INPUT_USAGE, output_tokens: 52, server_tool_use: { web_search_requests: 2 } },
    },
    { type: "message_stop" },
  ]);
}

function gemini(cached: boolean): unknown {
  return {
    candidates: [{ content: { role: "model", parts: [{ text: "ok" }] }, finishReason: "STOP" }],
    usageMetadata: {
      promptTokenCount: 100,
      ...(cached ? { cachedContentTokenCount: 40 } : {}),
      candidatesTokenCount: 20,
      thoughtsTokenCount: 30,
      totalTokenCount: 150,
    },
    modelVersion: cached ? NATIVE["gemini-cached"].upstreamModel : NATIVE["gemini-chat"].upstreamModel,
  };
}

const CONVERSE = {
  output: { message: { role: "assistant", content: [{ text: "ok" }] } },
  stopReason: "end_turn",
  usage: {
    inputTokens: 37,
    outputTokens: 52,
    totalTokens: 102,
    cacheReadInputTokens: 9,
    cacheWriteInputTokens: 4,
    cacheDetails: [
      { ttl: "5m", inputTokens: 1 },
      { ttl: "1h", inputTokens: 3 },
    ],
  },
  metrics: { latencyMs: 1 },
};

async function startUpstream(): Promise<{ server: Server; baseUrl: string }> {
  const server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = decodeURIComponent((req.url ?? "/").split("?")[0]);
      const raw = Buffer.concat(chunks).toString("utf8");
      const body = (raw ? JSON.parse(raw) : {}) as { model?: unknown; stream?: boolean };
      const json = (payload: unknown) => {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify(payload));
      };
      const sse = (payload: string) => {
        res.writeHead(200, { "content-type": "text/event-stream" });
        res.end(payload);
      };
      if (path === "/v1/chat/completions") {
        return body.stream ? sse(openAiChatStream(body.model)) : json(openAiChat(body.model));
      }
      if (path === "/v1/responses") {
        return body.stream ? sse(openAiResponseStream(body.model)) : json(openAiResponse(body.model, "completed"));
      }
      if (path === "/v1/embeddings") return json(EMBEDDINGS);
      if (path === "/v1/messages/count_tokens") return json({ input_tokens: 42 });
      if (path === "/v1/messages") {
        return body.stream ? sse(anthropicStream(body.model)) : json(anthropicMessage(body.model));
      }
      if (path.endsWith(":generateContent")) {
        return json(gemini(path.includes(`/models/${NATIVE["gemini-cached"].upstreamModel}:`)));
      }
      if (path.endsWith("/converse")) return json(CONVERSE);
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: `no mock route for ${path}` } }));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const addr = server.address();
  if (addr === null || typeof addr === "string") throw new Error("the mock upstream has no port");
  return { server, baseUrl: `http://127.0.0.1:${addr.port}` };
}

describe("pricing goldens: the usage rows a real gateway sends, one per provider family", () => {
  let cp: MockControlPlane | undefined;
  let app: SpawnedApp | undefined;
  let upstream: Awaited<ReturnType<typeof startUpstream>> | undefined;
  let etcdReachable = false;

  const teamId = randomUUID();
  const modelIds = {} as Record<Native, string>;
  const providerKeyIds = {} as Record<Provider, string>;
  /** Each family's one row, as delivered. */
  const captured = new Map<string, Row>();

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startUpstream();
    cp = await startMockControlPlane();
    app = await spawnApp({ extra: { managed: cp.managed }, awaitProxyListener: false });
    const seed = new SeedClient(etcd, `${app.etcdPrefix}/${cp.envId}`);

    const keys: Record<Provider, Record<string, unknown>> = {
      openai: {
        provider: "openai",
        adapter: "openai",
        secret: "sk-golden-mock",
        api_base: `${upstream.baseUrl}/v1`,
      },
      anthropic: {
        provider: "anthropic",
        adapter: "anthropic",
        secret: "sk-ant-golden-mock",
        api_base: upstream.baseUrl,
      },
      google: {
        provider: "google",
        adapter: "vertex",
        secret: JSON.stringify({ access_token: "ya29.golden-mock", project: "proj-golden", region: "us-central1" }),
        api_base: upstream.baseUrl,
      },
      bedrock: {
        provider: "bedrock",
        adapter: "bedrock",
        secret: JSON.stringify({
          access_key_id: "AKIA-golden-mock",
          secret_access_key: "golden-mock-secret",
          region: "us-east-1",
        }),
        api_base: upstream.baseUrl,
      },
    };
    for (const [provider, key] of Object.entries(keys) as Array<[Provider, Record<string, unknown>]>) {
      providerKeyIds[provider] = (await seed.createProviderKey({ display_name: `golden-${provider}`, ...key })).id;
    }
    for (const [native, { provider, upstreamModel }] of Object.entries(NATIVE) as Array<
      [Native, (typeof NATIVE)[Native]]
    >) {
      const created = await seed.createModel({
        display_name: modelName(native),
        provider,
        model_name: upstreamModel,
        provider_key_id: providerKeyIds[provider],
        cost: COST,
      });
      modelIds[native] = created.id;
    }

    // An exact-match cache for the second caller only, so every other
    // family's row is a call the upstream served.
    const cacheCallerId = randomUUID();
    await seed.createCachePolicy({
      name: "golden-cache",
      enabled: true,
      applies_to: `api_key:${cacheCallerId}`,
    });

    // The caller keys come last: once both authenticate, every row above is
    // in the gateway's snapshot.
    const allowed_models = Object.keys(NATIVE).map((n) => modelName(n as Native));
    await seed.update("api_keys", randomUUID(), { key_hash: hash(CALLER), team_id: teamId, allowed_models });
    await seed.update("api_keys", cacheCallerId, { key_hash: hash(CACHE_CALLER), team_id: teamId, allowed_models });
    const authenticates = async (key: string) => {
      try {
        return (await new ProxyClient(app!.proxyUrl, key).listModels()).status === 200;
      } catch (err) {
        // A managed gateway binds its proxy listener once the first
        // configuration applies; until then the connection is refused.
        // Anything else is a real failure.
        const refused = err as { code?: string; cause?: { code?: string } };
        if ((refused.code ?? refused.cause?.code) === "ECONNREFUSED") return false;
        throw err;
      }
    };
    await waitConfigPropagation(async () => {
      return (await authenticates(CALLER)) && (await authenticates(CACHE_CALLER));
    });
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await cp?.close();
    await new Promise<void>((resolve) => (upstream ? upstream.server.close(() => resolve()) : resolve()));
  });

  const send = async (key: string, path: string, requestId: string, body: unknown) => {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
        "x-aisix-request-id": requestId,
      },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  };

  const rowsFor = (requestId: string): Row[] =>
    cp!.telemetryBodies
      .flatMap((body) => (JSON.parse(body) as { events: Row[] }).events)
      .filter((row) => row.request_id === requestId);

  const requestId = (family: Family) => `golden-${family.name}`;

  test("every family's request is answered and leaves exactly one row", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    for (const family of FAMILIES) {
      if (family.cached) {
        const warm = await send(CACHE_CALLER, family.path, `${requestId(family)}-warm`, family.body);
        expect(warm.status, `${family.name} (warm): ${warm.text}`).toBe(200);
      }
      const res = await send(family.cached ? CACHE_CALLER : CALLER, family.path, requestId(family), family.body);
      expect(res.status, `${family.name}: ${res.text}`).toBe(200);
    }
    // A later request's row follows every row before it, so once it is in,
    // a family's count can no longer grow.
    const barrier = await send(CALLER, "/v1/embeddings", "golden-barrier", {
      model: modelName("openai-embeddings"),
      input: "barrier",
    });
    expect(barrier.status, barrier.text).toBe(200);
    await waitConfigPropagation(async () => rowsFor("golden-barrier").length > 0, 30_000);

    for (const family of FAMILIES) {
      const rows = rowsFor(requestId(family));
      expect(rows, `${family.name}: ${JSON.stringify(rows)}`).toHaveLength(1);
      captured.set(family.name, rows[0]);
    }
  }, 90_000);

  const row = (family: string): Row => {
    const found = captured.get(family);
    if (found === undefined) throw new Error(`no row was captured for ${family}`);
    return found;
  };

  test("every row names its tenant, the model asked for and the upstream that served it", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    for (const family of FAMILIES) {
      const r = row(family.name);
      const what = `${family.name}: ${JSON.stringify(r)}`;
      expect(typeof r.event_id, what).toBe("string");
      expect((r.event_id as string).length, what).toBeGreaterThan(0);
      expect(r.requested_model_id, what).toBe(modelIds[family.native]);
      expect(r.team_id, what).toBe(teamId);
      expect(r.upstream_provider, what).toBe(NATIVE[family.native].provider);
      expect(r.upstream_model, what).toBe(NATIVE[family.native].upstreamModel);
      if (!family.cached) {
        expect(r.provider_key_id, what).toBe(providerKeyIds[NATIVE[family.native].provider]);
      }
    }
  });

  test("every row carries the usage its upstream reported", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    const expectUsage = (family: string, usage: Row) =>
      expect(row(family), `${family}: ${JSON.stringify(row(family))}`).toMatchObject(usage);

    expectUsage("openai-chat", {
      prompt_tokens: 1000,
      cached_prompt_tokens: 800,
      completion_tokens: 200,
      reasoning_tokens: 50,
      total_tokens: 1200,
      service_tier: "flex",
    });
    expectUsage("openai-responses", {
      prompt_tokens: 2000,
      cached_prompt_tokens: 1500,
      completion_tokens: 300,
      reasoning_tokens: 120,
    });
    expectUsage("anthropic-messages", {
      prompt_tokens: 37,
      cache_read_tokens: 9,
      cache_creation_tokens: 4,
      cache_creation_5m_tokens: 1,
      cache_creation_1h_tokens: 3,
      completion_tokens: 52,
      web_search_requests: 2,
    });
    const geminiUsage = { prompt_tokens: 100, completion_tokens: 20, reasoning_tokens: 30, total_tokens: 150 };
    expectUsage("gemini-chat", geminiUsage);
    expectUsage("gemini-cached", { ...geminiUsage, cached_prompt_tokens: 40 });
    expectUsage("bedrock-converse-chat", {
      prompt_tokens: 37,
      cache_read_tokens: 9,
      cache_creation_tokens: 4,
      cache_creation_5m_tokens: 1,
      cache_creation_1h_tokens: 3,
    });
    expectUsage("openai-embeddings", { prompt_tokens: 8 });
    // A count is a measurement of a prompt, not tokens an upstream consumed.
    expectUsage("anthropic-count-tokens", { prompt_tokens: 0, completion_tokens: 0 });
    expectUsage("cache-hit-messages", { cache_status: "hit" });
    expectUsage("cache-hit-chat", { cache_status: "hit" });
  });

  /** The row reduced to what the golden keeps, with stable names for its ids. */
  const normalized = (family: Family): Row => {
    const r = row(family.name);
    const stable = (field: string, names: Map<unknown, string>): string => {
      const name = names.get(r[field]);
      if (name === undefined) throw new Error(`${family.name}: ${field} ${String(r[field])} is not a seeded id`);
      return name;
    };
    const models = new Map<unknown, string>(
      (Object.keys(NATIVE) as Native[]).map((n) => [modelIds[n], `model-${n}`]),
    );
    const providerKeys = new Map<unknown, string>(
      (Object.keys(providerKeyIds) as Provider[]).map((p) => [providerKeyIds[p], `pk-${p}`]),
    );
    const out: Row = {};
    for (const key of KEPT_KEYS) if (key in r) out[key] = r[key];
    out.event_id = `evt-${family.name}-1`;
    out.occurred_at = GOLDEN_INSTANT;
    out.requested_model_id = stable("requested_model_id", models);
    out.team_id = stable("team_id", new Map([[teamId, "tenant-golden"]]));
    if ("provider_key_id" in out) out.provider_key_id = stable("provider_key_id", providerKeys);
    return out;
  };

  test("a stream, and another inbound protocol, record the same row as the native call", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    const byName = (name: string) => {
      const family = FAMILIES.find((f) => f.name === name);
      if (family === undefined) throw new Error(`no family ${name}`);
      // The row's own id and the operation the caller asked for differ.
      const { event_id: _id, operation: _operation, ...rest } = normalized(family);
      return rest;
    };
    for (const [other, native] of [
      ["openai-chat-stream", "openai-chat"],
      ["openai-responses-stream", "openai-responses"],
      ["anthropic-messages-stream", "anthropic-messages"],
      ["openai-via-messages", "openai-chat"],
      ["anthropic-via-chat", "anthropic-messages"],
    ]) {
      expect(byName(other), `${other} against ${native}`).toStrictEqual(byName(native));
    }
  });

  test(UPDATING ? "the wire goldens are rewritten" : "the rows match the wire goldens", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    if (UPDATING) mkdirSync(GOLDEN_DIR, { recursive: true });
    for (const family of FAMILIES) {
      const rows = [normalized(family)];
      const path = `${GOLDEN_DIR}${family.name}.json`;
      if (UPDATING) {
        writeFileSync(path, `${JSON.stringify(rows, null, 2)}\n`);
        continue;
      }
      if (!existsSync(path)) {
        throw new Error(`${path} is missing: capture it with UPDATE_PRICING_GOLDEN=1`);
      }
      expect(rows, `${family.name}: rewrite with UPDATE_PRICING_GOLDEN=1 if the change is intended`).toStrictEqual(
        JSON.parse(readFileSync(path, "utf8")),
      );
    }
  });
});
