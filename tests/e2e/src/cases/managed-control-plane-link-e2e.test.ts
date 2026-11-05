import { rmSync } from "node:fs";
import { createHash } from "node:crypto";
import { afterEach, describe, expect, test } from "vitest";
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

// E2E: the link between a managed gateway and its control plane.
//
// It carries the usage records billing runs on and per-user credentials, so two
// things about it are not negotiable. It is verified against the control plane's
// own CA, whatever an operator decided for one model endpoint. And it exists
// whenever the gateway is serving: a restart that loses a small sidecar file
// must not quietly turn into a gateway that meters nothing.

const HEARTBEAT = "/dp/heartbeat";
const TELEMETRY = "/dp/telemetry";
const CALLER = "sk-control-plane-link";

describe("a managed gateway and its control plane", () => {
  const apps: SpawnedApp[] = [];
  let cp: MockControlPlane | undefined;
  let upstream: OpenAiUpstream | undefined;

  afterEach(async () => {
    for (const app of apps.splice(0)) await app.exit();
    await cp?.close();
    await upstream?.close();
    cp = undefined;
    upstream = undefined;
  });

  async function boot(managed: Record<string, unknown>, extra: Record<string, unknown> = {}) {
    const app = await spawnApp({
      extra: { managed: { ...managed, heartbeat_interval_secs: 1 }, ...extra },
      awaitProxyListener: false,
    });
    apps.push(app);
    return app;
  }

  test("upstream.tls.verify: false does not extend to the control plane", async (ctx) => {
    if (!(await new EtcdClient().ping())) return ctx.skip();

    // Someone else's certificate on the REST surface: nothing may be sent there.
    cp = await startMockControlPlane({ restCertFromForeignCa: true });
    await boot(cp.managed, { upstream: { tls: { verify: false } } });
    await new Promise((r) => setTimeout(r, 3_000));
    expect(cp.calls).toEqual([]);
  }, 30_000);

  test("with upstream.tls.verify: false, a control plane it can verify still hears from it", async (ctx) => {
    if (!(await new EtcdClient().ping())) return ctx.skip();

    cp = await startMockControlPlane();
    const plane = cp;
    await boot(plane.managed, { upstream: { tls: { verify: false } } });
    await waitConfigPropagation(async () => plane.calls.includes(HEARTBEAT), 15_000);
  }, 30_000);

  test("a restart that lost the dp_id file keeps reporting usage", async (ctx) => {
    const etcd = new EtcdClient();
    if (!(await etcd.ping())) return ctx.skip();

    upstream = await startOpenAiUpstream();
    cp = await startMockControlPlane();
    const plane = cp;
    const first = await boot(plane.managed);
    await waitConfigPropagation(async () => plane.calls.includes(HEARTBEAT), 15_000);
    await first.exit();
    apps.splice(apps.indexOf(first), 1);

    // The bundle survived the restart and the file beside it did not, and the
    // certificates are no longer supplied: this boot reuses what is on disk.
    rmSync(String(plane.managed.dp_id_file));
    const { cp_cert_file: _c, cp_key_file: _k, cp_ca_file: _a, ...reused } = plane.managed;
    plane.calls.length = 0;
    const app = await boot(reused);

    const seed = new SeedClient(etcd, `${app.etcdPrefix}/${plane.envId}`);
    const pk = await seed.createProviderKey({
      display_name: "cp-link-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "cp-link-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      allowed_models: ["*"],
    });

    const client = () => new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => {
      try {
        return (await client().listModels()).status === 200;
      } catch {
        // The proxy listener binds once the first configuration applies.
        return false;
      }
    });
    const res = await client().chat({
      model: "cp-link-model",
      messages: [{ role: "user", content: "hi" }],
    });
    expect(res.status).toBe(200);

    await waitConfigPropagation(async () => plane.calls.includes(HEARTBEAT), 15_000);
    await waitConfigPropagation(async () => plane.calls.includes(TELEMETRY), 15_000);
  }, 60_000);
});
