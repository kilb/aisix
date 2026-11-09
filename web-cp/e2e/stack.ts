import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";

/**
 * The real stack for the Dashboard's e2e: `scripts/cp-dev-stack.mjs` (two
 * fresh Postgres databases, kine, the control plane with an admin operator,
 * a managed gateway under the credential `stack-gw`, and the upstream
 * stand-in), plus `vite preview` serving the built `dist/` with `/admin`
 * proxied to that control plane.
 *
 * Nothing here stands in for the control plane: setup goes through its real
 * `/admin` and `/api`. The only stand-in is the upstream provider the
 * gateway calls.
 *
 * Needs, in the environment: `CP_E2E_PG_URL` (a Postgres URL whose role may
 * CREATE DATABASE) and `CP_E2E_KINE_BIN` (or `kine` on PATH). Needs, built:
 * `target/debug/aisix` and `target/debug/aisix-control-plane` (the stack
 * refuses a control plane older than its sources), and `dist/` (`npm run
 * build`).
 */

/** One spec file's `beforeAll` budget: a cold stack plus `vite preview`. */
export const STACK_START_TIMEOUT_MS = 180_000;

/** Above the stack's own readiness budget (60 s per step). */
const STACK_READY_MS = 150_000;
const PREVIEW_READY_MS = 30_000;
const STOP_GRACE_MS = 30_000;

const WEB_CP = process.cwd();

/** A JSON body whose shape each test asserts on. */
export type Json = any;

export interface ApiAnswer {
  status: number;
  body: Json;
  text: string;
  /** The `ETag` version, when the answer carries one. */
  version: number | null;
}

/**
 * A cookie-holding `fetch` client against the control plane's browser
 * listener, for setup through the real `/admin` and `/api`. `request` returns
 * any answer; `get`, `post`, `put`, `patch` and `del` return the parsed body
 * and throw unless the status is 2xx. `ifMatch` sends `If-Match: "<n>"`.
 */
export class ApiClient {
  private readonly cookies = new Map<string, string>();

  constructor(readonly baseUrl: string) {}

