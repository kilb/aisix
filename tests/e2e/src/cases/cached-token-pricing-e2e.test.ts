import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// Prompt-cached tokens are priced at their own rate, and the two upstream
// vocabularies disagree about what `prompt_tokens` contains:
//
//   Anthropic  cache_creation_input_tokens / cache_read_input_tokens are
//              counters SEPARATE from input_tokens.
//   OpenAI     prompt_tokens_details.cached_tokens is a SUBSET of
//              prompt_tokens.
//
// Priced with `input_per_1k` alone, cached traffic is mis-billed in BOTH
// directions: the separate counters are charged nothing at all, and the
// subset one is charged twice over at the full fresh rate. Neither mistake
// fails a request, and neither shows up in a test that only checks a token
// total — the only symptom is a spend figure that is quietly wrong, which is
// indistinguishable from a correct one unless something asserts the number.
//
// So this asserts the NUMBER, computed independently from the configured
// rates, for both shapes: the `cost_usd` on each request's usage row, as an
// exporter receives it.
//
// A cache write has a lifetime, and the longer one costs more: Anthropic
// prices a five-minute write at 1.25x input and a one-hour write at 2x
// (https://docs.claude.com/en/docs/build-with-claude/prompt-caching#pricing),
// and reports the split as `usage.cache_creation.ephemeral_5m_input_tokens` /
// `ephemeral_1h_input_tokens`, parts of `cache_creation_input_tokens`. A
// `cost` with a one-hour write rate charges the one-hour part at it, on
// every endpoint that reaches that upstream, streamed or not.

const CALLER = "sk-cached-pricing-e2e";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const CREDENTIAL_REF = "mock";
const LOGSTORE = "cached-token-pricing";

// Distinct enough that mixing two buckets up cannot land on the same total.
const IN_PER_1K = 1.0;
const OUT_PER_1K = 10.0;
const CACHED_PER_1K = 0.1;
const WRITE_PER_1K = 1.25;
const WRITE_1H_PER_1K = 2.0;

const micro = (usd: number) => Math.round(usd * 1e6);

/** What the split upstream reports: a 4000-token write, 1000 of it for one hour. */
const SPLIT_USAGE = {
  input_tokens: 1000,
  cache_read_input_tokens: 2000,
  cache_creation_input_tokens: 4000,
  cache_creation: { ephemeral_5m_input_tokens: 3000, ephemeral_1h_input_tokens: 1000 },
};
const SPLIT_OUTPUT = 100;

/**
 * An Anthropic Messages upstream that reports the write-lifetime split,
 * answering a streamed request with the documented event stream (the input
 * side on `message_start`, the output count on `message_delta`:
 * https://docs.anthropic.com/en/docs/build-with-claude/streaming) and any
 * other with the message object. Which one the gateway asked for depends on
 * the inbound endpoint, so it reads the request's own `stream` flag.
 */
async function startSplitUpstream(): Promise<{ server: Server; baseUrl: string }> {
  const message = (model: unknown, usage: Record<string, unknown>) => ({
    id: "msg_ctp_1h",
    type: "message",
    role: "assistant",
    model,
    content: [],
    stop_reason: null,
    stop_sequence: null,
    usage,
  });
  const server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      if (!(req.url ?? "").endsWith("/v1/messages")) {
        res.writeHead(404, { "content-type": "application/json" });
        res.end(JSON.stringify({ type: "error", error: { type: "not_found_error", message: req.url } }));
        return;
      }
      const body = JSON.parse(raw) as { model?: unknown; stream?: boolean };
      if (!body.stream) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            ...message(body.model, { ...SPLIT_USAGE, output_tokens: SPLIT_OUTPUT }),
            content: [{ type: "text", text: "ok" }],
            stop_reason: "end_turn",
          }),
        );
        return;
      }
      const events: Array<Record<string, unknown>> = [
        { type: "message_start", message: message(body.model, { ...SPLIT_USAGE, output_tokens: 1 }) },
        { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
        { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "ok" } },
        { type: "content_block_stop", index: 0 },
        {
          type: "message_delta",
          delta: { stop_reason: "end_turn", stop_sequence: null },
          usage: { output_tokens: SPLIT_OUTPUT },
        },
        { type: "message_stop" },
      ];
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.end(events.map((e) => `event: ${String(e.type)}\ndata: ${JSON.stringify(e)}\n\n`).join(""));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const addr = server.address();
  if (addr === null || typeof addr === "string") throw new Error("the split upstream has no port");
  return { server, baseUrl: `http://127.0.0.1:${addr.port}` };
}

