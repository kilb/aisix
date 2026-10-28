import { createHash } from "node:crypto";
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

// E2E: a gateway reading its resources from etcd has no file to reload, so
// SIGHUP has nothing to do there — but it must not take the gateway down.
// Stray hang-ups are ordinary on a host: a terminal session closing, or a
// co-located process broadcasting `pkill -HUP` to every gateway on the
// machine to reload the file-mode ones. A managed gateway that dies on one
// drops every in-flight and subsequent request until something restarts it.
//
// Pinned contract: after SIGHUP the process is still running and keeps
// serving requests from its etcd-sourced configuration, however many times
// the signal arrives.

const KEY = "sk-etcd-sighup-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");
// How long the process must stay up after a SIGHUP. The default action for
// SIGHUP terminates the process at once, so a short window is decisive.
const SURVIVAL_WINDOW_MS = 1_500;

describe("etcd resource source: SIGHUP", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "sighup-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sighup-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: sha256(KEY),
      allowed_models: ["sighup-model"],
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("the gateway survives SIGHUP and keeps serving", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const proxy = new ProxyClient(app.proxyUrl, KEY);
    const chat = () =>
      proxy.chat({
        model: "sighup-model",
        messages: [{ role: "user", content: "still there?" }],
      });
    await waitConfigPropagation(async () => (await chat()).status === 200);

    for (let round = 1; round <= 2; round++) {
      app.signal("SIGHUP");
      await expect(
        app.waitForExit(SURVIVAL_WINDOW_MS),
        `the gateway exited on SIGHUP #${round}`,
      ).rejects.toThrow(/did not exit/);

      const baseline = upstream.receivedRequests.length;
      const res = await chat();
      expect(res.status, `chat after SIGHUP #${round}: ${JSON.stringify(res.body)}`).toBe(200);
      const forwarded = upstream.receivedRequests
        .slice(baseline)
        .filter((r) => r.path === "/v1/chat/completions");
      expect(forwarded).toHaveLength(1);
    }
  }, 30_000);
});