  async request(method: string, path: string, body?: unknown, ifMatch?: number): Promise<ApiAnswer> {
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (ifMatch !== undefined) headers["if-match"] = `"${ifMatch}"`;
    if (this.cookies.size > 0) {
      headers.cookie = [...this.cookies].map(([k, v]) => `${k}=${v}`).join("; ");
    }
    const res = await fetch(`${this.baseUrl}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    for (const line of res.headers.getSetCookie()) {
      const pair = line.split(";")[0] ?? "";
      const eq = pair.indexOf("=");
      if (eq <= 0) continue;
      const name = pair.slice(0, eq).trim();
      const value = pair.slice(eq + 1).trim();
      // A cleared session comes back empty with `Max-Age=0`.
      if (value === "" || /;\s*max-age=0\b/i.test(line)) this.cookies.delete(name);
      else this.cookies.set(name, value);
    }
    const text = await res.text();
    let parsed: Json = null;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = text;
      }
    }
    const etag = res.headers.get("etag")?.replace(/^W\//, "").replace(/"/g, "");
    return { status: res.status, body: parsed, text, version: etag ? Number(etag) : null };
  }

  private async ok(method: string, path: string, body?: unknown, ifMatch?: number): Promise<Json> {
    const answer = await this.request(method, path, body, ifMatch);
    if (answer.status < 200 || answer.status >= 300) {
      throw new Error(`${method} ${path} answered ${answer.status}: ${answer.text}`);
    }
    return answer.body;
  }

  get(path: string): Promise<Json> {
    return this.ok("GET", path);
  }

  post(path: string, body: unknown = {}): Promise<Json> {
    return this.ok("POST", path, body);
  }

  put(path: string, body: unknown, version?: number): Promise<Json> {
    return this.ok("PUT", path, body, version);
  }

  /** A live provider group named `name`; its view. */
  createGroup(name: string, displayName: string, cnyPerUsd = "1"): Promise<Json> {
    return this.post("/admin/provider-groups", {
      name,
      display_name: displayName,
      description: "e2e 分组",
      default_cny_per_usd: cnyPerUsd,
    });
  }

  /**
   * Every group in `names` a live one, created at the default rate unless it
   * already is: a fixture that lists a group in an account, a tenant or the
   * defaults names a live one first, and one a spec here creates itself is
   * skipped.
   */
  async seedGroups(names: string[]): Promise<void> {
    const listed: Json = await this.get("/admin/provider-groups");
    const live = new Set<string>(listed.items.map((g: Json) => g.name as string));
    for (const name of names) {
      if (live.has(name)) continue;
      await this.createGroup(name, name);
      live.add(name);
    }
  }

  patch(path: string, body: unknown, version?: number): Promise<Json> {
    return this.ok("PATCH", path, body, version);
  }

  del(path: string, version?: number): Promise<Json> {
    return this.ok("DELETE", path, undefined, version);
  }
}

/**
 * Funds a customer tenant with an operator credit, as `admin`: a prepaid
 * tenant at $0 is blocked by design, so a test that calls the gateway with
 * its keys needs a balance first.
 */
export async function fundTenant(admin: ApiClient, tenantId: string): Promise<void> {
  await admin.post(`/admin/tenants/${tenantId}/credits`, { amount_cny: "10.00", reason: "e2e funding" });
}

export interface Stack {
  /** The Dashboard, served from `dist/` by `vite preview`. */
  previewUrl: string;
  /** The control plane's browser listener (`/admin`, `/api`). */
  httpUrl: string;
  /** The managed gateway's proxy listener. */
  proxyUrl: string;
  /** The managed gateway's Prometheus listener (`<url>/metrics`). */
  gatewayMetricsUrl: string;
  /** Present with `startStack({ telemetryRelay: true })`: holds the gateway's link to the control plane. */
  relay?: { pause(): Promise<void>; resume(): Promise<void> };
  /** The upstream stand-in; an upstream account reaches it at `<url>/v1`. */
  mockUpstreamUrl: string;
  /** The stack's first operator, an admin. */
  operator: { email: string; password: string };
  /** A client signed in as `operator`. */
  admin: ApiClient;
  /** A fresh client with no session, for `/api` (sign up or log in first). */
  tenant(): ApiClient;
  /**
   * Sets a key's `group_id` back to `NULL`: the state an older control-plane
   * replica leaves behind, which no API write produces (every create names a
   * group) and which a spec that needs a legacy groupless key sets directly.
   */
  clearKeyGroup(id: string): void;
  stop(): Promise<void>;
}

interface ReadyLine {
  ready: true;
  httpUrl: string;
  /** The stack's own control-plane database, for [`sqlAgainst`]. */
  cpDatabase: string;
  operator: { email: string; password: string };
  gateway: { proxyUrl: string; metricsUrl: string; dpId: string; name: string };
  mockUpstream: { url: string };
  telemetryRelay?: { controlUrl: string };
}

/** `psql`, or what `CP_E2E_PSQL` names. */
const PSQL = process.env.CP_E2E_PSQL || "psql";

/**
 * One statement against the stack's control-plane database, for state no API
 * write produces. The stack names its database, never a credential: the URL
 * is built from the `CP_E2E_PG_URL` the run already holds.
 */
function sqlAgainst(database: string, statement: string): string {
  const pgUrl = process.env.CP_E2E_PG_URL;
  if (!pgUrl) throw new Error("CP_E2E_PG_URL is not set");
  const url = new URL(pgUrl);
  url.pathname = `/${database}`;
  const r = spawnSync(PSQL, [url.toString(), "-v", "ON_ERROR_STOP=1", "-qAtc", statement], {
    encoding: "utf8",
    timeout: 30_000,
  });
  if (r.error) throw new Error(`cannot run ${PSQL}: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`${PSQL} failed (${statement}): ${r.stderr.trim()}`);
  return r.stdout;
}

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const address = srv.address();
      if (address && typeof address === "object") {
        const port = address.port;
        srv.close(() => resolvePort(port));
      } else {
        reject(new Error("no port"));
      }
    });
  });
}

/**
 * `dist/` exists and is built for `/`. Without it every test would time out
 * on an empty page, which reads like a product bug.
 */
function assertBuilt(): void {
  const html = join(WEB_CP, "dist", "index.html");
  if (!existsSync(html)) throw new Error("no dist/: run `npm run build` first");
  if (!/(src|href)="\/assets\//.test(readFileSync(html, "utf8"))) {
    throw new Error("dist/ is not built for the root path: rerun `npm run build`");
  }
}

/** Stops a detached child's whole process group. */
function killGroup(child: ChildProcess, signal: NodeJS.Signals): void {
  if (child.pid === undefined || child.exitCode !== null || child.signalCode !== null) return;
  try {
    process.kill(-child.pid, signal);
  } catch {
    // Already gone.
  }
}

