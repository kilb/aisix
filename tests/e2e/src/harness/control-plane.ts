import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  assertGatewayBinaryIsFresh,
  BIN_PATH,
  isAddrInUseStartupFailure,
  recordProfileOwner,
  stopGateway,
  suiteThreadPerCore,
} from "./app.js";
import { harnessRequest } from "./http.js";
import { pickFreePorts } from "./ports.js";

/**
 * The real control plane, with real kine and Postgres, for the `*.cp.ts`
 * cases. It is `scripts/cp-dev-stack.mjs` (the one implementation of the
 * stack, which the Dashboard's Playwright suites and developers run too),
 * spawned without a gateway: a case issues its gateways' credentials itself
 * and boots each one from the bundle with `spawnManagedGateway`.
 *
 * Everything is seeded through the control plane's API. kine does not serve
 * etcd's JSON gateway, so `harness/etcd.ts` cannot reach it, and a document
 * that did not go through the control plane is not what a gateway in this
 * deployment would ever see. Every upstream account a tenant key must reach
 * carries `groups: ["shared"]`: a new tenant is confined to the default
 * provider groups, and the first boot's default is `["shared"]`.
 */

const REPO = fileURLToPath(new URL("../../../../", import.meta.url));
const STACK_SCRIPT = join(REPO, "scripts", "cp-dev-stack.mjs");
/** The file the official image bakes at `/etc/aisix/config.managed.yaml`. */
const MANAGED_CONFIG = join(REPO, "config.managed.yaml");
/** Above the stack's own readiness budget (60 s), which covers a cold start. */
const STACK_READY_MS = 90_000;
const GATEWAY_READY_MS = 20_000;

// ── logs kept for a failed run ───────────────────────────────────────────────

/**
 * Where the stacks and gateways a run started leave their output when they
 * stop, for CI to upload when a leg fails (`CP_E2E_LOG_DIR`); nothing is
 * kept when it is unset. A case that fails mid-run leaves no other trace of
 * what the control plane, kine and the gateway said.
 */
const LOG_DIR = process.env.CP_E2E_LOG_DIR;

/**
 * `text` with what could be a secret taken out: a URL's userinfo (the
 * database URL's password), a minted caller key, and each of `secrets`.
 */
function scrubbed(text: string, secrets: string[]): string {
  let out = text
    .replace(/([a-z][a-z0-9+.-]*:\/\/)[^/@\s]+@/gi, "$1***@")
    .replace(/sk-aisix-[0-9a-f]{16,}/g, "sk-aisix-***");
  for (const secret of secrets) {
    if (secret) out = out.split(secret).join("***");
  }
  return out;
}

async function keepLog(name: string, text: string, secrets: string[] = []): Promise<void> {
  if (!LOG_DIR) return;
  await mkdir(LOG_DIR, { recursive: true });
  await writeFile(join(LOG_DIR, `${name}.log`), scrubbed(text, secrets));
}

// ── HTTP clients ────────────────────────────────────────────────────────────

/** Bodies are JSON whose shape each case asserts on. */
export type Json = any;

export interface CpResponse {
  status: number;
  body: Json;
  text: string;
  /** The `ETag` header, unquoted, when the answer carries one. */
  version: number | undefined;
  headers: Record<string, string | string[] | undefined>;
}

class CpClient {
  protected cookie: string | undefined;

  constructor(
    readonly baseUrl: string,
    private readonly cookieName: string,
  ) {}

