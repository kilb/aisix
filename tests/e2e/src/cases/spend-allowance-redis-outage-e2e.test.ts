import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  metricDelta,
  pickFreePort,
  scrapeMetrics,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a prepaid spend allowance (`granted_micro_usd` on a rate-limit
// policy) lives in the shared Redis rate-limit store, because every gateway
// instance spends from it. While that store does not answer, one instance's
// own count cannot tell what the others spent, so a request the allowance
// applies to is refused with `503` `allowance_unverified` and the message
// "Spending could not be verified right now. Retry later.", with no figures
// and no `Retry-After`, on every endpoint and before any upstream call.
// Every other limit keeps serving on the instance's own count: a key under a
// daily spend ceiling is still answered. The refusal names no upstream, so it
// is the same with platform mode on, and it is counted in
// `aisix_ratelimit_rejections_total` under its own scope.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const ALLOWANCE_KEY = "sk-allowance-outage-e2e-granted";
const CEILING_KEY = "sk-allowance-outage-e2e-ceiling";
const SENTINEL_KEY = "sk-allowance-outage-e2e-sentinel";
const MESSAGE = "Spending could not be verified right now. Retry later.";
/** A figure only the allowance carries; no answer may spell it. */
const GRANTED_MICRO_USD = 7_340_017;
const PLATFORM_SECRET_ENV = "P0_E2E_ALLOWANCE_PLATFORM_KEY";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "c4e1a9b7d3f25e8a6c0b4d2f9e7a1c3b5d8f0a2c4e6b8d1f3a5c7e9b0d2f4a6c";

type Route = { path: string; anthropic: boolean; stream: boolean; body: Record<string, unknown> };
const ROUTES: Route[] = [
  {
    path: "/v1/chat/completions",
    anthropic: false,
    stream: false,
    body: { model: "ao-chat", messages: [{ role: "user", content: "hi" }] },
  },
  {
    path: "/v1/chat/completions",
    anthropic: false,
    stream: true,
    body: { model: "ao-chat", stream: true, messages: [{ role: "user", content: "hi" }] },
  },
  {
    path: "/v1/messages",
    anthropic: true,
    stream: false,
    body: { model: "ao-claude", max_tokens: 16, messages: [{ role: "user", content: "hi" }] },
  },
  {
    path: "/v1/messages",
    anthropic: true,
    stream: true,
    body: { model: "ao-claude", stream: true, max_tokens: 16, messages: [{ role: "user", content: "hi" }] },
  },
  { path: "/v1/responses", anthropic: false, stream: false, body: { model: "ao-chat", input: "hi" } },
  { path: "/v1/embeddings", anthropic: false, stream: false, body: { model: "ao-embed", input: "hi" } },
];

describe.each([false, true])("a spend allowance refuses while the shared rate-limit store does not answer (platform mode: %s)", (platform) => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  const call = async (key: string, route: Route) => {
    const res = await fetch(`${app!.proxyUrl}${route.path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "content-type": "application/json",
        ...(route.anthropic ? { "anthropic-version": "2023-06-01" } : {}),
      },
      body: JSON.stringify(route.body),
    });
    return { status: res.status, headers: res.headers, text: await res.text() };
  };
  const upstreamCalls = () => upstream!.receivedRequests.length;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream();
    // A port nothing listens on: every Redis command is refused at once.
    const deadRedis = `redis://127.0.0.1:${await pickFreePort()}`;
    app = await spawnApp({
      extra: { ratelimit: { backend: "redis", redis: { url: deadRedis, timeout_secs: 1 } } },
      ...(platform
        ? {
            extraEnv: {
              AISIX_PROXY__PLATFORM__ENABLED: "true",
              AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
              [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
            },
          }
        : {}),
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const openai = await seed.createProviderKey({
      display_name: "ao-pk-openai",
      secret: "sk-upstream",
      api_base: `${upstream.baseUrl}/v1`,
    });
    const anthropic = await seed.createProviderKey({
      display_name: "ao-pk-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-upstream",
      api_base: upstream.baseUrl,
    });
    await seed.createModel({ display_name: "ao-chat", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: openai.id });
    await seed.createModel({
      display_name: "ao-claude",
      provider: "anthropic",
      model_name: "claude-sonnet-4-5",
      provider_key_id: anthropic.id,
    });
    await seed.createModel({
      display_name: "ao-embed",
      provider: "openai",
      model_name: "text-embedding-3-small",
      provider_key_id: openai.id,
      embedding: { dimensions: 2 },
    });

    const grantedId = randomUUID();
    const ceilingId = randomUUID();
    await seed.update("rate_limit_policies", randomUUID(), {
      name: "ao-allowance",
      scope: "api_key",
      scope_ref: grantedId,
      granted_micro_usd: GRANTED_MICRO_USD,
    });
    await seed.update("rate_limit_policies", randomUUID(), {
      name: "ao-ceiling",
      scope: "api_key",
      scope_ref: ceilingId,
      window: "day",
      max_spend_micro_usd: 50_000_000,
    });
    // The caller keys go in last, the sentinel after them: once it
    // authenticates, everything above is in the snapshot.
    await seed.update("api_keys", grantedId, { key_hash: hash(ALLOWANCE_KEY), allowed_models: ["*"] });
    await seed.update("api_keys", ceilingId, { key_hash: hash(CEILING_KEY), allowed_models: ["*"] });
    await seed.update("api_keys", randomUUID(), { key_hash: hash(SENTINEL_KEY), allowed_models: ["*"] });
    await waitConfigPropagation(
      async () => (await new ProxyClient(app!.proxyUrl, SENTINEL_KEY).listModels()).status === 200,
    );
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test.for(ROUTES)("$path (stream: $stream) answers 503 allowance_unverified", async (route, ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const before = upstreamCalls();
    const metricsBefore = await scrapeMetrics(app.metricsUrl);
    const answer = await call(ALLOWANCE_KEY, route);
    expect(answer.status, answer.text).toBe(503);
    const json = JSON.parse(answer.text) as { type?: string; error: Record<string, unknown> };
    expect(json.error.message).toBe(MESSAGE);
    if (route.anthropic) {
      expect(json.type).toBe("error");
    } else {
      expect(json.error.code).toBe("allowance_unverified");
      expect(json.error.type).toBe("api_error");
    }
    expect(answer.headers.get("retry-after")).toBeNull();
    expect(answer.text).not.toContain(String(GRANTED_MICRO_USD));
    expect(answer.text).not.toContain("ao-allowance");
    expect(upstreamCalls(), "nothing reached the upstream").toBe(before);
    const metricsAfter = await scrapeMetrics(app.metricsUrl);
    expect(
      metricDelta(metricsBefore, metricsAfter, "aisix_ratelimit_rejections_total", {
        scope: "spend_allowance_unverified",
        layer: "policy_spend",
      }),
    ).toBe(1);
  });

  test("a key under a windowed spend ceiling is still served", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const chat = await new ProxyClient(app.proxyUrl, CEILING_KEY).chat({
      model: "ao-chat",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(chat.status, JSON.stringify(chat.body)).toBe(200);
  });
});
