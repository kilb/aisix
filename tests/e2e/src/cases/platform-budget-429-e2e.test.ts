import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockControlPlane,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MockControlPlane,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: in platform mode a spent budget tells a tenant about its own scope
// (its key, member, team or team member) and nothing else. A budget on the
// platform's side (a provider key, the environment) is a bare "limit
// reached": no scope, no id, no amount, no reset time. The same holds on
// every endpoint family, whether the caller asked for a stream or not.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const rfc3339 = (ms: number) => new Date(ms).toISOString().replace(".000Z", "Z");
const now = new Date();
const MONTH_START = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 1);

const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_BUDGET_KEY";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "ef4c49de00b0d8ff3bd998f2db44d9f2e2fc1c5520907e6bf4010a8dd173a311";
const TEAM_KEY = "sk-pb-e2e-team";
const PLAIN_KEY = "sk-pb-e2e-plain";
const SENTINEL_KEY = "sk-pb-e2e-sentinel";
const TEAM_ID = randomUUID();
/** Figures only the platform's budgets carry. */
const PLATFORM_LIMIT = "777.00";
const PLATFORM_SPENT = "777.25";
const NEUTRAL = "Spending limit reached.";
/** A name only the platform's shared rate-limit policies carry. */
const POLICY_NAME = "pb-platform-shared-policy";
const POLICY_KEYS = { chat: "sk-pb-e2e-policy-chat", messages: "sk-pb-e2e-policy-messages" };
const POLICY_TEAMS = { chat: randomUUID(), messages: randomUUID() };

const reason = (scope: string, scopeRef: string, limit: string, spent: string, name: string) => ({
  type: "billing_error",
  code: "budget_exceeded",
  message: `${name} budget exceeded`,
  scope,
  scope_ref: scopeRef,
  limit_usd: limit,
  spent_usd: spent,
  remaining_usd: "-0.25",
  period: "month",
});

type Path =
  | "/v1/chat/completions"
  | "/v1/messages"
  | "/v1/messages/count_tokens"
  | "/v1/responses"
  | "/v1/embeddings";
interface Route {
  path: Path;
  envelope: "openai" | "anthropic";
  /** The model family the route's models come from. */
  family: "chat" | "claude" | "embed";
  streams: boolean;
}
const ROUTES: Route[] = [
  { path: "/v1/chat/completions", envelope: "openai", family: "chat", streams: true },
  { path: "/v1/messages", envelope: "anthropic", family: "claude", streams: true },
  { path: "/v1/messages/count_tokens", envelope: "anthropic", family: "claude", streams: false },
  { path: "/v1/responses", envelope: "openai", family: "chat", streams: true },
  { path: "/v1/embeddings", envelope: "openai", family: "embed", streams: false },
];
const FAMILIES = ["chat", "claude", "embed"] as const;
type Family = (typeof FAMILIES)[number];
/** Models behind a clean provider key, and behind a spent one. */
const cleanModel = (f: Family) => `pb-clean-${f}`;
const spentModel = (f: Family) => `pb-spent-${f}`;

function body(route: Route, model: string, stream: boolean): Record<string, unknown> {
  switch (route.path) {
    case "/v1/chat/completions":
      return { model, stream, messages: [{ role: "user", content: "hi" }] };
    case "/v1/messages":
      return { model, stream, max_tokens: 64, messages: [{ role: "user", content: "hi" }] };
    case "/v1/messages/count_tokens":
      return { model, messages: [{ role: "user", content: "hi" }] };
    case "/v1/responses":
      return { model, stream, input: "hi" };
    case "/v1/embeddings":
      return { model, input: "hi" };
  }
}