  protected async request(
    method: string,
    path: string,
    body?: unknown,
    headers: Record<string, string> = {},
  ): Promise<CpResponse> {
    const res = await harnessRequest(`${this.baseUrl}${path}`, {
      method,
      // Labelled JSON with or without a body, as the Dashboard sends every
      // request: the control plane refuses a mutating one that is not.
      headers: {
        ...(this.cookie ? { cookie: this.cookie } : {}),
        "content-type": "application/json",
        ...headers,
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.body.text();
    const setCookie = res.headers["set-cookie"];
    for (const line of Array.isArray(setCookie) ? setCookie : setCookie ? [setCookie] : []) {
      const pair = line.split(";")[0];
      if (pair.startsWith(`${this.cookieName}=`)) this.cookie = pair;
    }
    const etag = res.headers.etag?.toString().replace(/"/g, "");
    let parsed: Json = null;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = text;
      }
    }
    return {
      status: res.statusCode,
      body: parsed,
      text,
      version: etag === undefined ? undefined : Number(etag),
      headers: res.headers,
    };
  }

  get(path: string): Promise<CpResponse> {
    return this.request("GET", path);
  }

  post(path: string, body: unknown = {}): Promise<CpResponse> {
    return this.request("POST", path, body);
  }

  put(path: string, body: unknown, version: number): Promise<CpResponse> {
    return this.request("PUT", path, body, { "if-match": `"${version}"` });
  }

  patch(path: string, body: unknown, version?: number): Promise<CpResponse> {
    return this.request(
      "PATCH",
      path,
      body,
      version === undefined ? {} : { "if-match": `"${version}"` },
    );
  }

  del(path: string, version?: number): Promise<CpResponse> {
    return this.request(
      "DELETE",
      path,
      undefined,
      version === undefined ? {} : { "if-match": `"${version}"` },
    );
  }
}

/** An operator's session on `/admin/*`. */
export class CpAdmin extends CpClient {
  constructor(baseUrl: string) {
    super(baseUrl, "aisix_cp_admin");
  }

  async login(email: string, password: string): Promise<CpResponse> {
    const res = await this.post("/admin/login", { email, password });
    if (res.status !== 200) throw new Error(`operator login answered ${res.status}: ${res.text}`);
    return res;
  }

  /** Creates a gateway resource through the generic API; returns its view. */
  async create(kind: string, doc: Record<string, unknown>): Promise<Json> {
    const res = await this.post(`/admin/resources/${kind}`, { doc });
    if (res.status !== 201) throw new Error(`creating ${kind} answered ${res.status}: ${res.text}`);
    return res.body;
  }

  /** The gateway instance running under credential `dpId`, if it has reported. */
  async instance(dpId: string): Promise<Json | undefined> {
    const res = await this.get("/admin/gateways/instances");
    if (res.status !== 200) throw new Error(`listing instances answered ${res.status}: ${res.text}`);
    return (res.body.items as Json[]).find((i) => i.dp_id === dpId);
  }

  /**
   * Waits until the gateway under credential `dpId` has sent a heartbeat.
   * A case calls it before writing a key: a control plane with a release
   * stamp judges such writes by the versions registered gateways report,
   * and passes them unjudged while none has.
   */
  async waitRegistered(dpId: string): Promise<Json> {
    return pollUntil(`the gateway under ${dpId} to register`, () => this.instance(dpId));
  }

  /** Polls the resource until its `rollout.state` is `applied`; returns its view. */
  async waitApplied(kind: string, id: string, timeoutMs = 30_000): Promise<Json> {
    return waitRolloutApplied(`${kind}/${id}`, () => this.get(`/admin/resources/${kind}/${id}`), timeoutMs);
  }

  /** Polls the tenant until its aggregated `rollout.state` is `applied`; returns its view. */
  async waitTenantApplied(tenantId: string, timeoutMs = 30_000): Promise<Json> {
    return waitRolloutApplied(`tenant ${tenantId}`, () => this.get(`/admin/tenants/${tenantId}`), timeoutMs);
  }
}

async function waitRolloutApplied(
  what: string,
  read: () => Promise<CpResponse>,
  timeoutMs: number,
): Promise<Json> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown = "no read";
  for (;;) {
    const res = await read();
    last = res.status === 200 ? res.body.rollout : `HTTP ${res.status}: ${res.text}`;
    if (res.status === 200 && res.body.rollout?.state === "applied") return res.body;
    if (Date.now() >= deadline) {
      throw new Error(`${what} was not applied within ${timeoutMs} ms; last rollout: ${JSON.stringify(last)}`);
    }
    await sleep(250);
  }
}

/** A member's session on `/api/*`. */
export class CpPortal extends CpClient {
  constructor(baseUrl: string) {
    super(baseUrl, "aisix_cp_portal");
  }

  signup(org: string, email: string, password: string): Promise<CpResponse> {
    return this.post("/api/signup", { org_name: org, email, password });
  }

