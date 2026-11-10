import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: in platform mode, a cached Responses answer is one the receiving key
// can continue.
//
// A Response's ids are sealed to the tenant together with the target that
// served it, and a continuation through `previous_response_id` is refused to
// a key whose provider groups do not hold that target. Keys of one tenant can
// hold different provider groups, and a team-scoped cache shares entries
// between them. So a key must never be handed a cached Response that came from
// a target outside its groups: it would get an answer it cannot go on from.
// Keys with the same groups still share.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const PLATFORM_SECRET_ENV = "P0_E2E_RESPONSES_CACHE_GROUPS_SECRET";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "3f9a1c7e5b2d4086a1e3c5f7092b4d6e8f0a1b2c3d4e5f60718293a4b5c6d7e8";
const KEYS = {
  /** In the alpha group. */
  alpha: "sk-platform-rc-groups-alpha",
  /** Same tenant, same group. */
  alphaTwin: "sk-platform-rc-groups-alpha-twin",
  /** Same tenant, the beta group only. */
  beta: "sk-platform-rc-groups-beta",
};
const ROUTE = "rc-groups-route";

const responseBody = (id: string) => ({
  id,
  object: "response",
  created_at: 1,
  status: "completed",
  model: "gpt-4o-mini",
  output: [
    {
      type: "message",
      id: "msg_rc_1",
      role: "assistant",
      status: "completed",
      content: [{ type: "output_text", text: "ok", annotations: [] }],
    },
  ],
  usage: { input_tokens: 3, output_tokens: 1, total_tokens: 4 },
});

describe("platform mode: a cached Response is one the receiving key can continue", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let alphaUpstream: OpenAiUpstream | undefined;
  let betaUpstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    alphaUpstream = await startOpenAiUpstream({
      pathBodies: { "/v1/responses": responseBody("resp_rc_alpha") },
    });
    betaUpstream = await startOpenAiUpstream({
      pathBodies: { "/v1/responses": responseBody("resp_rc_beta") },
    });
    app = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const target = async (name: string, groups: string[], upstream: OpenAiUpstream) => {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
        groups,
      });
      await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
        retries: 0,
      });
    };
    await target("rc-groups-alpha", ["rc-grp-alpha"], alphaUpstream);
    await target("rc-groups-beta", ["rc-grp-beta"], betaUpstream);
    await seed.createModel({
      display_name: ROUTE,
      routing: {
        strategy: "failover",
        targets: [{ model: "rc-groups-alpha" }, { model: "rc-groups-beta" }],
      },
    });
    await seed.createCachePolicy({
      name: "rc-groups-cache",
      enabled: true,
      backend: "memory",
      ttl_seconds: 300,
      applies_to: `model:${ROUTE}`,
      scope: "team",
    });
    // The keys last: once the last one authenticates, the rest is in.
    const team = randomUUID();
    await seed.createApiKey({
      key_hash: hash(KEYS.alpha),
      team_id: team,
      allowed_models: [ROUTE],
      allowed_provider_groups: ["rc-grp-alpha"],
    });
    await seed.createApiKey({
      key_hash: hash(KEYS.alphaTwin),
      team_id: team,
      allowed_models: [ROUTE],
      allowed_provider_groups: ["rc-grp-alpha"],
    });
    await seed.createApiKey({
      key_hash: hash(KEYS.beta),
      team_id: team,
      allowed_models: [ROUTE],
      allowed_provider_groups: ["rc-grp-beta"],
    });
    const last = new ProxyClient(app.proxyUrl, KEYS.beta);
    await waitConfigPropagation(async () => (await last.listModels()).status === 200);
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await alphaUpstream?.close();
    await betaUpstream?.close();
  });

  const respond = async (key: string, body: Record<string, unknown>) => {
    const res = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify({ model: ROUTE, ...body }),
    });
    const text = await res.text();
    return { status: res.status, text, json: () => JSON.parse(text) as { id: string } };
  };

  test("a key of the same tenant in another provider group gets its own answer, and goes on from it", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const input = `rc-groups ${randomUUID()}`;
    const a0 = alphaUpstream!.receivedRequests.length;
    const b0 = betaUpstream!.receivedRequests.length;

    const first = await respond(KEYS.alpha, { input });
    expect(first.status, first.text).toBe(200);
    expect(alphaUpstream!.receivedRequests.length - a0).toBe(1);

    // Control: a key with the same groups shares the tenant's entry.
    const twin = await respond(KEYS.alphaTwin, { input });
    expect(twin.status, twin.text).toBe(200);
    expect(alphaUpstream!.receivedRequests.length - a0, "served from the cache").toBe(1);
    expect(twin.json().id).toBe(first.json().id);

    // The beta key is answered by its own group's target…
    const beta = await respond(KEYS.beta, { input });
    expect(beta.status, beta.text).toBe(200);
    expect(betaUpstream!.receivedRequests.length - b0).toBe(1);
    expect(beta.json().id).not.toBe(first.json().id);

    // …and can go on from the answer it was handed.
    const next = await respond(KEYS.beta, {
      input: "and then?",
      previous_response_id: beta.json().id,
    });
    expect(next.status, next.text).toBe(200);
    expect(betaUpstream!.receivedRequests.length - b0).toBe(2);
    expect(alphaUpstream!.receivedRequests.length - a0).toBe(1);
  });
});
