import { createHash } from "node:crypto";
import { connect } from "node:net";
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

// A `CachePolicy` has no endpoint dimension: an operator writes one for a
// model, and it reasonably covers that model wherever it is addressed. Only
// `/v1/chat/completions` consulted the cache, so the policy silently did
// nothing for the Anthropic SDK on `/v1/messages`, for Codex on
// `/v1/responses`, and for `/v1/embeddings` — the single most cacheable
// surface a gateway has, where the same text always maps to the same vector
// and the provider charges for every repeat.
//
// Nothing errored; the requests just kept paying the upstream while the
// dashboard showed a policy in place. These specs pin the coverage per
// endpoint on both backends, because "cached" on memory and "cached" on
// redis are different code paths and only the redis one is shared between
// replicas.

const CALLER_PLAINTEXT = "sk-response-cache-coverage";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

const REDIS_URL = process.env.AISIX_E2E_REDIS ?? "redis://127.0.0.1:6379";

async function redisPing(url: string): Promise<boolean> {
  const m = /^redis:\/\/(?:[^@/]*@)?([^:/]+)(?::(\d+))?/.exec(url);
  if (!m) return false;
  const host = m[1];
  const port = m[2] ? Number(m[2]) : 6379;
  return new Promise((resolve) => {
    const sock = connect({ host, port }, () => sock.write("PING\r\n"));
    const done = (ok: boolean) => {
      sock.destroy();
      resolve(ok);
    };
    sock.once("data", (buf) => done(buf.toString().startsWith("+PONG")));
    sock.once("error", () => done(false));
    sock.setTimeout(1000, () => done(false));
  });
}

/** The three endpoints under test, and a request body for each. */
const CASES = [
  {
    endpoint: "/v1/messages",
    model: "cachecov-messages",
    body: {
      max_tokens: 64,
      messages: [{ role: "user", content: "cache coverage" }],
    },
  },
  {
    endpoint: "/v1/responses",
    model: "cachecov-responses",
    body: { input: "cache coverage" },
  },
  {
    endpoint: "/v1/embeddings",
    model: "cachecov-embeddings",
    body: { input: "cache coverage" },
  },
] as const;

function pathBodies(): Record<string, unknown> {
  return {
    "/v1/messages": {
      id: "msg_cachecov",
      type: "message",
      role: "assistant",
      model: "claude-3-5-haiku-20241022",
      content: [{ type: "text", text: "cached answer" }],
      stop_reason: "end_turn",
      usage: { input_tokens: 6, output_tokens: 3 },
    },
    "/v1/responses": {
      id: "resp_cachecov",
      object: "response",
      created_at: 0,
      status: "completed",
      model: "gpt-4o-mini",
      output: [
        {
          id: "msg_cachecov",
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: "cached answer" }],
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

/** Seed one environment: a model per endpoint, a policy, then the key. */
async function seedEnv(
  etcd: EtcdClient,
  prefix: string,
  upstreamBase: string,
  backend: "memory" | "redis",
): Promise<void> {
  const seed = new SeedClient(etcd, prefix);
  const anthropicPk = await seed.createProviderKey({
    display_name: "cachecov-anthropic-pk",
    secret: "sk-mock",
    api_base: upstreamBase,
    provider: "anthropic",
    adapter: "anthropic",
  });
  const openaiPk = await seed.createProviderKey({
    display_name: "cachecov-openai-pk",
    secret: "sk-mock",
    api_base: `${upstreamBase}/v1`,
  });
  await seed.createModel({
    display_name: "cachecov-messages",
    provider: "anthropic",
    model_name: "claude-3-5-haiku-20241022",
    provider_key_id: anthropicPk.id,
  });
  await seed.createModel({
    display_name: "cachecov-responses",
    provider: "openai",
    model_name: "gpt-4o-mini",
    provider_key_id: openaiPk.id,
  });
  await seed.createModel({
    display_name: "cachecov-embeddings",
    provider: "openai",
    model_name: "text-embedding-3-small",
    provider_key_id: openaiPk.id,
  });
  await seed.createCachePolicy({
    name: `cachecov-${backend}`,
    enabled: true,
    backend,
    ttl_seconds: 300,
    applies_to: "all",
  });
  // Last, so a gate on this key authenticating implies every row above.
  await seed.createApiKey({
    key_hash: CALLER_KEY_HASH,
    allowed_models: CASES.map((c) => c.model),
  });
}

async function post(
  app: SpawnedApp,
  endpoint: string,
  model: string,
  body: unknown,
): Promise<{ status: number; cache: string | null; text: string }> {
  const res = await fetch(`${app.proxyUrl}${endpoint}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CALLER_PLAINTEXT}`,
      "x-api-key": CALLER_PLAINTEXT,
      "content-type": "application/json",
    },
    body: JSON.stringify({ model, ...(body as Record<string, unknown>) }),
  });
  return {
    status: res.status,
    cache: res.headers.get("x-aisix-cache"),
    text: await res.text(),
  };
}

describe("response cache endpoint coverage e2e", () => {
  let upstream: OpenAiUpstream | undefined;
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({ pathBodies: pathBodies() });
    app = await spawnApp();
    await seedEnv(etcd, app.etcdPrefix, upstream.baseUrl, "memory");
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a policy covers every cacheable endpoint, not just chat", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // Gate on the caller key AND on the policy being live: a request that
    // reports a cache header proves both propagated, so the assertions below
    // cannot pass vacuously against an un-propagated policy.
    await waitConfigPropagation(async () => {
      const r = await post(app!, "/v1/embeddings", "cachecov-embeddings", {
        input: "readiness",
      });
      return r.status === 200 && r.cache !== null;
    });

    const uncovered: string[] = [];
    const notReplayed: string[] = [];
    for (const { endpoint, model, body } of CASES) {
      const before = upstream!.receivedRequests.filter((r) =>
        r.path.endsWith(endpoint),
      ).length;
      const first = await post(app!, endpoint, model, body);
      expect(first.status, `${endpoint} first call`).toBe(200);
      const second = await post(app!, endpoint, model, body);
      expect(second.status, `${endpoint} second call`).toBe(200);
      const after = upstream!.receivedRequests.filter((r) =>
        r.path.endsWith(endpoint),
      ).length;

      // The upstream is the ground truth: one call for two identical
      // requests is the only thing that proves a cache served the second.
      if (after - before !== 1 || second.cache !== "hit") {
        uncovered.push(
          `${endpoint}: upstream calls +${after - before} (want 1), ` +
            `x-aisix-cache=${second.cache} (want hit)`,
        );
        continue;
      }
      if (first.text !== second.text) {
        notReplayed.push(endpoint);
      }
    }

    expect(
      uncovered,
      `a cache policy covering these models did nothing on these endpoints, ` +
        `so every identical request kept paying the upstream:\n` +
        uncovered.map((u) => `  ${u}`).join("\n"),
    ).toEqual([]);
    expect(
      notReplayed,
      `these endpoints served a hit whose body differs from the original — a ` +
        `replay must be byte-identical, or the cache is rewriting responses:\n` +
        notReplayed.map((u) => `  ${u}`).join("\n"),
    ).toEqual([]);
  }, 120_000);
});