  login(email: string, password: string): Promise<CpResponse> {
    return this.post("/api/login", { email, password });
  }
}

// ── the stack ───────────────────────────────────────────────────────────────

/** A gateway credential the control plane issued, ready to boot a gateway from. */
export interface IssuedGateway {
  name: string;
  dpId: string;
  /** The issue response's `env`, verbatim. */
  bundleEnv: Record<string, string>;
  /**
   * Where the bundle points the gateway, as `managed` keys. A case that wants
   * a link to go elsewhere (through a relay) overrides one in
   * `spawnManagedGateway`'s `managed`.
   */
  managed: { cp_base_url: string; cp_etcd_endpoint: string };
  /**
   * What this machine needs on top of the bundle: the paths the image keeps
   * under `/var/lib/aisix`, which are not writable here, and a heartbeat
   * every 5 s instead of 15 s so the control plane sees a change sooner.
   */
  extraEnv: Record<string, string>;
}

export interface ControlPlaneStack {
  httpUrl: string;
  dpUrl: string;
  envId: string;
  etcdPrefix: string;
  /** Logged in as the stack's first admin operator. */
  admin: CpAdmin;
  /** A fresh, logged-out member client. */
  portal(): CpPortal;
  issueGateway(name?: string): Promise<IssuedGateway>;
  /** The stack's own output and every child's, line-prefixed. */
  output(): string;
  stop(): Promise<void>;
}

interface ReadyLine {
  ready: true;
  httpUrl: string;
  dpUrl: string;
  envId: string;
  kineEndpoint: string;
  etcdPrefix: string;
  operator: { email: string; password: string };
}

/** The environment a child of the harness gets: nothing the gateway reads as its own config. */
function childEnv(): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v === undefined || k.startsWith("AISIX_")) continue;
    if (/^(https?|all)_proxy$/i.test(k)) continue;
    env[k] = v;
  }
  env.NO_PROXY = "127.0.0.1,localhost";
  env.no_proxy = "127.0.0.1,localhost";
  return env;
}

export async function startControlPlane(opts: { etcdPrefix?: string } = {}): Promise<ControlPlaneStack> {
  const [httpPort, dpPort, kinePort] = await pickFreePorts(3);
  const args = [
    STACK_SCRIPT,
    "--http-port",
    String(httpPort),
    "--dp-port",
    String(dpPort),
    "--kine-port",
    String(kinePort),
    ...(opts.etcdPrefix === undefined ? [] : ["--etcd-prefix", opts.etcdPrefix]),
  ];
  const child = spawn(process.execPath, args, {
    cwd: REPO,
    env: childEnv(),
    stdio: ["pipe", "pipe", "pipe"],
  });
  let log = "";
  child.stderr!.setEncoding("utf8");
  child.stderr!.on("data", (chunk: string) => (log += chunk));
  const exited = new Promise<number | null>((resolve) => child.once("exit", (code) => resolve(code)));

  const ready = await new Promise<ReadyLine>((resolve, reject) => {
    let pending = "";
    const timer = setTimeout(() => {
      reject(new Error(`the control-plane stack was not ready within ${STACK_READY_MS} ms`));
    }, STACK_READY_MS);
    child.stdout!.setEncoding("utf8");
    child.stdout!.on("data", (chunk: string) => {
      pending += chunk;
      const lines = pending.split("\n");
      pending = lines.pop() ?? "";
      for (const line of lines) {
        try {
          const parsed = JSON.parse(line) as ReadyLine;
          if (parsed.ready === true) {
            clearTimeout(timer);
            resolve(parsed);
          }
        } catch {
          // Not the ready line.
        }
      }
    });
    void exited.then((code) => {
      clearTimeout(timer);
      reject(new Error(`the control-plane stack exited (code ${code}) before it was ready`));
    });
  }).catch(async (e: Error) => {
    await stopStack(child, exited);
    await keepLog(`cp-stack-${httpPort}`, log);
    throw new Error(`${e.message}\n--- stack output ---\n${log}`);
  });

  const admin = new CpAdmin(ready.httpUrl);
  await admin.login(ready.operator.email, ready.operator.password);
  const gatewayDirs: string[] = [];

  return {
    httpUrl: ready.httpUrl,
    dpUrl: ready.dpUrl,
    envId: ready.envId,
    etcdPrefix: ready.etcdPrefix,
    admin,
    portal: () => new CpPortal(ready.httpUrl),
    output: () => log,
    async issueGateway(name = `gw-${randomUUID().slice(0, 8)}`): Promise<IssuedGateway> {
      const res = await admin.post("/admin/gateways/credentials", { name });
      if (res.status !== 201) throw new Error(`issuing a credential answered ${res.status}: ${res.text}`);
      const dir = await mkdtemp(join(tmpdir(), "aisix-cp-gw-"));
      gatewayDirs.push(dir);
      const bundleEnv = res.body.env as Record<string, string>;
      return {
        name,
        dpId: res.body.dp_id,
        bundleEnv,
        managed: {
          cp_base_url: bundleEnv.AISIX_MANAGED__CP_BASE_URL,
          cp_etcd_endpoint: bundleEnv.AISIX_MANAGED__CP_ETCD_ENDPOINT,
        },
        // Mirrors the local overrides in scripts/cp-dev-stack.mjs; the
        // listener addresses are picked per spawn.
        extraEnv: {
          AISIX_MANAGED__MTLS_DIR: join(dir, "mtls"),
          AISIX_MANAGED__DP_ID_FILE: join(dir, "dp_id"),
          AISIX_MANAGED__SNAPSHOT_CACHE_PATH: join(dir, "config_cache.json"),
          AISIX_MANAGED__HEARTBEAT_INTERVAL_SECS: "5",
          AISIX_OBSERVABILITY__DEBUG__ENABLED: "false",
          AISIX_OBSERVABILITY__HEAP_PROFILING__AUTO_DUMP__DIR: join(dir, "heap"),
          AISIX_SHUTDOWN__MIN_DRAIN_SECS: "0",
        },
      };
    },
    async stop() {
      await stopStack(child, exited);
      await keepLog(`cp-stack-${ready.envId}`, log, [ready.operator.password]);
      await Promise.allSettled(gatewayDirs.map((d) => rm(d, { recursive: true, force: true })));
    },
  };
}