function exited(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((resolveExit) => child.once("exit", () => resolveExit()));
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function relayCall(controlUrl: string, action: "pause" | "resume"): Promise<void> {
  const res = await fetch(`${controlUrl}/__relay/${action}`, { method: "POST" });
  await res.arrayBuffer();
  if (res.status !== 204) throw new Error(`relay ${action} answered ${res.status}`);
}

/** Reads the stack's stdout until its ready line, racing its exit and a deadline. */
function readyLine(child: ChildProcess): Promise<ReadyLine> {
  return new Promise((resolveLine, reject) => {
    let pending = "";
    const timer = setTimeout(
      () => reject(new Error(`cp-dev-stack printed no ready line within ${STACK_READY_MS} ms`)),
      STACK_READY_MS,
    );
    const onExit = (code: number | null, signal: string | null) => {
      clearTimeout(timer);
      reject(new Error(`cp-dev-stack exited before it was ready (code=${code} signal=${signal}); see its output above`));
    };
    child.once("exit", onExit);
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      pending += chunk;
      const lines = pending.split("\n");
      pending = lines.pop() ?? "";
      for (const line of lines) {
        let parsed: unknown;
        try {
          parsed = JSON.parse(line);
        } catch {
          continue;
        }
        if (parsed && typeof parsed === "object" && (parsed as { ready?: unknown }).ready === true) {
          clearTimeout(timer);
          child.off("exit", onExit);
          resolveLine(parsed as ReadyLine);
          return;
        }
      }
    });
  });
}

async function waitServing(url: string): Promise<void> {
  const deadline = Date.now() + PREVIEW_READY_MS;
  for (;;) {
    try {
      const res = await fetch(url);
      await res.arrayBuffer();
      if (res.status < 500) return;
    } catch {
      // Not listening yet.
    }
    if (Date.now() >= deadline) throw new Error(`${url} did not answer within ${PREVIEW_READY_MS} ms`);
    await sleep(200);
  }
}

export async function startStack(opts: { telemetryRelay?: boolean } = {}): Promise<Stack> {
  assertBuilt();
  // stdin is held open: the stack stops when it closes, so a worker that
  // dies takes its databases with it. Its own progress and every child's
  // lines go to stderr, which is this worker's.
  const stackProc = spawn(
    process.execPath,
    ["../scripts/cp-dev-stack.mjs", "--gateway", "../target/debug/aisix", "--mock-upstream", ...(opts.telemetryRelay ? ["--telemetry-relay"] : [])],
    { cwd: WEB_CP, stdio: ["pipe", "pipe", "inherit"], detached: true },
  );
  let preview: ChildProcess | undefined;

  const stop = async (): Promise<void> => {
    if (preview) {
      killGroup(preview, "SIGTERM");
      await Promise.race([exited(preview), sleep(5_000)]);
      killGroup(preview, "SIGKILL");
    }
    // Closing stdin is the stack's stop signal: it stops its children, drops
    // both databases and removes its temporary directory, then exits.
    stackProc.stdin?.end();
    const stopped = await Promise.race([exited(stackProc).then(() => true), sleep(STOP_GRACE_MS).then(() => false)]);
    if (!stopped) {
      killGroup(stackProc, "SIGTERM");
      await Promise.race([exited(stackProc), sleep(STOP_GRACE_MS)]);
      killGroup(stackProc, "SIGKILL");
    }
  };

  try {
    const ready = await readyLine(stackProc);
    const port = await freePort();
    preview = spawn(
      "npx",
      ["vite", "preview", "--host", "127.0.0.1", "--port", String(port), "--strictPort"],
      {
        cwd: WEB_CP,
        env: { ...process.env, CP_ORIGIN: ready.httpUrl },
        stdio: "inherit",
        // `npx` is a wrapper around the real server: a process group of its
        // own lets stop() take both.
        detached: true,
      },
    );
    const previewUrl = `http://127.0.0.1:${port}`;
    await waitServing(previewUrl);

    const control = ready.telemetryRelay?.controlUrl;
    const admin = new ApiClient(ready.httpUrl);
    await admin.post("/admin/login", ready.operator);
    return {
      previewUrl,
      httpUrl: ready.httpUrl,
      proxyUrl: ready.gateway.proxyUrl,
      gatewayMetricsUrl: ready.gateway.metricsUrl,
      relay: control === undefined ? undefined : {
        pause: () => relayCall(control, "pause"),
        resume: () => relayCall(control, "resume"),
      },
      mockUpstreamUrl: ready.mockUpstream.url,
      operator: ready.operator,
      admin,
      tenant: () => new ApiClient(ready.httpUrl),
      clearKeyGroup: (id: string) =>
        sqlAgainst(ready.cpDatabase, `UPDATE tenant_keys SET group_id = NULL WHERE id = '${id}'`),
      stop,
    };
  } catch (e) {
    await stop();
    throw e;
  }
}
