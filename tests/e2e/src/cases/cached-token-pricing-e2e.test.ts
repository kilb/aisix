import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  metricDelta,
  scrapeMetrics,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MetricSample,
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
// rates, for both shapes.

const CALLER = "sk-cached-pricing-e2e";
const HASH = createHash("sha256").update(CALLER).digest("hex");

// Distinct enough that mixing two buckets up cannot land on the same total.
const IN_PER_1K = 1.0;
const OUT_PER_1K = 10.0;
const CACHED_PER_1K = 0.1;
const WRITE_PER_1K = 1.25;

const micro = (usd: number) => Math.round(usd * 1e6);

describe("prompt-cached tokens are priced at their own rate", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
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
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

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
    // Last, per `tests/e2e/AGENTS.md`.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: ["ctp-chat", "ctp-messages", "ctp-chat-nocacherate"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  const scrape = () => scrapeMetrics(app!.metricsUrl);

  async function spendDelta(
    before: MetricSample[],
    endpoint: string,
    model: string,
  ): Promise<number> {
    for (let i = 0; i < 40; i++) {
      const got = metricDelta(
        before,
        await scrape(),
        "aisix_llm_spend_micro_usd_total",
        { endpoint, model },
      );
      if (got !== 0) return got;
      await new Promise((r) => setTimeout(r, 50));
    }
    return 0;
  }

  function chat(model: string) {
    return fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
    });
  }
  function messages() {
    return fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER },
      body: JSON.stringify({
        model: "ctp-messages",
        max_tokens: 64,
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

    const before = await scrape();
    expect((await chat("ctp-chat")).status).toBe(200);
    const got = await spendDelta(before, "/v1/chat/completions", "ctp-chat");

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

    const before = await scrape();
    expect((await messages()).status).toBe(200);
    const got = await spendDelta(before, "/v1/messages", "ctp-messages");

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

  test("a cost block with no cache rates prices exactly as before", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await waitConfigPropagation(async () => (await chat("ctp-chat-nocacherate")).ok);

    const before = await scrape();
    expect((await chat("ctp-chat-nocacherate")).status).toBe(200);
    const got = await spendDelta(before, "/v1/chat/completions", "ctp-chat-nocacherate");

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
});
