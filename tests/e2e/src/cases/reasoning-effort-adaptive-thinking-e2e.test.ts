import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  eventStream,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a caller's reasoning effort on a Claude model that takes adaptive
// thinking also asks for the thinking, on every route to Claude, and what comes
// back is the caller's and is on the bill.
//
// Per https://platform.claude.com/docs/en/build-with-claude/thinking#configuring-thinking
// Claude 4.6 and later take `adaptive`, Claude 4.5 and earlier answer it with a
// 400, Claude 4.6-4.8 do not think without it, and Claude 5 and later hide their
// thinking unless `display` says otherwise. Thinking tokens are billed as output
// tokens, and `output_tokens` is the inclusive total
// (https://platform.claude.com/docs/en/build-with-claude/thinking-steering-and-cost#pricing).
// Before API version 2026-10-21 `temperature` and `top_k` are incompatible with
// thinking on Claude 4.6 (same page as the table, "Sampling parameters").

const CALLER = "sk-reasoning-effort-adaptive";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const CREDENTIAL_REF = "e2e";
const LOGSTORE = "reasoning-effort-adaptive-store";
const ADAPTIVE = { type: "adaptive", display: "summarized" };

const THOUGHT = "Weighing the capital of France.";
const ANSWER = "Paris.";
const PROMPT_TOKENS = 10;
const OUTPUT_TOKENS = 40;
const THINKING_TOKENS = 30;
const INPUT_PER_1K = 1;
const OUTPUT_PER_1K = 2;
// Thinking is inside the output count, so it is not paid for a second time.
const COST = (PROMPT_TOKENS * INPUT_PER_1K + OUTPUT_TOKENS * OUTPUT_PER_1K) / 1000;

const USAGE = {
  input_tokens: PROMPT_TOKENS,
  output_tokens: OUTPUT_TOKENS,
  output_tokens_details: { thinking_tokens: THINKING_TOKENS },
};

const CLAUDE_MESSAGE = {
  id: "msg_1",
  type: "message",
  role: "assistant",
  model: "claude",
  content: [
    { type: "thinking", thinking: THOUGHT, signature: "sig" },
    { type: "text", text: ANSWER },
  ],
  stop_reason: "end_turn",
  usage: USAGE,
};

