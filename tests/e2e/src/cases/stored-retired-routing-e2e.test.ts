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

// E2E: a model group stored while `strategy: weighted` and `sticky` shipped.
//
// A control plane holds rows written under the older contract, and a gateway
// restart reads them again. Those groups must keep serving through the
// deprecation window, with the behaviour they were configured for: the group
// splits traffic across its targets, and a sticky group holds a key on one of
// them. The status has to say the row is on borrowed time.

const CALLER_PLAINTEXT = "sk-stored-routing-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

interface StatusConfig {
  rejected: Array<{ resource_id: string }>;
  partially_compatible: Array<{ resource_kind: string; field: string; count: number }>;
}

describe("a stored group using a retired routing setting", () => {
  let app: SpawnedApp | undefined;
  let seed: SeedClient | undefined;
  const upstreams: OpenAiUpstream[] = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstreams.push(await startOpenAiUpstream(), await startOpenAiUpstream());
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [i, upstream] of upstreams.entries()) {
      const pk = await seed.createProviderKey({
        display_name: `stored-routing-pk-${i}`,
        provider: "openai",
        api_key: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: `stored-routing-target-${i}`,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
    }
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  async function chat(model: string, headers: Record<string, string> = {}): Promise<number> {
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
        ...headers,
      },
      body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
    });
    await res.arrayBuffer();
    return res.status;
  }

  const hits = () => upstreams.map((u) => u.receivedRequests.length);

  async function statusConfig(): Promise<StatusConfig> {
    return (await (await fetch(`${app!.metricsUrl}/status/config`)).json()) as StatusConfig;
  }

  test("`strategy: weighted` keeps splitting traffic by weight", async (ctx) => {
    if (!etcdReachable || !seed) {
      ctx.skip();
      return;
    }
    await seed.createModel({
      display_name: "stored-weighted-group",
      routing: {
        strategy: "weighted",
        targets: [
          { model: "stored-routing-target-0", weight: 3 },
          { model: "stored-routing-target-1", weight: 1 },
        ],
      },
    });
    await waitConfigPropagation(async () => (await chat("stored-weighted-group")) === 200);

    const before = hits();
    for (let i = 0; i < 40; i++) {
      expect(await chat("stored-weighted-group")).toBe(200);
    }
    const after = hits();
    const first = after[0]! - before[0]!;
    const second = after[1]! - before[1]!;
    expect(first + second).toBe(40);
    expect(second).toBeGreaterThan(0);
    expect(first).toBeGreaterThan(second);

    const cfg = await statusConfig();
    expect(cfg.rejected).toEqual([]);
    expect(
      cfg.partially_compatible.some(
        (p) => p.resource_kind === "models" && p.field.includes("weighted"),
      ),
    ).toBe(true);
  });

  test("`sticky: true` keeps a key on one target", async (ctx) => {
    if (!etcdReachable || !seed) {
      ctx.skip();
      return;
    }
    await seed.createModel({
      display_name: "stored-sticky-group",
      routing: {
        strategy: "weighted",
        sticky: true,
        targets: [
          { model: "stored-routing-target-0", weight: 1 },
          { model: "stored-routing-target-1", weight: 1 },
        ],
      },
    });
    await waitConfigPropagation(async () => (await chat("stored-sticky-group")) === 200);

    for (const key of ["cohort-a", "cohort-b", "cohort-c", "cohort-d"]) {
      const before = hits();
      for (let i = 0; i < 10; i++) {
        expect(await chat("stored-sticky-group", { "x-aisix-routing-key": key })).toBe(200);
      }
      const after = hits();
      const served = after.map((n, i) => n - before[i]!);
      expect([...served].sort((a, b) => a - b)).toEqual([0, 10]);
    }

    const cfg = await statusConfig();
    expect(cfg.rejected).toEqual([]);
    expect(
      cfg.partially_compatible.some(
        (p) => p.resource_kind === "models" && p.field.includes("sticky"),
      ),
    ).toBe(true);
  });
});

describe("a resources file using a retired routing setting", () => {
  test("is refused at boot, naming what to write instead", async () => {
    const upstream = await startOpenAiUpstream();
    try {
      await expect(
        spawnApp({
          resourcesFile: `
_format_version: "1"
provider_keys:
  - display_name: stored-routing-file-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstream.baseUrl}/v1
models:
  - display_name: stored-routing-file-target
    provider: openai
    model_name: gpt-4o-mini
    provider_key: stored-routing-file-pk
  - display_name: stored-routing-file-group
    routing:
      strategy: weighted
      targets:
        - model: stored-routing-file-target
`,
        }),
      ).rejects.toThrow(/round_robin/);
    } finally {
      await upstream.close();
    }
  });
});
