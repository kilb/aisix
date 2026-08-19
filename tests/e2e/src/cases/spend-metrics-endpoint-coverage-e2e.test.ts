import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  metricDelta,
  scrapeMetrics,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MetricSample,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// `aisix_llm_spend_micro_usd_total` is the one series an operator reads to
// answer "what did this key cost me", and it had no e2e at all: the series
// is skipped whenever spend is zero, and no spec ever configured a price,
// so a scrape with the family entirely absent was indistinguishable from a
// scrape of un-priced traffic. Every endpoint could report nothing forever
// and every test would stay green.
//
// What the family owes a caller, per endpoint:
//
//   spend = input_tokens/1000 * input_per_1k + output_tokens/1000 * output_per_1k
//
// and nothing at all when the operator set no price on the row — the price
// is opt-in, so a deployment whose billing lives upstream of the gateway
// must not see a locally-invented number appear next to it.
//
// The prices below are deliberately different per model, so a handler that
// priced against the wrong row (the caller-addressed alias, a sibling
// target, a hardcoded table) lands on a wrong number rather than passing by
// coincidence.

const CALLER_PLAINTEXT = "sk-spend-metrics-caller";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

const CHAT_MODEL = "spend-chat";
const STREAM_MODEL = "spend-chat-stream";
const MESSAGES_MODEL = "spend-messages";
const RESP_MODEL = "spend-resp";
const EMBED_MODEL = "spend-embed";
const UNPRICED_MODEL = "spend-unpriced";

/** Upstream-reported usage on the non-streaming chat body. */
const CHAT_IN = 1_000;
const CHAT_OUT = 500;
/** …on the streaming chat body. */
const STREAM_IN = 17;
const STREAM_OUT = 23;
/** …on the /v1/responses body. */
const RESP_IN = 11;
const RESP_OUT = 13;
/** …on the /v1/embeddings body. */
const EMBED_IN = 7;

/** USD per 1,000 tokens, one distinct pair per row. */
const PRICES = {
  [CHAT_MODEL]: { input_per_1k: 3, output_per_1k: 15 },
  [STREAM_MODEL]: { input_per_1k: 2, output_per_1k: 4 },
  [MESSAGES_MODEL]: { input_per_1k: 2, output_per_1k: 10 },
  [RESP_MODEL]: { input_per_1k: 1, output_per_1k: 2 },
  [EMBED_MODEL]: { input_per_1k: 0.5, output_per_1k: 9 },
} as const;

/** The contract, in the metric's own unit. */
function expectedMicroUsd(
  model: keyof typeof PRICES,
  inputTokens: number,
  outputTokens: number,
): number {
  const p = PRICES[model];
  const usd =
    (inputTokens / 1000) * p.input_per_1k + (outputTokens / 1000) * p.output_per_1k;
  return Math.round(usd * 1_000_000);
}