/** Closing stdin is the stack's stop signal; SIGTERM, then SIGKILL, if it does not take. */
async function stopStack(child: ChildProcess, exited: Promise<number | null>): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.stdin?.end();
  const settle = (ms: number) => Promise.race([exited.then(() => true), sleep(ms).then(() => false)]);
  if (await settle(60_000)) return;
  child.kill("SIGTERM");
  if (await settle(20_000)) return;
  child.kill("SIGKILL");
  await exited;
}

// ── a gateway booted from the bundle ─────────────────────────────────────────

export interface ManagedGatewayOptions {
  /** Overrides of the bundle's `managed` keys, e.g. `cp_base_url` through a relay. */
  managed?: Partial<IssuedGateway["managed"]> & { heartbeat_interval_secs?: number };
  /** More environment, applied last. */
  env?: Record<string, string>;
  /** Wait for `/livez` and the metrics listener. Defaults to true. */
  awaitReady?: boolean;
}

export interface ManagedGateway {
  proxyUrl: string;
  metricsUrl: string;
  pid: number;
  output(): string;
  /** The exit code once the process has exited, `null` while it runs or after a signal. */
  exitCode(): number | null;
  /** Resolves with the exit code when the process exits on its own; rejects after `timeoutMs`. */
  waitForExit(timeoutMs: number): Promise<number | null>;
  exit(): Promise<void>;
}

/**
 * Boot the gateway as the official image does: `--config` the managed
 * bootstrap file, configured by the bundle's environment (minus
 * `AISIX_CONFIG_PATH`, which only the image's entrypoint reads) and the
 * credential's `extraEnv`, plus this spawn's listener addresses and the
 * suite's serving mode.
 */
export async function spawnManagedGateway(
  gw: IssuedGateway,
  opts: ManagedGatewayOptions = {},
): Promise<ManagedGateway> {
  for (let attempt = 1; ; attempt++) {
    try {
      return await spawnManagedGatewayOnce(gw, opts);
    } catch (err) {
      if (attempt < 3 && isAddrInUseStartupFailure(err)) continue;
      throw err;
    }
  }
}

