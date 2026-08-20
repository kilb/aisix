import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  scrapeMetrics,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// `aisix_cache_requests_total` had no E2E at all, and two defects lived in
// that gap — the exact shape `CLAUDE.md` names: an emit function nothing
// scrapes is invisible to every check, because a series that never appears
// is indistinguishable from "no traffic yet".
//
// 1. The `outcome` label had two vocabularies on one series. Chat emitted
//    the documented `hit_exact` / `hit_semantic`; the byte-bodied endpoints
//    fed the metric the control plane wire value instead and emitted `hit`, which
//    is not in the series' value set. Any dashboard written against the
//    documentation (`outcome=~"hit_.*"`) covered chat and silently showed
//    nothing for Anthropic-SDK and Codex traffic.
//
// 2. A hit the output guardrail BLOCKED incremented nothing at all — not
//    hit, not miss. The series documents one increment per request that
//    reached an enabled policy with an available backend, and a blocked hit
//    reached one, so cache events stopped summing to gated requests.
//
// Deltas, not absolutes, per `harness/metrics.ts`: the app serves other
// traffic in this spec.

const CALLER = "sk-cache-metric-labels";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const POLICY = "cml-policy";

/** Documented value set for `aisix_cache_requests_total{outcome}`. */
const DOCUMENTED = ["hit_exact", "hit_semantic", "miss", "bypass"];

describe("cache gate outcomes land on the documented labels", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cml-chat",
        object: "chat.completion",
        created: 0,
        model: "gpt-4o-mini",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "some confidential answer" },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 },
      },
      pathBodies: {
        "/v1/messages": {
          id: "cml-msg",
          type: "message",
          role: "assistant",
          model: "claude-3-5-haiku-20241022",
          content: [{ type: "text", text: "some confidential answer" }],
          stop_reason: "end_turn",
          usage: { input_tokens: 3, output_tokens: 2 },
        },
        "/v1/responses": {
          id: "cml-resp",
          object: "response",
          created_at: 0,
          status: "completed",
          model: "gpt-4o-mini",
          output: [
            {
              id: "cml-msg-2",
              type: "message",
              role: "assistant",
              content: [{ type: "output_text", text: "some confidential answer" }],
            },
          ],
          usage: { input_tokens: 3, output_tokens: 2, total_tokens: 5 },
        },
      },
    });
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    const anthropicPk = await seed.createProviderKey({
      display_name: "cml-anthropic-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
      provider: "anthropic",
      adapter: "anthropic",
    });
    const openaiPk = await seed.createProviderKey({
      display_name: "cml-openai-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "cml-chat",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openaiPk.id,
    });
    await seed.createModel({
      display_name: "cml-messages",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: anthropicPk.id,
    });
    await seed.createModel({
      display_name: "cml-responses",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openaiPk.id,
    });
    await seed.createCachePolicy({ name: POLICY, enabled: true, applies_to: "all" });
    // Last, per `tests/e2e/AGENTS.md`.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: ["cml-chat", "cml-messages", "cml-responses"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("every endpoint reports a hit under the same documented label", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => (await chat("ready")).ok);
    await waitConfigPropagation(async () => (await messages("ready")).ok);
    await waitConfigPropagation(async () => (await responses("ready")).ok);

    const before = await outcomes();
    // One miss then one hit on each endpoint of the family.
    for (const call of [chat, messages, responses]) {
      const prompt = `cml-${call.name}`;
      expect((await call(prompt)).status, `${call.name} miss`).toBe(200);
      expect((await call(prompt)).status, `${call.name} hit`).toBe(200);
    }
    const delta = diff(before, await outcomes());

    expect(
      Object.keys(delta).sort(),
      "one series, one vocabulary: an endpoint that invents its own `outcome` " +
        "value is invisible to every dashboard written against the documented set",
    ).toEqual(["hit_exact", "miss"]);
    // Three endpoints, one miss and one hit each.
    expect(delta.miss, "each endpoint's first call is a miss").toBe(3);
    expect(delta.hit_exact, "each endpoint's second call is a hit").toBe(3);
  }, 180_000);

  test("a hit the output guardrail blocks is still counted", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    // Populate all three caches while no output guardrail exists.
    const prompt = "cml-blocked-hit";
    for (const call of [chat, messages, responses]) {
      expect((await call(prompt)).status, `${call.name} stores`).toBe(200);
    }

    await seed.createGuardrail({
      name: "cml-output-keyword",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: "confidential" }],
    });
    await waitConfigPropagation(async () => (await chat(`probe-${Math.random()}`)).status === 422);

    const before = await outcomes();
    for (const call of [chat, messages, responses]) {
      expect((await call(prompt)).status, `${call.name} blocked hit`).toBe(422);
    }
    const delta = diff(before, await outcomes());

    expect(
      delta.hit_exact,
      "a blocked hit reached an enabled policy, so it must land on the series — " +
        "counted only on delivery it increments nothing at all, and cache events " +
        "stop summing to the requests the gate actually decided",
    ).toBe(3);
  }, 180_000);

  /** `{outcome: count}` for this spec's policy, documented labels only. */
  async function outcomes(): Promise<Record<string, number>> {
    const out: Record<string, number> = {};
    for (const s of await scrapeMetrics(app!.metricsUrl)) {
      if (s.name !== "aisix_cache_requests_total") continue;
      if (s.labels.policy !== POLICY) continue;
      expect(
        DOCUMENTED,
        `undocumented outcome label ${JSON.stringify(s.labels.outcome)} on ` +
          `aisix_cache_requests_total — the series' value set is fixed`,
      ).toContain(s.labels.outcome);
      out[s.labels.outcome] = (out[s.labels.outcome] ?? 0) + s.value;
    }
    return out;
  }

  function diff(before: Record<string, number>, after: Record<string, number>) {
    const out: Record<string, number> = {};
    for (const k of new Set([...Object.keys(before), ...Object.keys(after)])) {
      const d = (after[k] ?? 0) - (before[k] ?? 0);
      if (d !== 0) out[k] = d;
    }
    return out;
  }

  function chat(text: string) {
    return fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({ model: "cml-chat", messages: [{ role: "user", content: text }] }),
    });
  }

  function messages(text: string) {
    return fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER },
      body: JSON.stringify({
        model: "cml-messages",
        max_tokens: 64,
        messages: [{ role: "user", content: text }],
      }),
    });
  }

  function responses(text: string) {
    return fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({ model: "cml-responses", input: text }),
    });
  }
});
