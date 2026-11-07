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

// A cache policy with `scope: team` on a gateway shared by several tenants.
//
// `env` would share one answer with every caller in the environment, which on
// a multi-tenant gateway means one tenant's answers replayed to another. The
// `team` scope draws the boundary at the tenant: the keys of one team reuse
// each other's entries, a key of another team never reads them, and a key that
// belongs to no team shares with no one.
//
// The upstream's request count is the ground truth: one upstream call for two
// identical requests is the only thing that proves the second was served from
// the cache, and a second upstream call for the other tenant is the only thing
// that proves it was not.

/** Plaintext of each caller key, by role. */
const KEYS = {
  tenantOneFirst: "sk-cache-team-scope-t1-a",
  tenantOneSecond: "sk-cache-team-scope-t1-b",
  tenantTwo: "sk-cache-team-scope-t2",
  noTeamFirst: "sk-cache-team-scope-none-a",
  noTeamSecond: "sk-cache-team-scope-none-b",
} as const;

const hash = (plaintext: string) =>
  createHash("sha256").update(plaintext).digest("hex");

/** Each cacheable endpoint, the model it is addressed through, and a body. */
const CASES = [
  {
    endpoint: "/v1/chat/completions",
    model: "teamscope-chat",
    body: (text: string) => ({ messages: [{ role: "user", content: text }] }),
  },
  {
    endpoint: "/v1/messages",
    model: "teamscope-messages",
    body: (text: string) => ({
      max_tokens: 64,
      messages: [{ role: "user", content: text }],
    }),
  },
  {
    endpoint: "/v1/responses",
    model: "teamscope-responses",
    body: (text: string) => ({ input: text }),
  },
  {
    endpoint: "/v1/embeddings",
    model: "teamscope-embeddings",
    body: (text: string) => ({ input: text }),
  },
] as const;

function pathBodies(): Record<string, unknown> {
  return {
    "/v1/chat/completions": {
      id: "chatcmpl-teamscope",
      object: "chat.completion",
      created: 0,
      model: "gpt-4o-mini",
      choices: [
        {
          index: 0,
          message: { role: "assistant", content: "team answer" },
          finish_reason: "stop",
        },
      ],
      usage: { prompt_tokens: 6, completion_tokens: 3, total_tokens: 9 },
    },
    "/v1/messages": {
      id: "msg_teamscope",
      type: "message",
      role: "assistant",
      model: "claude-3-5-haiku-20241022",
      content: [{ type: "text", text: "team answer" }],
      stop_reason: "end_turn",
      usage: { input_tokens: 6, output_tokens: 3 },
    },
    "/v1/responses": {
      id: "resp_teamscope",
      object: "response",
      created_at: 0,
      status: "completed",
      model: "gpt-4o-mini",
      output: [
        {
          id: "msg_teamscope",
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "team answer" }],
        },
      ],
      usage: { input_tokens: 6, output_tokens: 3, total_tokens: 9 },
    },
    "/v1/embeddings": {
      object: "list",
      model: "text-embedding-3-small",
      data: [{ object: "embedding", index: 0, embedding: [0.25, 0.5] }],
      usage: { prompt_tokens: 6, total_tokens: 6 },
    },
  };
}

async function post(
  app: SpawnedApp,
  plaintext: string,
  endpoint: string,
  model: string,
  body: Record<string, unknown>,
): Promise<{ status: number; cache: string | null; headers: Headers; text: string }> {
  const res = await fetch(`${app.proxyUrl}${endpoint}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${plaintext}`,
      "x-api-key": plaintext,
      "content-type": "application/json",
    },
    body: JSON.stringify({ model, ...body }),
  });
  return {
    status: res.status,
    cache: res.headers.get("x-aisix-cache"),
    headers: res.headers,
    text: await res.text(),
  };
}

/**
 * Seed every model and the team-scoped policy, then the caller keys last, and
 * wait until the last key authenticates: that implies every row is in the
 * gateway's snapshot.
 */