async function spawnManagedGatewayOnce(
  gw: IssuedGateway,
  opts: ManagedGatewayOptions,
): Promise<ManagedGateway> {
  const [proxyPort, metricsPort] = await pickFreePorts(2);
  const env = childEnv();
  env.RUST_LOG = process.env.RUST_LOG ?? "warn";
  for (const [k, v] of Object.entries(gw.bundleEnv)) {
    if (k !== "AISIX_CONFIG_PATH") env[k] = v;
  }
  Object.assign(env, gw.extraEnv, {
    AISIX_PROXY__ADDR: `127.0.0.1:${proxyPort}`,
    AISIX_OBSERVABILITY__METRICS__PROMETHEUS__ADDR: `127.0.0.1:${metricsPort}`,
  });
  if (suiteThreadPerCore !== undefined) env.AISIX_PROXY__THREAD_PER_CORE = String(suiteThreadPerCore);
  for (const [k, v] of Object.entries(opts.managed ?? {})) {
    if (v !== undefined) env[`AISIX_MANAGED__${k.toUpperCase()}`] = String(v);
  }
  Object.assign(env, opts.env ?? {});

  assertGatewayBinaryIsFresh(BIN_PATH);
  const child = spawn(BIN_PATH, ["--config", MANAGED_CONFIG], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  recordProfileOwner(child.pid);
  let out = "";
  child.stdout!.on("data", (c: Buffer) => (out += c.toString("utf8")));
  child.stderr!.on("data", (c: Buffer) => (out += c.toString("utf8")));
  const exited = new Promise<number | null>((resolve) => child.once("exit", (code) => resolve(code)));

  const proxyUrl = `http://127.0.0.1:${proxyPort}`;
  const metricsUrl = `http://127.0.0.1:${metricsPort}`;
  if (opts.awaitReady ?? true) {
    const early = exited.then((code) => {
      throw new Error(`aisix exited early with code=${code}`);
    });
    try {
      await Promise.race([
        Promise.all([
          waitForOk(`${proxyUrl}/livez`, GATEWAY_READY_MS),
          waitForOk(`${metricsUrl}/metrics`, GATEWAY_READY_MS),
        ]),
        early,
      ]);
    } catch (err) {
      await stopGateway(child);
      throw new Error(`${(err as Error).message}\n  stderr:\n${out}`);
    }
    early.catch(() => {});
  }

  return {
    proxyUrl,
    metricsUrl,
    pid: child.pid!,
    output: () => out,
    exitCode: () => child.exitCode,
    waitForExit(timeoutMs: number) {
      return Promise.race([
        exited,
        sleep(timeoutMs).then(() => {
          throw new Error(`aisix did not exit within ${timeoutMs} ms\n  stderr:\n${out}`);
        }),
      ]);
    },
    async exit() {
      await stopGateway(child);
      // The bundle's two secrets: the gateway's private key and the
      // platform-mode id key.
      const secrets = [gw.bundleEnv.AISIX_MANAGED__CP_KEY_PEM, gw.bundleEnv.PLATFORM_ID_KEY];
      await keepLog(`gateway-${gw.name}-${child.pid}`, out, secrets);
    },
  };
}

async function waitForOk(url: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let last = "no attempt";
  while (Date.now() < deadline) {
    try {
      const res = await harnessRequest(url);
      await res.body.dump();
      if (res.statusCode === 200) return;
      last = `HTTP ${res.statusCode}`;
    } catch (err) {
      last = (err as Error).message;
    }
    await sleep(100);
  }
  throw new Error(`timed out waiting for ${url} after ${timeoutMs} ms: ${last}`);
}

// ── waiting ─────────────────────────────────────────────────────────────────

/**
 * Polls `probe` until it returns something other than `undefined`, and
 * returns that. On timeout it throws with `what` and the last value `describe`
 * gives, which is the diagnostic a case has when the condition never holds.
 */
export async function pollUntil<T>(
  what: string,
  probe: () => Promise<T | undefined>,
  opts: { timeoutMs?: number; intervalMs?: number; describe?: () => string } = {},
): Promise<T> {
  const timeoutMs = opts.timeoutMs ?? 30_000;
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await probe();
    if (value !== undefined) return value;
    if (Date.now() >= deadline) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}${opts.describe ? `: ${opts.describe()}` : ""}`);
    }
    await sleep(opts.intervalMs ?? 250);
  }
}

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/**
 * Funds `tenantId` so that its model calls are not refused for want of
 * money, through the operator API: the tenant becomes postpaid with a credit
 * limit of `creditLimitUsd` dollars. A prepaid tenant an operator creates
 * holds nothing, and a prepaid tenant at 0 is refused at the gateway.
 *
 * The credit limit is settled before the answer, but the gateway learns of
 * it with the next documents written for the tenant: a case calls this
 * before it mints the tenant's key, whose authenticating is its readiness
 * gate.
 */
export async function fundTenant(cp: ControlPlaneStack, tenantId: string, creditLimitUsd: string): Promise<void> {
  const current = await cp.admin.get(`/admin/tenants/${tenantId}`);
  if (current.status !== 200) throw new Error(`reading tenant ${tenantId} answered ${current.status}: ${current.text}`);
  const mode = await cp.admin.patch(`/admin/tenants/${tenantId}`, { billing_mode: "postpaid" }, current.version);
  if (mode.status !== 200) throw new Error(`making tenant ${tenantId} postpaid answered ${mode.status}: ${mode.text}`);
  const limit = await cp.admin.patch(`/admin/tenants/${tenantId}/billing`, { credit_limit_usd: creditLimitUsd });
  if (limit.status !== 200) throw new Error(`setting the credit limit of ${tenantId} answered ${limit.status}: ${limit.text}`);
}
