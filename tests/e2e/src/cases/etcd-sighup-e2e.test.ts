import { spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { stringify as yamlStringify } from "yaml";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  pickFreePorts,
  spawnApp,
  startOpenAiUpstream,
  suiteThreadPerCore,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const BIN_PATH =
  process.env.AISIX_BIN ?? join(process.cwd(), "..", "..", "target", "debug", "aisix");

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

// The same signal while the gateway is still starting up. A gateway whose
// etcd requires authentication dials etcd during startup — it must log in
// before it can read anything — so an etcd that accepts the connection and
// never answers holds the gateway there, not yet serving: the shape of a
// cluster that is slow or unreachable while the gateway boots. A SIGHUP in
// that window must not terminate the process either. This case spawns the
// binary itself: the shared harness waits for a gateway that serves.
describe("etcd resource source: SIGHUP during startup", () => {
  let child: ChildProcess | undefined;
  let etcd: Server | undefined;
  const sockets: Socket[] = [];
  let dir: string | undefined;

  afterAll(async () => {
    if (child && child.exitCode === null && child.signalCode === null) {
      const exited = new Promise<void>((resolve) => child?.once("exit", () => resolve()));
      child.kill("SIGKILL");
      await exited;
    }
    for (const socket of sockets) socket.destroy();
    await new Promise<void>((resolve) => (etcd ? etcd.close(() => resolve()) : resolve()));
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  test("a SIGHUP while the gateway waits for etcd does not terminate it", async () => {
    // An etcd endpoint that accepts connections and never answers.
    let dialed!: () => void;
    const etcdDialed = new Promise<void>((resolve) => (dialed = resolve));
    etcd = createServer((socket) => {
      sockets.push(socket);
      dialed();
    });
    const server = etcd;
    const etcdPort = await new Promise<number>((resolve) =>
      server.listen(0, "127.0.0.1", () => resolve((server.address() as AddressInfo).port)),
    );

    const [proxyPort, adminPort, metricsPort] = await pickFreePorts(3);
    dir = await mkdtemp(join(tmpdir(), "aisix-sighup-boot-"));
    const passwordEnv = "SIGHUP_E2E_ETCD_PASSWORD";
    const cfg = {
      etcd: {
        endpoints: [`http://127.0.0.1:${etcdPort}`],
        prefix: "/aisix-sighup-boot",
        user: "e2e",
        password_env: passwordEnv,
        dial_timeout_ms: 5000,
        request_timeout_ms: 5000,
      },
      proxy: {
        addr: `127.0.0.1:${proxyPort}`,
        // This case spawns the binary itself, so it honours the suite-wide
        // serving mode the shared harness applies.
        ...(suiteThreadPerCore !== undefined ? { thread_per_core: suiteThreadPerCore } : {}),
      },
      admin: { addr: `127.0.0.1:${adminPort}`, enabled: false },
      observability: {
        service_name: "aisix-sighup-boot",
        log_level: "warn",
        metrics: {
          prometheus: { enabled: true, path: "/metrics", addr: `127.0.0.1:${metricsPort}` },
          otlp: { enabled: false, endpoint: "http://127.0.0.1:4317" },
        },
        tracing: { otlp: { enabled: false, endpoint: "http://127.0.0.1:4317", sample_ratio: 1 } },
      },
      cache: { backend: "memory" },
    };
    const cfgPath = join(dir, "config.yaml");
    await writeFile(cfgPath, yamlStringify(cfg), "utf8");

    // Strip AISIX_* so the harness's own env can't override the config.
    const childEnv: Record<string, string> = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (v !== undefined && !k.startsWith("AISIX_")) childEnv[k] = v;
    }
    childEnv.RUST_LOG = "warn";
    childEnv[passwordEnv] = "e2e-password";

    let output = "";
    const gateway = spawn(BIN_PATH, ["--config", cfgPath], {
      stdio: ["ignore", "pipe", "pipe"],
      env: childEnv,
    });
    child = gateway;
    gateway.stdout?.on("data", (c: Buffer) => (output += c.toString("utf8")));
    gateway.stderr?.on("data", (c: Buffer) => (output += c.toString("utf8")));
    const exited = new Promise<string>((resolve) =>
      gateway.once("exit", (code, signal) => resolve(`exited (code=${code} signal=${signal})`)),
    );

    // The gateway reaches etcd during startup.
    const reached = await Promise.race([
      etcdDialed.then(() => "dialed"),
      exited,
      sleep(20_000).then(() => "etcd never dialed within 20s"),
    ]);
    expect(reached, output).toBe("dialed");
    // And it is still starting up: nothing serves on the proxy port yet.
    await expect(
      fetch(`http://127.0.0.1:${proxyPort}/livez`),
      "the gateway was already serving; this case needs it mid-startup",
    ).rejects.toThrow();

    gateway.kill("SIGHUP");
    const outcome = await Promise.race([
      exited,
      sleep(SURVIVAL_WINDOW_MS).then(() => "running"),
    ]);
    expect(outcome, `the gateway exited on a SIGHUP during startup:\n${output}`).toBe("running");
  }, 40_000);
});