describe("response cache endpoint coverage on redis", () => {
  let upstream: OpenAiUpstream | undefined;
  let appA: SpawnedApp | undefined;
  let appB: SpawnedApp | undefined;
  let infraReady = false;

  beforeAll(async () => {
    infraReady = (await new EtcdClient().ping()) && (await redisPing(REDIS_URL));
    if (!infraReady) return;
    upstream = await startOpenAiUpstream({ pathBodies: pathBodies() });
    const extra = { cache: { backend: "memory", redis: { url: REDIS_URL } } };
    appA = await spawnApp({ extra });
    appB = await spawnApp({ extra, etcdPrefix: appA.etcdPrefix });
    await seedEnv(new EtcdClient(), appA.etcdPrefix, upstream.baseUrl, "redis");
  });

  afterAll(async () => {
    await appA?.exit();
    await appB?.exit();
    await upstream?.close();
  });

  // Two replicas of one environment sharing one redis: a hit on the replica
  // that never served the original request is the only way to show the entry
  // really landed in redis. A (wrong) node-local write would hit only on the
  // replica that wrote it, and a single-replica test could not tell the two
  // apart.
  test("entries written by one replica are served by another", async (ctx) => {
    if (!infraReady || !appA || !appB) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => {
      const a = await post(appA!, "/v1/embeddings", "cachecov-embeddings", {
        input: "readiness",
      });
      const b = await post(appB!, "/v1/embeddings", "cachecov-embeddings", {
        input: "readiness",
      });
      return a.status === 200 && b.status === 200 && a.cache !== null && b.cache !== null;
    });

    const notShared: string[] = [];
    for (const { endpoint, model, body } of CASES) {
      // A per-case input so the readiness probe above cannot have stored it.
      const scoped = { ...(body as Record<string, unknown>) };
      if ("input" in scoped) scoped.input = `redis ${endpoint}`;
      if ("messages" in scoped) {
        scoped.messages = [{ role: "user", content: `redis ${endpoint}` }];
      }
      const written = await post(appA!, endpoint, model, scoped);
      expect(written.status, `${endpoint} on replica A`).toBe(200);
      const read = await post(appB!, endpoint, model, scoped);
      expect(read.status, `${endpoint} on replica B`).toBe(200);
      if (read.cache !== "hit" || read.text !== written.text) {
        notShared.push(
          `${endpoint}: replica B reported ${read.cache}, bodies ${
            read.text === written.text ? "match" : "differ"
          }`,
        );
      }
    }

    expect(
      notShared,
      `these endpoints did not share their entries through redis, so each ` +
        `replica pays the upstream separately:\n` +
        notShared.map((n) => `  ${n}`).join("\n"),
    ).toEqual([]);
  }, 180_000);
});
