import { createHash, randomUUID } from "node:crypto";
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

// ai-gateway#396. A rate-limit policy on an `hour` window accepted
// `max_tokens`, the control plane stored it, the dashboard displayed it — and
// the gateway ignored it. The only trace was one warn line per policy saying
// the cap was inert, which nobody reads in aggregate. The `day` window's token
// cap worked the whole time, which is exactly what made this invisible: the
// feature looked present because its sibling was.
//
// The failure mode is the worst kind for a limit: an operator who set an
// hourly token budget believed spend was bounded and it was not.
//
// Token windows are checked, not incremented, at admission — the count is only
// known once the upstream has answered — so the first request is always
// admitted and the cap bites on the next one. That is the same shape `tpm` and
// `tpd` have; this spec pins it for `tph`.

const CALLER = "sk-hourly-token-cap-e2e";
const KEY_ID = "a11c0000-0000-4000-8000-00000000cap1";
const POLICY_ID = "b22c0000-0000-4000-8000-00000000cap1";

describe("hourly token cap (ai-gateway#396)", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // 40 tokens per answer, so one request overshoots a 25-token hourly cap.
    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "chatcmpl-tph",
        object: "chat.completion",
        created: 1765000000,
        model: "gpt-4o-mini",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "spent" },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 25, completion_tokens: 15, total_tokens: 40 },
      },
    });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "tph-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "tph-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // An hour window carrying BOTH figures: the request cap is generous, so a
    // refusal can only come from the token cap.
    await seed.update("rate_limit_policies", POLICY_ID, {
      name: "hourly-tokens",
      scope: "api_key",
      scope_ref: KEY_ID,
      window: "hour",
      max_requests: 1000,
      max_tokens: 25,
    });
    // The caller key LAST, and at a fixed id because `api_key` scope matches
    // on the key's etcd entry id.
    await etcd.put(
      `${app.etcdPrefix}/api_keys/${KEY_ID}`,
      JSON.stringify({
        key_hash: createHash("sha256").update(CALLER).digest("hex"),
        allowed_models: ["tph-model"],
      }),
    );
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("an hourly token budget refuses the request after it is spent", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // The gate is the first billable call, so it cannot be a throwaway: it
    // spends the budget this test then asserts on. A 401 would mean the key
    // has not propagated, and the policy was written before it.
    await waitConfigPropagation(async () => (await chat()).status === 200);

    // The upstream reported 40 tokens against a 25-token hourly cap, so the
    // next request must be refused. Poll: the commit happens after the
    // response is handed back.
    let status = 0;
    for (let i = 0; i < 40; i++) {
      status = (await chat()).status;
      if (status === 429) break;
      await new Promise((r) => setTimeout(r, 50));
    }
    expect(
      status,
      "an hourly token cap that is spent must refuse the next request — an " +
        "accepted-but-inert cap leaves spend unbounded while the dashboard " +
        "shows a limit in place",
    ).toBe(429);

    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "tph-model",
        messages: [{ role: "user", content: "again" }],
      }),
    });
    expect(res.status).toBe(429);
    expect(
      res.headers.get("retry-after"),
      "the refusal must tell the caller when the hour reopens",
    ).toBeTruthy();
    const retryAfter = Number(res.headers.get("retry-after"));
    expect(retryAfter).toBeGreaterThan(0);
    expect(
      retryAfter,
      "an hour window's Retry-After cannot exceed the hour it is waiting on",
    ).toBeLessThanOrEqual(3600);
    await res.text();
  }, 120_000);

  async function chat(): Promise<{ status: number }> {
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "tph-model",
        messages: [{ role: "user", content: "spend" }],
      }),
    });
    await res.text();
    return { status: res.status };
  }
});