describe("prompt-cached tokens are priced at their own rate", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let splitUpstream: Awaited<ReturnType<typeof startSplitUpstream>> | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      // OpenAI shape: cached_tokens is INSIDE prompt_tokens.
      nonStreamBody: {
        id: "ctp-chat",
        object: "chat.completion",
        created: 0,
        model: "gpt-4o-mini",
        choices: [
          { index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" },
        ],
        usage: {
          prompt_tokens: 1000,
          completion_tokens: 100,
          total_tokens: 1100,
          prompt_tokens_details: { cached_tokens: 800 },
        },
      },
      pathBodies: {
        // Anthropic shape: the cache counters sit BESIDE input_tokens.
        "/v1/messages": {
          id: "ctp-msg",
          type: "message",
          role: "assistant",
          model: "claude-3-5-haiku-20241022",
          content: [{ type: "text", text: "ok" }],
          stop_reason: "end_turn",
          usage: {
            input_tokens: 1000,
            output_tokens: 100,
            cache_creation_input_tokens: 4000,
            cache_read_input_tokens: 2000,
          },
        },
      },
    });
    // 同样的 OpenAI 缓存口径，流式：用量在最后一个 chunk 里。
    const chunk = (delta: unknown, extra: Record<string, unknown> = {}) =>
      JSON.stringify({
        id: "ctp-stream",
        object: "chat.completion.chunk",
        created: 0,
        model: "gpt-4o-mini",
        choices: [{ index: 0, delta, finish_reason: null }],
        ...extra,
      });
    streamUpstream = await startOpenAiUpstream({
      streamEvents: [
        chunk({ role: "assistant", content: "ok" }),
        JSON.stringify({
          id: "ctp-stream",
          object: "chat.completion.chunk",
          created: 0,
          model: "gpt-4o-mini",
          choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
          usage: {
            prompt_tokens: 1000,
            completion_tokens: 100,
            total_tokens: 1100,
            prompt_tokens_details: { cached_tokens: 800 },
          },
        }),
        "[DONE]",
      ],
    });
    splitUpstream = await startSplitUpstream();
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-cached-token-pricing",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });

    const openaiPk = await seed.createProviderKey({
      display_name: "ctp-openai-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    const anthropicPk = await seed.createProviderKey({
      display_name: "ctp-anthropic-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
      provider: "anthropic",
      adapter: "anthropic",
    });
    const cost = {
      input_per_1k: IN_PER_1K,
      output_per_1k: OUT_PER_1K,
      cached_input_per_1k: CACHED_PER_1K,
      cache_write_per_1k: WRITE_PER_1K,
    };
    await seed.createModel({
      display_name: "ctp-chat",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openaiPk.id,
      cost,
    });
    // `/v1/messages` 打到 OpenAI 兼容上游：跨协议翻译那条路。
    await seed.createModel({
      display_name: "ctp-messages-openai",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openaiPk.id,
      cost,
    });
    const streamPk = await seed.createProviderKey({
      display_name: "ctp-openai-stream-pk",
      secret: "sk-mock",
      api_base: `${streamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "ctp-messages-openai-stream",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: streamPk.id,
      cost,
    });
    await seed.createModel({
      display_name: "ctp-messages",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: anthropicPk.id,
      cost,
    });
    // Same traffic, no cache rates: pins that the fallback keeps charging
    // cached tokens at the plain input rate, so an existing `cost` block
    // reports exactly what it reported before these fields existed.
    await seed.createModel({
      display_name: "ctp-chat-nocacherate",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openaiPk.id,
      cost: { input_per_1k: IN_PER_1K, output_per_1k: OUT_PER_1K },
    });
    const splitPk = await seed.createProviderKey({
      display_name: "ctp-split-pk",
      secret: "sk-mock",
      api_base: splitUpstream.baseUrl,
      provider: "anthropic",
      adapter: "anthropic",
    });
    await seed.createModel({
      display_name: "ctp-1h",
      provider: "anthropic",
      model_name: "claude-sonnet-4-5-20250929",
      provider_key_id: splitPk.id,
      cost: { ...cost, cache_write_1h_per_1k: WRITE_1H_PER_1K },
    });
    // The same upstream, priced by a `cost` written before the one-hour rate
    // existed.
    await seed.createModel({
      display_name: "ctp-1h-norate",
      provider: "anthropic",
      model_name: "claude-sonnet-4-5-20250929",
      provider_key_id: splitPk.id,
      cost,
    });
    // Last, per `tests/e2e/AGENTS.md`.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: [
        "ctp-chat",
        "ctp-messages",
        "ctp-chat-nocacherate",
        "ctp-messages-openai",
        "ctp-messages-openai-stream",
        "ctp-1h",
        "ctp-1h-norate",
      ],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await streamUpstream?.close();
    await new Promise<void>((resolve) =>
      splitUpstream ? splitUpstream.server.close(() => resolve()) : resolve(),
    );
    await sls?.close();
  });

  /** What the usage row of the request `res` answered bills, in micro-USD. */
  async function billedFor(res: Response, model: string): Promise<number> {
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    expect(requestId, "the response names its request").not.toBe("");
    const row = await waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      `the usage row of ${requestId}`,
      20_000,
    );
    expect(row.get("requested_model")).toBe(model);
    return micro(Number(row.get("cost_usd") ?? "0"));
  }

  function chat(model: string, stream = false) {
    return fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model,
        messages: [{ role: "user", content: "hi" }],
        ...(stream ? { stream: true, stream_options: { include_usage: true } } : {}),
      }),
    });
  }
  function responses(model: string, stream = false) {
    return fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({ model, input: "hi", ...(stream ? { stream: true } : {}) }),
    });
  }
  function messages(model = "ctp-messages", stream = false) {
    return fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER },
      body: JSON.stringify({
        model,
        max_tokens: 64,
        stream,
        messages: [{ role: "user", content: "hi" }],
      }),
    });
  }

  test("a cached OpenAI prompt is not charged twice over", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await chat("ctp-chat")).ok);

    const res = await chat("ctp-chat");
    expect(res.status).toBe(200);
    const got = await billedFor(res, "ctp-chat");

    // 800 of the 1000 prompt tokens were cache hits, so only 200 are fresh.
    const want = micro(
      (200 * IN_PER_1K + 800 * CACHED_PER_1K + 100 * OUT_PER_1K) / 1000,
    );
    // What the pre-fix gateway reported: every prompt token at the fresh rate.
    const wrong = micro((1000 * IN_PER_1K + 100 * OUT_PER_1K) / 1000);
    expect(
      got,
      `cached prompt tokens must bill at the cached rate; ${wrong} would mean ` +
        `the whole prompt was charged as fresh input even though most of it ` +
        `was served from the provider's cache`,
    ).toBe(want);
    expect(got).not.toBe(wrong);
  }, 120_000);

  test("Anthropic cache counters are billed rather than ignored", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await messages()).ok);

    const res = await messages();
    expect(res.status).toBe(200);
    const got = await billedFor(res, "ctp-messages");

    // input_tokens is already exclusive of both cache counters, so all three
    // buckets are additive.
    const want = micro(
      (1000 * IN_PER_1K + 2000 * CACHED_PER_1K + 4000 * WRITE_PER_1K + 100 * OUT_PER_1K) /
        1000,
    );
    // What the pre-fix gateway reported: the cache counters priced at zero.
    const wrong = micro((1000 * IN_PER_1K + 100 * OUT_PER_1K) / 1000);
    expect(
      got,
      `cache creation and read tokens are real charges on the provider bill; ` +
        `${wrong} would mean 6000 billed tokens were priced at nothing`,
    ).toBe(want);
    expect(got).not.toBe(wrong);
  }, 120_000);

  // Anthropic 口径下 `input_tokens` 不含缓存命中，命中的部分单独报在
  // `cache_read_input_tokens`（https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching：
  // total input = cache_read + cache_creation + input_tokens）。OpenAI 的
  // `cached_tokens` 是 `prompt_tokens` 的子集，翻译过去必须拆开。
  const WANT_CROSS = () => micro((200 * IN_PER_1K + 800 * CACHED_PER_1K + 100 * OUT_PER_1K) / 1000);

  test("a cached OpenAI prompt reached through /v1/messages is reported and billed as cached", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await messages("ctp-messages-openai")).ok);

    const res = await messages("ctp-messages-openai");
    expect(res.status).toBe(200);
    const body = (await res.json()) as { usage?: Record<string, number> };
    expect(body.usage?.input_tokens, "only the fresh part of the prompt").toBe(200);
    expect(body.usage?.cache_read_input_tokens, "the cache hits, reported apart").toBe(800);
    expect(body.usage?.output_tokens).toBe(100);

    const got = await billedFor(res, "ctp-messages-openai");
    expect(
      got,
      "800 cached prompt tokens must bill at the cached rate, not as fresh input",
    ).toBe(WANT_CROSS());
  }, 120_000);

  test("the same through a streamed /v1/messages", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => {
      const r = await messages("ctp-messages-openai-stream", true);
      await r.text();
      return r.ok;
    });

    const res = await messages("ctp-messages-openai-stream", true);
    expect(res.status).toBe(200);
    const sse = await res.text();
    const delta = sse
      .split("\n\n")
      .find((frame) => frame.includes("event: message_delta"));
    expect(delta, `no message_delta frame in:\n${sse}`).toBeDefined();
    const data = JSON.parse(
      delta!
        .split("\n")
        .find((l) => l.startsWith("data:"))!
        .slice("data:".length),
    ) as { usage?: Record<string, number> };
    expect(data.usage?.input_tokens, "only the fresh part of the prompt").toBe(200);
    expect(data.usage?.cache_read_input_tokens, "the cache hits, reported apart").toBe(800);

    const got = await billedFor(res, "ctp-messages-openai-stream");
    expect(got).toBe(WANT_CROSS());
  }, 120_000);

  test("a cost block with no cache rates prices exactly as before", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await chat("ctp-chat-nocacherate")).ok);

    const res = await chat("ctp-chat-nocacherate");
    expect(res.status).toBe(200);
    const got = await billedFor(res, "ctp-chat-nocacherate");

    // Every prompt token at the input rate, cached or not — the behaviour
    // that existed before the cache rates did. Upgrading must not move an
    // existing deployment's reported spend.
    const want = micro((1000 * IN_PER_1K + 100 * OUT_PER_1K) / 1000);
    expect(
      got,
      "an operator who has not set cache rates must see the same number the " +
        "previous build reported, not a silently changed one",
    ).toBe(want);
  }, 120_000);

  // 1000 fresh + 2000 read + 3000 written for five minutes + 1000 written for
  // one hour, and 100 out.
  const WANT_1H = micro(
    (1000 * IN_PER_1K +
      2000 * CACHED_PER_1K +
      3000 * WRITE_PER_1K +
      1000 * WRITE_1H_PER_1K +
      SPLIT_OUTPUT * OUT_PER_1K) /
      1000,
  );
  // Every write at the five-minute rate: what a `cost` without the one-hour
  // rate charges, and what the gateway charged before it had one.
  const AT_THE_WRITE_RATE = micro(
    (1000 * IN_PER_1K + 2000 * CACHED_PER_1K + 4000 * WRITE_PER_1K + SPLIT_OUTPUT * OUT_PER_1K) /
      1000,
  );

  /** Every model and key is seeded before the caller key, so its first 200 implies them all. */
  const callerAuthenticates = () =>
    waitConfigPropagation(async () => {
      return (await new ProxyClient(app!.proxyUrl, CALLER).listModels()).status === 200;
    });

  const ENDPOINTS = [
    { endpoint: "/v1/messages", send: messages },
    { endpoint: "/v1/chat/completions", send: chat },
    { endpoint: "/v1/responses", send: responses },
  ] as const;

  for (const { endpoint, send } of ENDPOINTS) {
    for (const stream of [false, true]) {
      test(`a one-hour cache write is charged at its own rate: ${endpoint}${stream ? ", streamed" : ""}`, async (ctx) => {
        if (!etcdReachable || !app) {
          ctx.skip();
          return;
        }
        await callerAuthenticates();

        const res = await send("ctp-1h", stream);
        const text = await res.text();
        expect(res.status, text).toBe(200);
        const got = await billedFor(res, "ctp-1h");
        expect(
          got,
          `1000 of the 4000 written tokens were written for one hour and bill at ` +
            `${WRITE_1H_PER_1K}/1k; ${AT_THE_WRITE_RATE} would mean they were charged ` +
            `at the five-minute write rate`,
        ).toBe(WANT_1H);
      }, 120_000);
    }
  }

  test("a cost block with no one-hour rate charges a one-hour write at the write rate", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await callerAuthenticates();

    const res = await messages("ctp-1h-norate");
    const text = await res.text();
    expect(res.status, text).toBe(200);
    const got = await billedFor(res, "ctp-1h-norate");
    expect(
      got,
      "a `cost` written before the one-hour rate existed must report what it reported before",
    ).toBe(AT_THE_WRITE_RATE);
  }, 120_000);
});