const claudeEvent = (type: string, rest: Record<string, unknown>) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...rest })}\n\n`;

const CLAUDE_STREAM = [
  claudeEvent("message_start", {
    message: { ...CLAUDE_MESSAGE, content: [], stop_reason: null, usage: { input_tokens: PROMPT_TOKENS, output_tokens: 1 } },
  }),
  claudeEvent("content_block_start", { index: 0, content_block: { type: "thinking", thinking: "", signature: "" } }),
  claudeEvent("content_block_delta", { index: 0, delta: { type: "thinking_delta", thinking: THOUGHT } }),
  claudeEvent("content_block_delta", { index: 0, delta: { type: "signature_delta", signature: "sig" } }),
  claudeEvent("content_block_stop", { index: 0 }),
  claudeEvent("content_block_start", { index: 1, content_block: { type: "text", text: "" } }),
  claudeEvent("content_block_delta", { index: 1, delta: { type: "text_delta", text: ANSWER } }),
  claudeEvent("content_block_stop", { index: 1 }),
  claudeEvent("message_delta", { delta: { stop_reason: "end_turn" }, usage: USAGE }),
  claudeEvent("message_stop", {}),
].join("");

// Converse's usage has no thinking breakdown:
// https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_TokenUsage.html
const CONVERSE_STREAM = eventStream([
  ["messageStart", { role: "assistant" }],
  ["contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { text: THOUGHT } } }],
  ["contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { signature: "sig" } } }],
  ["contentBlockStop", { contentBlockIndex: 0 }],
  ["contentBlockDelta", { contentBlockIndex: 1, delta: { text: ANSWER } }],
  ["contentBlockStop", { contentBlockIndex: 1 }],
  ["messageStop", { stopReason: "end_turn" }],
  [
    "metadata",
    {
      usage: { inputTokens: PROMPT_TOKENS, outputTokens: OUTPUT_TOKENS, totalTokens: PROMPT_TOKENS + OUTPUT_TOKENS },
      metrics: { latencyMs: 1 },
    },
  ],
]);

const ANTHROPIC_PK = { provider: "anthropic", adapter: "anthropic", secret: "k" };
const VERTEX_PK = {
  provider: "google",
  adapter: "vertex",
  secret: JSON.stringify({ access_token: "t", project: "p", region: "us-east5" }),
};
const BEDROCK_PK = {
  provider: "bedrock",
  adapter: "bedrock",
  secret: JSON.stringify({ access_key_id: "AKIA", secret_access_key: "s", region: "us-west-2" }),
};

const MODELS: Record<string, { pk: Record<string, string>; provider: string; model_name: string }> = {
  "adaptive-anthropic": { pk: ANTHROPIC_PK, provider: "anthropic", model_name: "claude-opus-4-6" },
  "adaptive-vertex": { pk: VERTEX_PK, provider: "google", model_name: "claude-opus-4-6" },
  "adaptive-bedrock": { pk: BEDROCK_PK, provider: "bedrock", model_name: "us.anthropic.claude-opus-4-6-v1:0" },
  "adaptive-opus-55": { pk: ANTHROPIC_PK, provider: "anthropic", model_name: "claude-opus-5-5" },
  "adaptive-sonnet-46": { pk: ANTHROPIC_PK, provider: "anthropic", model_name: "claude-sonnet-4-6" },
  "older-opus-45": { pk: ANTHROPIC_PK, provider: "anthropic", model_name: "claude-opus-4-5" },
  "older-sonnet-45": { pk: ANTHROPIC_PK, provider: "anthropic", model_name: "claude-sonnet-4-5" },
  "not-claude": { pk: ANTHROPIC_PK, provider: "anthropic", model_name: "glm-5" },
};

describe("a caller's reasoning effort on a Claude model that takes adaptive thinking", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let sls: MockSls | undefined;
  const sent: { path: string; body: Record<string, any> }[] = [];
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
        const body = JSON.parse(Buffer.concat(parts).toString("utf8") || "{}");
        sent.push({ path, body });
        if (path.includes("/converse-stream")) {
          res.writeHead(200, { "content-type": "application/vnd.amazon.eventstream" });
          res.end(CONVERSE_STREAM);
        } else if (body.stream || path.includes(":streamRawPredict")) {
          res.writeHead(200, { "content-type": "text/event-stream" });
          res.end(CLAUDE_STREAM);
        } else {
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify(CLAUDE_MESSAGE));
        }
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const addr = upstream.address();
    if (addr === null || typeof addr === "string") throw new Error("no port");

    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-ak-id",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-ak-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "reasoning-effort-adaptive-sls",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "reasoning-effort-adaptive-proj",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
    });
    for (const [display_name, model] of Object.entries(MODELS)) {
      const pk = await seed.createProviderKey({
        display_name: `${display_name}-pk`,
        api_base: `http://127.0.0.1:${addr.port}`,
        ...model.pk,
      } as Parameters<typeof seed.createProviderKey>[0]);
      await seed.createModel({
        display_name,
        provider: model.provider,
        model_name: model.model_name,
        provider_key_id: pk.id,
        cost: { input_per_1k: INPUT_PER_1K, output_per_1k: OUTPUT_PER_1K },
      });
    }
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: Object.keys(MODELS) });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await new Promise<void>((resolve) => (upstream ? upstream.close(() => resolve()) : resolve()));
  });

  async function send(route: string, body: unknown) {
    const res = await fetch(`${app!.proxyUrl}${route}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, raw: await res.text(), requestId: res.headers.get("x-aisix-request-id") ?? "" };
  }

  const row = (requestId: string) =>
    waitForSlsLog(sls!, LOGSTORE, (l) => l.get("request_id") === requestId, `row for ${requestId}`, 15_000);

  /** What Claude was asked for, on the wire of whichever route carried it. */
  function claudeRequest(body: Record<string, any>) {
    return body.additionalModelRequestFields ?? body;
  }

  const chat = (model: string, extra: Record<string, unknown> = {}) => ({
    model,
    messages: [{ role: "user", content: "What is the capital of France?" }],
    reasoning_effort: "high",
    ...extra,
  });

  const FAMILIES = [
    {
      name: "chat",
      route: "/v1/chat/completions",
      body: (model: string, stream: boolean) => chat(model, stream ? { stream, stream_options: { include_usage: true } } : {}),
    },
    {
      name: "responses",
      route: "/v1/responses",
      body: (model: string, stream: boolean) => ({
        model,
        input: "What is the capital of France?",
        reasoning: { effort: "high" },
        ...(stream ? { stream } : {}),
      }),
    },
  ];

  for (const model of ["adaptive-anthropic", "adaptive-vertex", "adaptive-bedrock"]) {
    for (const family of FAMILIES) {
      for (const stream of [false, true]) {
        test(`${model} ${family.name}${stream ? " stream" : ""}: thinking is asked for, returned and billed`, async (ctx) => {
          if (!etcdReachable || !app) return ctx.skip();
          const out = await send(family.route, family.body(model, stream));
          expect(out.status, out.raw).toBe(200);

          const wire = claudeRequest(sent.at(-1)!.body);
          expect(wire.thinking).toEqual(ADAPTIVE);
          expect(wire.output_config).toEqual({ effort: "high" });
          expect(wire.reasoning_effort).toBeUndefined();

          expect(out.raw).toContain(THOUGHT);
          expect(out.raw).toContain(ANSWER);

          const log = await row(out.requestId);
          expect(log.get("prompt_tokens")).toBe(String(PROMPT_TOKENS));
          expect(log.get("completion_tokens")).toBe(String(OUTPUT_TOKENS));
          expect(Number(log.get("cost_usd"))).toBeCloseTo(COST, 9);
          // A streamed Bedrock Claude answer comes over Converse, whose usage
          // has no thinking breakdown; its output count still holds the tokens.
          if (model === "adaptive-bedrock" && stream) {
            expect(Number(log.get("reasoning_tokens") ?? "0")).toBe(0);
          } else {
            expect(log.get("reasoning_tokens")).toBe(String(THINKING_TOKENS));
          }
        });
      }
    }
  }

  test("the chat caller's usage says how much of the output was reasoning", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await send("/v1/chat/completions", chat("adaptive-anthropic"));
    const body = JSON.parse(out.raw);
    expect(body.choices[0].message.reasoning_content).toBe(THOUGHT);
    expect(body.usage.completion_tokens).toBe(OUTPUT_TOKENS);
    expect(body.usage.completion_tokens_details.reasoning_tokens).toBe(THINKING_TOKENS);
  });

  test("Claude 5, where thinking is on but hidden, is asked to show it", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await send("/v1/chat/completions", chat("adaptive-opus-55"));
    expect(out.status, out.raw).toBe(200);
    expect(sent.at(-1)!.body.thinking).toEqual(ADAPTIVE);
  });

  test("a model that does not take adaptive thinking is not sent it", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const model of ["older-opus-45", "not-claude"]) {
      const out = await send("/v1/chat/completions", chat(model));
      expect(out.status, out.raw).toBe(200);
      expect(sent.at(-1)!.body.thinking, model).toBeUndefined();
      expect(sent.at(-1)!.body.output_config, model).toEqual({ effort: "high" });
    }
    const out = await send("/v1/chat/completions", chat("older-sonnet-45"));
    expect(out.status, out.raw).toBe(200);
    expect(sent.at(-1)!.body.thinking?.type).toBe("enabled");
  });

  test("none stays a request for no thinking", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const out = await send("/v1/chat/completions", chat("adaptive-anthropic", { reasoning_effort: "none" }));
    expect(out.status, out.raw).toBe(200);
    expect(sent.at(-1)!.body.thinking).toEqual({ type: "disabled" });
    expect(sent.at(-1)!.body.output_config).toBeUndefined();
  });

  test("a thinking the caller sent stands", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const own = { type: "adaptive", display: "omitted" };
    const out = await send("/v1/chat/completions", chat("adaptive-anthropic", { thinking: own }));
    expect(out.status, out.raw).toBe(200);
    expect(sent.at(-1)!.body.thinking).toEqual(own);
    expect(sent.at(-1)!.body.output_config).toEqual({ effort: "high" });
  });

  test("on Claude 4.6 a request with sampling thinking excludes keeps working as an effort", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const model of ["adaptive-anthropic", "adaptive-sonnet-46"]) {
      const out = await send("/v1/chat/completions", chat(model, { temperature: 0.2 }));
      expect(out.status, out.raw).toBe(200);
      expect(sent.at(-1)!.body.thinking, model).toBeUndefined();
      expect(sent.at(-1)!.body.temperature, model).toBe(0.2);
      expect(sent.at(-1)!.body.output_config, model).toEqual({ effort: "high" });
    }
  });
});
