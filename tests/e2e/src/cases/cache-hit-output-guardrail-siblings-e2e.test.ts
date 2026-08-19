import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// The sibling half of #448. That issue established the contract for
// `/v1/chat/completions`: a cache HIT is client-visible output and must run
// the output chain rather than bypass it, or a response stored before a
// guardrail existed keeps being replayed past it.
//
// When caching was extended to `/v1/messages` and `/v1/responses`, the hit
// path returned the stored bytes verbatim — re-introducing exactly the bug
// #448 fixed, on two more endpoints. The stored body is moderated under the
// policy in force when it is WRITTEN, so the hole is specifically a policy
// TIGHTENED afterwards: for the whole TTL (schema max seven days) the
// gateway keeps serving content the operator has since forbidden.
//
// One spec per endpoint would have caught it; nothing did, because the
// contract lived only in chat's spec. This is that contract, applied to the
// siblings.

const CALLER = "sk-cache-gr-siblings";
const HASH = createHash("sha256").update(CALLER).digest("hex");

describe("cache hits run output guardrails on the sibling endpoints", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Both endpoints answer with a body containing the literal "confidential",
    // which the guardrail attached mid-test will block.
    upstream = await startOpenAiUpstream({
      pathBodies: {
        "/v1/messages": {
          id: "msg_cgs",
          type: "message",
          role: "assistant",
          model: "claude-3-5-haiku-20241022",
          content: [{ type: "text", text: "this is confidential material" }],
          stop_reason: "end_turn",
          usage: { input_tokens: 5, output_tokens: 4 },
        },
        "/v1/responses": {
          id: "resp_cgs",
          object: "response",
          created_at: 0,
          status: "completed",
          model: "gpt-4o-mini",
          output: [
            {
              id: "msg_cgs",
              type: "message",
              role: "assistant",
              content: [{ type: "output_text", text: "this is confidential material" }],
            },
          ],
          usage: { input_tokens: 5, output_tokens: 4, total_tokens: 9 },
        },
      },
    });
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    const anthropicPk = await seed.createProviderKey({
      display_name: "cgs-anthropic-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
      provider: "anthropic",
      adapter: "anthropic",
    });
    const openaiPk = await seed.createProviderKey({
      display_name: "cgs-openai-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "cgs-messages",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: anthropicPk.id,
    });
    await seed.createModel({
      display_name: "cgs-responses",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openaiPk.id,
    });
    await seed.createCachePolicy({
      name: "cgs-policy",
      enabled: true,
      applies_to: "all",
    });
    // Last, per `tests/e2e/AGENTS.md`.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: ["cgs-messages", "cgs-responses"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a response cached before the guardrail is blocked on the hit", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => (await messages("ready")).ok);
    await waitConfigPropagation(async () => (await responses("ready")).ok);

    // 1) Populate both caches BEFORE any output guardrail exists.
    const cachedPrompt = "cache-and-guard-me";
    const m1 = await messages(cachedPrompt);
    expect(m1.status, "first /v1/messages call populates the cache").toBe(200);
    expect(m1.headers.get("x-aisix-cache")).toBe("miss");
    const r1 = await responses(cachedPrompt);
    expect(r1.status, "first /v1/responses call populates the cache").toBe(200);
    expect(r1.headers.get("x-aisix-cache")).toBe("miss");

    // 2) Attach an output guardrail the stored bodies violate.
    await seed.createGuardrail({
      name: "cgs-output-keyword",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: "confidential" }],
    });

    // Gate on propagation via a FRESH prompt (a miss), which must now block.
    await waitConfigPropagation(
      async () => (await messages(`probe-${Math.random()}`)).status === 422,
    );

    // 3) The cached requests are hits — and must now be blocked rather than
    //    replayed. A pass here is a policy bypass with a TTL-long window.
    const mHit = await messages(cachedPrompt);
    expect(
      mHit.status,
      "a /v1/messages cache hit must run the output chain, not replay the " +
        "stored body past a guardrail the operator has since added",
    ).toBe(422);

    const rHit = await responses(cachedPrompt);
    expect(
      rHit.status,
      "a /v1/responses cache hit must run the output chain, not replay the " +
        "stored body past a guardrail the operator has since added",
    ).toBe(422);
  }, 120_000);

  function messages(text: string) {
    return fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER },
      body: JSON.stringify({
        model: "cgs-messages",
        max_tokens: 64,
        messages: [{ role: "user", content: text }],
      }),
    });
  }

  function responses(text: string) {
    return fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${CALLER}`,
      },
      body: JSON.stringify({ model: "cgs-responses", input: text }),
    });
  }
});