describe("platform mode: a 429 shows a tenant only its own limits", () => {
  let cp: MockControlPlane | undefined;
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  const spentPkIds: string[] = [];

  const call = async (key: string, route: Route, model: string, stream: boolean) => {
    const res = await fetch(`${app!.proxyUrl}${route.path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "content-type": "application/json",
        ...(route.envelope === "anthropic" ? { "anthropic-version": "2023-06-01" } : {}),
      },
      body: JSON.stringify(body(route, model, stream)),
    });
    const text = await res.text();
    return { status: res.status, headers: res.headers, text, json: JSON.parse(text) as Record<string, any> };
  };

  /** A refusal that names nothing of the platform. */
  const expectHidden = (route: Route, answer: Awaited<ReturnType<typeof call>>) => {
    expect(answer.status, answer.text).toBe(429);
    const error = answer.json.error as Record<string, unknown>;
    expect(error.message).toBe(NEUTRAL);
    if (route.envelope === "anthropic") {
      expect(answer.json.type).toBe("error");
      expect(error.type).toBe("rate_limit_error");
    } else {
      expect(error.type).toBe("billing_error");
      expect(error.code).toBe("budget_exceeded");
    }
    for (const field of ["scope", "scope_ref", "limit_usd", "spent_usd", "period", "period_resets_at", "retry_after_seconds"]) {
      expect(error, `${field} leaks`).not.toHaveProperty(field);
    }
    const headerText = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
    const everything = `${answer.text}\n${headerText}`;
    // Only strings no uuid, timestamp or port can spell by chance.
    for (const leak of [PLATFORM_LIMIT, PLATFORM_SPENT, ...spentPkIds, "provider_key", "environment"]) {
      expect(everything, `leaks ${leak}`).not.toContain(leak);
    }
    // The reset time is the platform budget's own figure.
    expect(answer.headers.get("retry-after")).toBeNull();
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream();
    cp = await startMockControlPlane();
    app = await spawnApp({
      extra: { managed: cp.managed },
      awaitProxyListener: false,
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    seed = new SeedClient(etcd, `${app.etcdPrefix}/${cp.envId}`);

    for (const [label, spent] of [["clean", false], ["spent", true]] as const) {
      const openai = await seed.createProviderKey({
        display_name: `pb-pk-${label}-openai`,
        secret: "sk-upstream",
        api_base: `${upstream.baseUrl}/v1`,
      });
      const anthropic = await seed.createProviderKey({
        display_name: `pb-pk-${label}-anthropic`,
        provider: "anthropic",
        adapter: "anthropic",
        secret: "sk-ant-upstream",
        api_base: upstream.baseUrl,
      });
      const name = (f: Family) => (spent ? spentModel(f) : cleanModel(f));
      await seed.createModel({ display_name: name("chat"), provider: "openai", model_name: "gpt-4o-mini", provider_key_id: openai.id });
      await seed.createModel({
        display_name: name("claude"),
        provider: "anthropic",
        model_name: "claude-sonnet-4-5",
        provider_key_id: anthropic.id,
      });
      await seed.createModel({
        display_name: name("embed"),
        provider: "openai",
        model_name: "text-embedding-3-small",
        provider_key_id: openai.id,
        embedding: { dimensions: 2 },
      });
      if (spent) {
        for (const id of [openai.id, anthropic.id]) {
          spentPkIds.push(id);
          await seed.update("budgets", randomUUID(), {
            scope: "provider_key",
            scope_ref: id,
            period: "month",
            exhausted: {
              period_start: rfc3339(MONTH_START),
              reason: reason("provider_key", id, PLATFORM_LIMIT, PLATFORM_SPENT, `provider key ${id}`),
            },
          });
        }
      }
    }

    await seed.update("teams", TEAM_ID, { name: "pb-team", tags: {} });
    await seed.update("budgets", randomUUID(), {
      scope: "team",
      scope_ref: TEAM_ID,
      period: "month",
      exhausted: { period_start: rfc3339(MONTH_START), reason: reason("team", TEAM_ID, "10.00", "10.25", "team") },
    });

    // An ungrouped conditional policy is one counter for every caller it
    // matches. One team, key and policy per route, so the first request of
    // each route is the one that takes the single slot.
    for (const label of ["chat", "messages"] as const) {
      await seed.update("teams", POLICY_TEAMS[label], { name: `pb-policy-team-${label}`, tags: {} });
      await seed.update("rate_limit_policies", randomUUID(), {
        name: POLICY_NAME,
        conditions: [{ dimension: "team", operator: "in", value: [POLICY_TEAMS[label]] }],
        limits: { rpm: 1 },
      });
      await seed.update("api_keys", randomUUID(), {
        key_hash: hash(POLICY_KEYS[label]),
        allowed_models: ["*"],
        team_id: POLICY_TEAMS[label],
      });
    }
    // The caller keys go in last: once the sentinel authenticates, every
    // resource above is in the snapshot.
    await seed.update("api_keys", randomUUID(), { key_hash: hash(TEAM_KEY), allowed_models: ["*"], team_id: TEAM_ID });
    await seed.update("api_keys", randomUUID(), { key_hash: hash(PLAIN_KEY), allowed_models: ["*"] });
    await seed.update("api_keys", randomUUID(), { key_hash: hash(SENTINEL_KEY), allowed_models: ["*"] });

    await waitConfigPropagation(async () => {
      try {
        return (await new ProxyClient(app!.proxyUrl, SENTINEL_KEY).listModels()).status === 200;
      } catch (err) {
        // A managed gateway binds its proxy listener once the first
        // configuration applies; until then the connection is refused.
        const refused = err as { code?: string; cause?: { code?: string } };
        if ((refused.code ?? refused.cause?.code) === "ECONNREFUSED") return false;
        throw err;
      }
    });
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await cp?.close();
    await upstream?.close();
  });

  describe.each(ROUTES)("$path", (route) => {
    const modes = route.streams ? [false, true] : [false];

    test.for(modes)("a team budget refusal shows the team's own figures (stream: %s)", async (stream, ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(TEAM_KEY, route, cleanModel(route.family), stream);
      expect(answer.status, answer.text).toBe(429);
      if (route.envelope === "anthropic") {
        // The Anthropic envelope has no structured budget fields on any
        // deployment; it still tells the tenant which limit it hit.
        expect(answer.json.error.message).toContain("budget");
      } else {
        const error = answer.json.error;
        expect(error.code).toBe("budget_exceeded");
        expect(error.scope).toBe("team");
        expect(error.scope_ref).toBe(TEAM_ID);
        expect(error.limit_usd).toBe("10.00");
        expect(error.spent_usd).toBe("10.25");
      }
      expect(Number(answer.headers.get("retry-after"))).toBeGreaterThan(0);
    });

    test.for(modes)("a provider key budget refusal reveals nothing of the key or its spend (stream: %s)", async (stream, ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectHidden(route, await call(PLAIN_KEY, route, spentModel(route.family), stream));
    });
  });

  test.for([
    ["chat", ROUTES[0]],
    ["messages", ROUTES[1]],
  ] as const)("a shared policy refusal on %s names no policy and no figures", async ([label, route], ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const key = POLICY_KEYS[label];
    // The first request takes the single slot; what it gets back is not the point.
    await call(key, route, cleanModel(route.family), false);
    const answer = await call(key, route, cleanModel(route.family), false);
    expect(answer.status, answer.text).toBe(429);
    expect(answer.json.error).not.toHaveProperty("policy");
    expect(answer.headers.get("x-ratelimit-limit")).toBeNull();
    expect(answer.headers.get("x-ratelimit-remaining")).toBeNull();
    expect(answer.headers.get("x-ratelimit-scope")).toBeNull();
    const headerText = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
    expect(`${answer.text}\n${headerText}`).not.toContain(POLICY_NAME);
    // Windows are clock-aligned, so the back-off hint is only a window length.
    expect(Number(answer.headers.get("retry-after"))).toBeGreaterThan(0);
  });

  // Last: it refuses every key in the environment until removed.
  test("an environment budget refusal reveals nothing on any endpoint", async (ctx) => {
    if (!etcdReachable || !app || !seed) return ctx.skip();
    await seed.update("budgets", randomUUID(), {
      scope: "environment",
      scope_ref: "env",
      period: "week",
      exhausted: { reason: reason("environment", "env", PLATFORM_LIMIT, PLATFORM_SPENT, "environment") },
    });
    const chat = ROUTES[0];
    await waitConfigPropagation(async () => (await call(PLAIN_KEY, chat, cleanModel("chat"), false)).status === 429);
    for (const route of ROUTES) {
      for (const stream of route.streams ? [false, true] : [false]) {
        expectHidden(route, await call(PLAIN_KEY, route, cleanModel(route.family), stream));
      }
    }
  });
});