describe("spend metric endpoint coverage e2e", () => {
  let app: SpawnedApp | undefined;
  let chatUpstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let respUpstream: OpenAiUpstream | undefined;
  let embedUpstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    chatUpstream = await startOpenAiUpstream({ nonStreamBody: chatBody() });
    streamUpstream = await startOpenAiUpstream({ streamEvents: streamEvents() });
    respUpstream = await startOpenAiUpstream({ nonStreamBody: responsesBody() });
    embedUpstream = await startOpenAiUpstream({ nonStreamBody: embeddingBody() });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    // `/v1/messages` and the un-priced control share the chat upstream:
    // same bytes back, different row, so any difference in the recorded
    // spend is the row's price and nothing else.
    const rows: Array<[string, OpenAiUpstream, Record<string, unknown> | undefined]> = [
      [CHAT_MODEL, chatUpstream, PRICES[CHAT_MODEL]],
      [STREAM_MODEL, streamUpstream, PRICES[STREAM_MODEL]],
      [MESSAGES_MODEL, chatUpstream, PRICES[MESSAGES_MODEL]],
      [RESP_MODEL, respUpstream, PRICES[RESP_MODEL]],
      [EMBED_MODEL, embedUpstream, PRICES[EMBED_MODEL]],
      [UNPRICED_MODEL, chatUpstream, undefined],
    ];
    for (const [name, up, cost] of rows) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: `${up.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
        ...(cost ? { cost } : {}),
      });
    }
    // Last, so a gate on this key authenticating implies every row above.
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: rows.map(([name]) => name),
    });
  });

  afterAll(async () => {
    await app?.exit();
    await chatUpstream?.close();
    await streamUpstream?.close();
    await respUpstream?.close();
    await embedUpstream?.close();
  });

  test("every usage-bearing endpoint reports what its row prices", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => {
      const r = await post("/v1/chat/completions", {
        model: CHAT_MODEL,
        messages: [{ role: "user", content: "ready" }],
      });
      return r.status === 200;
    });

    const cases: Array<{
      endpoint: string;
      model: keyof typeof PRICES;
      expected: number;
      drive: () => Promise<{ status: number }>;
    }> = [
      {
        endpoint: "/v1/chat/completions",
        model: CHAT_MODEL,
        expected: expectedMicroUsd(CHAT_MODEL, CHAT_IN, CHAT_OUT),
        drive: () =>
          post("/v1/chat/completions", {
            model: CHAT_MODEL,
            messages: [{ role: "user", content: "hi" }],
          }),
      },
      {
        endpoint: "/v1/chat/completions",
        model: STREAM_MODEL,
        expected: expectedMicroUsd(STREAM_MODEL, STREAM_IN, STREAM_OUT),
        drive: () =>
          post("/v1/chat/completions", {
            model: STREAM_MODEL,
            messages: [{ role: "user", content: "hi" }],
            stream: true,
          }),
      },
      {
        endpoint: "/v1/messages",
        model: MESSAGES_MODEL,
        expected: expectedMicroUsd(MESSAGES_MODEL, CHAT_IN, CHAT_OUT),
        drive: () =>
          post("/v1/messages", {
            model: MESSAGES_MODEL,
            max_tokens: 64,
            messages: [{ role: "user", content: "hi" }],
          }),
      },
      {
        endpoint: "/v1/responses",
        model: RESP_MODEL,
        expected: expectedMicroUsd(RESP_MODEL, RESP_IN, RESP_OUT),
        drive: () => post("/v1/responses", { model: RESP_MODEL, input: "hi" }),
      },
      {
        endpoint: "/v1/embeddings",
        model: EMBED_MODEL,
        expected: expectedMicroUsd(EMBED_MODEL, EMBED_IN, 0),
        drive: () => post("/v1/embeddings", { model: EMBED_MODEL, input: "hi" }),
      },
    ];

    const missing: string[] = [];
    const wrong: string[] = [];
    for (const c of cases) {
      const before = await scrape(app);
      expect((await c.drive()).status, `${c.endpoint} (${c.model})`).toBe(200);
      // Streaming spend is recorded when the stream terminates, which is
      // after the response body is drained but not necessarily before the
      // next scrape; poll rather than race it.
      let got = 0;
      for (let i = 0; i < 40; i++) {
        got = metricDelta(before, await scrape(app), "aisix_llm_spend_micro_usd_total", {
          endpoint: c.endpoint,
          model: c.model,
        });
        if (got !== 0) break;
        await new Promise((r) => setTimeout(r, 50));
      }
      if (got === 0) missing.push(`${c.endpoint} (${c.model})`);
      else if (got !== c.expected) {
        wrong.push(`${c.endpoint} (${c.model}): got ${got}, want ${c.expected}`);
      }
    }

    expect(
      missing,
      `these endpoints billed tokens but reported no spend — the series is ` +
        `absent, which reads exactly like "no traffic yet":\n` +
        missing.map((m) => `  ${m}`).join("\n"),
    ).toEqual([]);
    expect(
      wrong,
      `these endpoints priced against something other than the dispatched ` +
        `row:\n` + wrong.map((m) => `  ${m}`).join("\n"),
    ).toEqual([]);
  }, 120_000);

  test("a row with no price reports no spend at all", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const before = await scrape(app);
    expect(
      (
        await post("/v1/chat/completions", {
          model: UNPRICED_MODEL,
          messages: [{ role: "user", content: "hi" }],
        })
      ).status,
    ).toBe(200);
    const after = await scrape(app);

    // Tokens still flow — this is a live, billed request, not a no-op.
    expect(
      metricDelta(before, after, "aisix_llm_input_tokens_total", {
        model: UNPRICED_MODEL,
      }),
    ).toBe(CHAT_IN);
    // …but the gateway invents no price for it.
    expect(
      metricDelta(before, after, "aisix_llm_spend_micro_usd_total", {
        model: UNPRICED_MODEL,
      }),
      "an un-priced row must not produce a spend figure",
    ).toBe(0);
  }, 30_000);

  async function post(path: string, body: unknown): Promise<{ status: number }> {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "x-api-key": CALLER_PLAINTEXT,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    // Drain: a streamed body must reach its terminal event before the
    // stream-completion telemetry runs.
    await res.text();
    return { status: res.status };
  }
});

async function scrape(app: SpawnedApp | undefined): Promise<MetricSample[]> {
  return scrapeMetrics(app!.metricsUrl);
}

function chatBody() {
  return {
    id: "chatcmpl-spend",
    object: "chat.completion",
    created: 1765000000,
    model: "gpt-4o-mini",
    choices: [
      {
        index: 0,
        message: { role: "assistant", content: "hello" },
        finish_reason: "stop",
      },
    ],
    usage: {
      prompt_tokens: CHAT_IN,
      completion_tokens: CHAT_OUT,
      total_tokens: CHAT_IN + CHAT_OUT,
    },
  };
}

function streamEvents() {
  const chunk = (json: Record<string, unknown>) =>
    JSON.stringify({
      id: "chatcmpl-spend-stream",
      object: "chat.completion.chunk",
      created: 1765000000,
      model: "gpt-4o-mini",
      ...json,
    });
  return [
    chunk({ choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] }),
    chunk({ choices: [{ index: 0, delta: { content: "hello" }, finish_reason: null }] }),
    chunk({ choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }),
    chunk({
      choices: [],
      usage: {
        prompt_tokens: STREAM_IN,
        completion_tokens: STREAM_OUT,
        total_tokens: STREAM_IN + STREAM_OUT,
      },
    }),
    "[DONE]",
  ];
}

function responsesBody() {
  return {
    id: "resp_spend",
    object: "response",
    created_at: 0,
    status: "completed",
    model: "gpt-4o-mini",
    output: [
      {
        id: "msg_spend",
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: "hello" }],
      },
    ],
    usage: {
      input_tokens: RESP_IN,
      output_tokens: RESP_OUT,
      total_tokens: RESP_IN + RESP_OUT,
    },
  };
}

function embeddingBody() {
  return {
    object: "list",
    model: "gpt-4o-mini",
    data: [{ object: "embedding", index: 0, embedding: [0.1, 0.2, 0.3] }],
    usage: { prompt_tokens: EMBED_IN, total_tokens: EMBED_IN },
  };
}
