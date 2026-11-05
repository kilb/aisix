import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: `background_model_check.interval_seconds` is a per-model setting.
//
// Each check is a real, billed call against the upstream, so an operator who
// sets one model to an hour and another to five seconds is telling the gateway
// how often it may spend each model's money. A hung upstream behind one model
// is that model's problem: it may not delay anyone else's checks, and it may
// not hold up a shutdown for its whole timeout.
//
// Interval and timeout values come from the scenario: 5 s is the schema's
// minimum, and 10 s of waiting therefore covers at least one more check.

const CALLER_KEY_HASH = createHash("sha256")
  .update("sk-background-interval-caller")
  .digest("hex");

function check(intervalSeconds: number, timeoutSeconds: number): Record<string, unknown> {
  return {
    enabled: true,
    interval_seconds: intervalSeconds,
    timeout_seconds: timeoutSeconds,
    prompt: "ping",
    max_tokens: 1,
    stale_after_seconds: 600,
  };
}

describe("background model checks run on each model's own interval", () => {
  let app: SpawnedApp | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  const upstreams: OpenAiUpstream[] = [];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  async function seededModel(
    name: string,
    upstream: OpenAiUpstream,
    extra: Record<string, unknown>,
  ): Promise<void> {
    const pk = await seed!.createProviderKey({
      display_name: `${name}-pk`,
      provider: "openai",
      api_key: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed!.createModel({
      display_name: name,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
      ...extra,
    });
  }

  test("a short interval, a long one, and a hung upstream", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }
    const frequent = await startOpenAiUpstream();
    const rare = await startOpenAiUpstream();
    const hung = await startOpenAiUpstream({ responseDelayMs: 120_000 });
    upstreams.push(frequent, rare, hung);

    await seededModel("interval-hung", hung, {
      background_model_check: check(3600, 300),
    });
    await seededModel("interval-rare", rare, {
      background_model_check: check(3600, 10),
    });
    await seededModel("interval-frequent", frequent, {
      background_model_check: check(5, 3),
    });

    await new Promise((r) => setTimeout(r, 11_000));

    // At least its first check and one more, while `interval-hung` hangs.
    expect(frequent.receivedRequests.length).toBeGreaterThanOrEqual(2);
    expect(rare.receivedRequests.length).toBe(1);
    expect(hung.receivedRequests.length).toBe(1);

    app.signal("SIGTERM");
    await app.waitForExit(10_000);
  }, 60_000);
});

describe("a background check on an embedding model", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({
      pathBodies: {
        "/v1/embeddings": {
          object: "list",
          model: "text-embedding-3-small",
          data: [{ index: 0, object: "embedding", embedding: [0.1, 0.2] }],
          usage: { prompt_tokens: 1, total_tokens: 1 },
        },
      },
    });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "embed-check-pk",
      provider: "openai",
      api_key: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "embed-check-model",
      provider: "openai",
      model_name: "text-embedding-3-small",
      provider_key_id: pk.id,
      embedding: { dimensions: 2 },
      background_model_check: check(5, 3),
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("calls the embeddings endpoint, not chat", async (ctx) => {
    if (!etcdReachable || !upstream) {
      ctx.skip();
      return;
    }
    const deadline = Date.now() + 15_000;
    while (upstream.receivedRequests.length === 0 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 50));
    }
    const paths = upstream.receivedRequests.map((r) => r.path);
    expect(paths.length).toBeGreaterThan(0);
    expect(paths.every((p) => p.endsWith("/embeddings"))).toBe(true);
  }, 30_000);
});