async function seedTeamScope(
  etcd: EtcdClient,
  app: SpawnedApp,
  upstream: OpenAiUpstream,
): Promise<void> {
  const seed = new SeedClient(etcd, app.etcdPrefix);
  const anthropicPk = await seed.createProviderKey({
    display_name: "teamscope-anthropic-pk",
    secret: "sk-mock",
    api_base: upstream.baseUrl,
    provider: "anthropic",
    adapter: "anthropic",
  });
  const openaiPk = await seed.createProviderKey({
    display_name: "teamscope-openai-pk",
    secret: "sk-mock",
    api_base: `${upstream.baseUrl}/v1`,
  });
  await seed.createModel({
    display_name: "teamscope-chat",
    provider: "openai",
    model_name: "gpt-4o-mini",
    provider_key_id: openaiPk.id,
  });
  await seed.createModel({
    display_name: "teamscope-messages",
    provider: "anthropic",
    model_name: "claude-3-5-haiku-20241022",
    provider_key_id: anthropicPk.id,
  });
  await seed.createModel({
    display_name: "teamscope-responses",
    provider: "openai",
    model_name: "gpt-4o-mini",
    provider_key_id: openaiPk.id,
  });
  await seed.createModel({
    display_name: "teamscope-embeddings",
    provider: "openai",
    model_name: "text-embedding-3-small",
    provider_key_id: openaiPk.id,
  });
  await seed.createCachePolicy({
    name: "teamscope",
    enabled: true,
    backend: "memory",
    ttl_seconds: 300,
    applies_to: "all",
    scope: "team",
  });

  // The caller keys come last, so the gate below on the last of them
  // authenticating implies every row above is in the snapshot.
  const tenantOne = randomUUID();
  const tenantTwo = randomUUID();
  const allowed_models = CASES.map((c) => c.model);
  await seed.createApiKey({
    key_hash: hash(KEYS.tenantOneFirst),
    team_id: tenantOne,
    allowed_models,
  });
  await seed.createApiKey({
    key_hash: hash(KEYS.tenantOneSecond),
    team_id: tenantOne,
    allowed_models,
  });
  await seed.createApiKey({
    key_hash: hash(KEYS.tenantTwo),
    team_id: tenantTwo,
    allowed_models,
  });
  await seed.createApiKey({ key_hash: hash(KEYS.noTeamFirst), allowed_models });
  await seed.createApiKey({ key_hash: hash(KEYS.noTeamSecond), allowed_models });

  const last = new ProxyClient(app.proxyUrl, KEYS.noTeamSecond);
  await waitConfigPropagation(async () => (await last.listModels()).status === 200);
}

describe("cache team scope e2e", () => {
  let upstream: OpenAiUpstream | undefined;
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  const upstreamCalls = (endpoint: string) =>
    upstream!.receivedRequests.filter((r) => r.path.endsWith(endpoint)).length;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({ pathBodies: pathBodies() });
    // Chat completions report `x-aisix-cache` only with the debug headers on.
    app = await spawnApp({ exposeDebugHeaders: true });
    await seedTeamScope(etcd, app, upstream);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a team's keys share entries and another team's key never reads them", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const failures: string[] = [];
    for (const { endpoint, model, body } of CASES) {
      const request = body(`shared within one tenant via ${endpoint}`);
      // A chat hit stamps the current second into `created`, so only that
      // number may differ; every other byte, and every byte the other
      // endpoints answer, is the answer.
      const stamp = (t: string) =>
        endpoint.endsWith("/chat/completions") ? t.replace(/"created":\d+/, '"created":0') : t;
      const before = upstreamCalls(endpoint);

      const first = await post(app, KEYS.tenantOneFirst, endpoint, model, request);
      expect(first.status, `${endpoint}: first key of tenant one`).toBe(200);

      const teammate = await post(app, KEYS.tenantOneSecond, endpoint, model, request);
      expect(teammate.status, `${endpoint}: second key of tenant one`).toBe(200);
      const afterTeammate = upstreamCalls(endpoint);
      if (afterTeammate - before !== 1 || teammate.cache !== "hit") {
        failures.push(
          `${endpoint}: tenant one's second key was not served tenant one's entry ` +
            `(upstream calls +${afterTeammate - before}, want 1; ` +
            `x-aisix-cache=${teammate.cache}, want hit)`,
        );
      } else if (stamp(teammate.text) !== stamp(first.text)) {
        failures.push(`${endpoint}: the teammate's hit differs from the original answer`);
      }

      const otherTenant = await post(app, KEYS.tenantTwo, endpoint, model, request);
      expect(otherTenant.status, `${endpoint}: key of tenant two`).toBe(200);
      const afterOtherTenant = upstreamCalls(endpoint);
      if (afterOtherTenant - afterTeammate !== 1 || otherTenant.cache === "hit") {
        failures.push(
          `${endpoint}: tenant two was served tenant one's entry ` +
            `(upstream calls +${afterOtherTenant - afterTeammate}, want 1; ` +
            `x-aisix-cache=${otherTenant.cache}, want a miss)`,
        );
      }
    }

    expect(
      failures,
      `under a team-scoped cache policy:\n${failures.map((f) => `  ${f}`).join("\n")}`,
    ).toEqual([]);
  });

  test("a key that belongs to no team shares its entries with no one", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const failures: string[] = [];
    for (const { endpoint, model, body } of CASES) {
      const request = body(`no tenant via ${endpoint}`);
      const before = upstreamCalls(endpoint);

      const first = await post(app, KEYS.noTeamFirst, endpoint, model, request);
      expect(first.status, `${endpoint}: first key without a team`).toBe(200);
      const second = await post(app, KEYS.noTeamSecond, endpoint, model, request);
      expect(second.status, `${endpoint}: second key without a team`).toBe(200);

      const calls = upstreamCalls(endpoint) - before;
      if (calls !== 2 || second.cache === "hit") {
        failures.push(
          `${endpoint}: two keys without a team shared an entry ` +
            `(upstream calls +${calls}, want 2; x-aisix-cache=${second.cache}, want a miss)`,
        );
      }
    }

    expect(
      failures,
      `under a team-scoped cache policy:\n${failures.map((f) => `  ${f}`).join("\n")}`,
    ).toEqual([]);
  });
});

// The team scope is what a multi-tenant gateway runs, and there a hit is a
// fact about another key: it says a teammate already asked the same thing. In
// platform mode that is a debug fact like any other, so unless the operator
// turned the debug headers on, the teammate is served the entry without any
// header saying the cache answered.
describe("cache team scope in platform mode e2e", () => {
  const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_ID_KEY";
  let upstream: OpenAiUpstream | undefined;
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  const upstreamCalls = (endpoint: string) =>
    upstream!.receivedRequests.filter((r) => r.path.endsWith(endpoint)).length;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({ pathBodies: pathBodies() });
    app = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        // 32 random bytes as hex digits, the form the gateway requires.
        [PLATFORM_SECRET_ENV]: "7f4c155f7eed0594e4d600ae4dadde1b9454198b268ba9319f4249b9d87d286c",
      },
    });
    await seedTeamScope(etcd, app, upstream);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a teammate is served the team's entry and neither answer says the cache was involved", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const failures: string[] = [];
    for (const { endpoint, model, body } of CASES) {
      const request = body(`shared within one tenant in platform mode via ${endpoint}`);
      const before = upstreamCalls(endpoint);

      const first = await post(app, KEYS.tenantOneFirst, endpoint, model, request);
      expect(first.status, `${endpoint}: first key of tenant one`).toBe(200);
      const teammate = await post(app, KEYS.tenantOneSecond, endpoint, model, request);
      expect(teammate.status, `${endpoint}: second key of tenant one`).toBe(200);

      // The upstream is the ground truth: one call for the two requests.
      const calls = upstreamCalls(endpoint) - before;
      if (calls !== 1) {
        failures.push(
          `${endpoint}: tenant one's second key was not served tenant one's entry ` +
            `(upstream calls +${calls}, want 1)`,
        );
      }
      for (const [who, answer] of [
        ["first key", first],
        ["teammate", teammate],
      ] as const) {
        const named = [...answer.headers.keys()].filter((h) => h.startsWith("x-aisix-cache"));
        if (named.length > 0) {
          failures.push(`${endpoint}: the ${who}'s answer carries ${named.join(", ")}`);
        }
      }
    }

    expect(
      failures,
      `under a team-scoped cache policy in platform mode:\n${failures.map((f) => `  ${f}`).join("\n")}`,
    ).toEqual([]);
  });
});
