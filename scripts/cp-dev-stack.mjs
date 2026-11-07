#!/usr/bin/env node
// A throwaway control-plane stack for development and end-to-end tests: two
// fresh Postgres databases, one kine process, the control plane with a first
// admin operator, and optionally one AISIX gateway in managed mode booted from
// a credential the control plane issues. Node built-ins only.
//
// Usage:
//   node scripts/cp-dev-stack.mjs [--http-port N] [--dp-port N] [--kine-port N]
//     [--etcd-prefix /aisix] [--gateway <aisix binary> [--gateway-name stack-gw]
//     [--proxy-port N]] [--stale-secs 10]
//
// Ports default to free ones picked on 127.0.0.1.
//
// Environment:
//   CP_E2E_PG_URL    required: a Postgres URL whose role may CREATE DATABASE.
//                    The stack creates cp_<rand> and kine_<rand> on that
//                    server and drops both on the way out.
//   CP_E2E_KINE_BIN  the kine binary (default: `kine` on PATH).
//   CP_E2E_BIN       the control-plane binary (default:
//                    target/debug/aisix-control-plane). Refused when it is
//                    older than the sources it is built from, except on CI.
//   CP_E2E_PSQL      the psql binary (default: `psql` on PATH).
//
// When everything is up it prints ONE line of JSON to stdout:
//   {"ready": true, "httpUrl", "dpUrl", "envId", "kineEndpoint", "etcdPrefix",
//    "operator": {"email", "password"}, "gateway"?: {"proxyUrl", "dpId", "name"}}
// Everything else (its own progress and every child's output, line-prefixed)
// goes to stderr.
//
// It stops on SIGTERM, on SIGINT, or when its stdin closes: every child is
// killed by process group, both databases are dropped and the temporary
// directory is removed. Run it with stdin attached (a terminal, or a pipe the
// caller holds open): a stdin that is already at its end, such as /dev/null,
// stops the stack as soon as it is up.

import { spawn, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtempSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer, connect } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const OPERATOR_EMAIL = "ops@e2e.local";
const READY_TIMEOUT_MS = 60_000;
const STOP_GRACE_MS = 15_000;

function log(message) {
  process.stderr.write(`[cp-dev-stack] ${message}\n`);
}

function fail(message) {
  log(message);
  return teardown(1);
}

// ── arguments ───────────────────────────────────────────────────────────────

const FLAGS = new Set([
  "--http-port",
  "--dp-port",
  "--kine-port",
  "--etcd-prefix",
  "--gateway",
  "--gateway-name",
  "--proxy-port",
  "--stale-secs",
]);

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 2) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (!FLAGS.has(flag) || value === undefined) {
      process.stderr.write(
        "usage: node scripts/cp-dev-stack.mjs [--http-port N] [--dp-port N] [--kine-port N] " +
          "[--etcd-prefix /aisix] [--gateway <aisix binary>] [--gateway-name stack-gw] " +
          "[--proxy-port N] [--stale-secs 10]\n",
      );
      process.exit(2);
    }
    out[flag.slice(2)] = value;
  }
  return out;
}

function port(raw, name) {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 1 || n > 65535) {
    process.stderr.write(`cp-dev-stack: --${name} must be a port number\n`);
    process.exit(2);
  }
  return n;
}

/** A port nothing is bound to on 127.0.0.1 right now. */
function freePort() {
  return new Promise((resolvePort, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port: p } = srv.address();
      srv.close(() => resolvePort(p));
    });
  });
}

// ── the control-plane binary must describe this tree ─────────────────────────

/// Refuse a binary older than the sources it is built from: a stale build
/// turns a green run into a description of whatever was last compiled. The
/// control plane links one workspace crate, aisix-core; its migrations and
/// protos are compiled in. Skipped on CI, where the job builds the binary
/// right before the run.
function assertFresh(bin) {
  if (process.env.CI) return;
  let binTime;
  try {
    binTime = statSync(bin).mtimeMs;
  } catch {
    throw new Error(
      `${bin} does not exist. Build it first: cargo build -p aisix-control-plane (DEBUG)`,
    );
  }
  let newest = 0;
  let newestFile = "";
  const consider = (file) => {
    const t = statSync(file).mtimeMs;
    if (t > newest) {
      newest = t;
      newestFile = file;
    }
  };
  const walk = (dir) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      const full = join(dir, e.name);
      if (e.isDirectory()) walk(full);
      else consider(full);
    }
  };
  const cp = join(REPO, "crates", "aisix-control-plane");
  for (const dir of ["src", "migrations", "proto"]) walk(join(cp, dir));
  walk(join(REPO, "crates", "aisix-core", "src"));
  for (const file of [
    join(cp, "Cargo.toml"),
    join(cp, "build.rs"),
    join(REPO, "crates", "aisix-core", "Cargo.toml"),
    join(REPO, "Cargo.toml"),
    join(REPO, "Cargo.lock"),
  ]) {
    try {
      consider(file);
    } catch {
      /* absent in some checkouts */
    }
  }
  if (newest > binTime) {
    throw new Error(
      `${bin} is older than ${newestFile}: the stack would run the last build, not this ` +
        `tree. Rebuild it: cargo build -p aisix-control-plane (DEBUG)`,
    );
  }
}

// ── children ────────────────────────────────────────────────────────────────

/** Running children, in start order; stopped in reverse. */
const children = [];
let stopping = false;

/** The inherited environment without anything a child reads as its config. */
function baseEnv() {
  const env = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v === undefined || k.startsWith("CP_") || k.startsWith("AISIX_")) continue;
    if (/^(https?|all)_proxy$/i.test(k)) continue;
    env[k] = v;
  }
  env.NO_PROXY = "127.0.0.1,localhost";
  env.no_proxy = "127.0.0.1,localhost";
  // The children's lines end up in a log, not a terminal.
  env.NO_COLOR = "1";
  return env;
}

/** Forward a child's output to stderr, one prefixed line at a time. */
function pipeLines(stream, tag) {
  let pending = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk) => {
    pending += chunk;
    const lines = pending.split("\n");
    pending = lines.pop();
    for (const line of lines) process.stderr.write(`[${tag}] ${line}\n`);
  });
  stream.on("end", () => {
    if (pending) process.stderr.write(`[${tag}] ${pending}\n`);
  });
}

/**
 * Start a child in a process group of its own, so stopping it reaches
 * anything it started. An exit nobody asked for stops the whole stack.
 */
function startChild(tag, bin, args, env) {
  // A stop that arrived while main() was still starting things has already
  // stopped every child it knew of; one started now would be left behind.
  if (stopping) throw new Error(`not starting ${tag}: the stack is stopping`);
  const child = spawn(bin, args, { env, detached: true, stdio: ["ignore", "pipe", "pipe"] });
  const entry = { tag, child, exited: false };
  entry.exit = new Promise((resolveExit) => {
    child.once("exit", (code, signal) => {
      entry.exited = true;
      resolveExit();
      if (!stopping) {
        void fail(`${tag} exited unexpectedly (code=${code} signal=${signal})`);
      }
    });
  });
  child.once("error", (e) => {
    entry.exited = true;
    if (!stopping) void fail(`${tag} could not be started: ${e.message}`);
  });
  pipeLines(child.stdout, tag);
  pipeLines(child.stderr, tag);
  children.push(entry);
  return entry;
}

async function stopChild(entry) {
  if (entry.exited || entry.child.pid === undefined) return;
  const signal = (sig) => {
    try {
      process.kill(-entry.child.pid, sig);
    } catch {
      /* already gone */
    }
  };
  signal("SIGTERM");
  const exited = await Promise.race([
    entry.exit.then(() => true),
    sleep(STOP_GRACE_MS).then(() => false),
  ]);
  if (!exited) {
    log(`${entry.tag} did not stop within ${STOP_GRACE_MS} ms; killing it`);
    signal("SIGKILL");
    await entry.exit;
  }
}

// ── databases ───────────────────────────────────────────────────────────────

const PSQL = process.env.CP_E2E_PSQL || "psql";
const databases = [];

function psql(pgUrl, sql) {
  const r = spawnSync(PSQL, [pgUrl, "-v", "ON_ERROR_STOP=1", "-qAtc", sql], {
    encoding: "utf8",
    env: baseEnv(),
  });
  if (r.error) throw new Error(`cannot run ${PSQL}: ${r.error.message}`);
  if (r.status !== 0) throw new Error(`psql failed (${sql}): ${r.stderr.trim()}`);
  return r.stdout;
}

function databaseUrl(pgUrl, name, params = {}) {
  const url = new URL(pgUrl);
  url.pathname = `/${name}`;
  for (const [k, v] of Object.entries(params)) url.searchParams.set(k, v);
  return url.toString();
}

// ── teardown ────────────────────────────────────────────────────────────────

let tempDir;
let teardownStarted;

function teardown(code) {
  if (teardownStarted) return teardownStarted;
  stopping = true;
  teardownStarted = (async () => {
    for (const entry of [...children].reverse()) await stopChild(entry);
    const pgUrl = process.env.CP_E2E_PG_URL;
    for (const name of databases) {
      try {
        psql(pgUrl, `DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
      } catch (e) {
        log(`could not drop database ${name}: ${e.message}`);
      }
    }
    if (tempDir) rmSync(tempDir, { recursive: true, force: true });
    process.exit(code);
  })();
  return teardownStarted;
}

process.on("SIGTERM", () => void teardown(0));
process.on("SIGINT", () => void teardown(0));
process.stdin.on("end", () => void teardown(0));
process.stdin.on("error", () => void teardown(0));
process.stdin.resume();

// ── HTTP helpers ────────────────────────────────────────────────────────────

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function waitFor(what, probe, timeoutMs = READY_TIMEOUT_MS) {
  const deadline = Date.now() + timeoutMs;
  let last = "no attempt";
  while (Date.now() < deadline) {
    if (stopping) throw new Error(`stopped while waiting for ${what}`);
    try {
      if (await probe()) return;
      last = "not ready";
    } catch (e) {
      last = e.message;
    }
    await sleep(100);
  }
  throw new Error(`timed out waiting for ${what}: ${last}`);
}

function tcpOpen(p) {
  return new Promise((resolveOpen) => {
    const sock = connect({ host: "127.0.0.1", port: p });
    sock.once("connect", () => {
      sock.destroy();
      resolveOpen(true);
    });
    sock.once("error", () => resolveOpen(false));
  });
}

async function httpOk(url) {
  const res = await fetch(url);
  await res.arrayBuffer();
  return res.status === 200;
}

async function api(httpUrl, method, path, cookie, body) {
  const res = await fetch(`${httpUrl}${path}`, {
    method,
    // Labelled JSON with or without a body: the control plane refuses a
    // mutating request that is not.
    headers: {
      ...(cookie ? { cookie } : {}),
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  if (res.status >= 300) throw new Error(`${method} ${path} answered ${res.status}: ${text}`);
  return { res, body: text ? JSON.parse(text) : null };
}

// ── main ────────────────────────────────────────────────────────────────────

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const pgUrl = process.env.CP_E2E_PG_URL;
  if (!pgUrl) return fail("CP_E2E_PG_URL is required");
  const kineBin = process.env.CP_E2E_KINE_BIN || "kine";
  const cpBin = resolve(REPO, process.env.CP_E2E_BIN || "target/debug/aisix-control-plane");
  assertFresh(cpBin);

  const httpPort = port(args["http-port"], "http-port") ?? (await freePort());
  const dpPort = port(args["dp-port"], "dp-port") ?? (await freePort());
  const kinePort = port(args["kine-port"], "kine-port") ?? (await freePort());
  const etcdPrefix = args["etcd-prefix"] ?? "/aisix";
  const staleSecs = args["stale-secs"] ?? "10";

  tempDir = mkdtempSync(join(tmpdir(), "cp-dev-stack-"));
  const suffix = randomBytes(6).toString("hex");
  const cpDb = `cp_${suffix}`;
  const kineDb = `kine_${suffix}`;
  for (const name of [cpDb, kineDb]) {
    psql(pgUrl, `CREATE DATABASE ${name}`);
    databases.push(name);
  }

  const kineEndpoint = `http://127.0.0.1:${kinePort}`;
  startChild("kine", kineBin, [
    "--endpoint",
    databaseUrl(pgUrl, kineDb, { sslmode: "disable" }),
    "--listen-address",
    `127.0.0.1:${kinePort}`,
    "--metrics-bind-address",
    "0",
    "--compact-interval",
    "5m",
  ], baseEnv());
  await waitFor(`kine on 127.0.0.1:${kinePort}`, () => tcpOpen(kinePort));

  const masterKeyFile = join(tempDir, "master.key");
  writeFileSync(masterKeyFile, `${randomBytes(32).toString("hex")}\n`, { mode: 0o600 });
  const cpEnv = {
    ...baseEnv(),
    CP_DATABASE_URL: databaseUrl(pgUrl, cpDb),
    CP_MASTER_KEY_FILE: masterKeyFile,
    CP_DP_HOSTS: "127.0.0.1",
    CP_HTTP_ADDR: `127.0.0.1:${httpPort}`,
    CP_DP_ADDR: `127.0.0.1:${dpPort}`,
    CP_KINE_ENDPOINT: kineEndpoint,
    CP_ETCD_PREFIX: etcdPrefix,
    CP_INSTANCE_STALE_SECS: staleSecs,
  };

  // The first operator: the only way one comes to exist.
  const password = `ops-${randomBytes(12).toString("hex")}`;
  const created = spawnSync(
    cpBin,
    ["create-operator", "--email", OPERATOR_EMAIL, "--role", "admin"],
    { env: cpEnv, input: `${password}\n`, encoding: "utf8" },
  );
  if (created.error || created.status !== 0) {
    return fail(
      `create-operator failed: ${created.error?.message ?? ""}${created.stderr ?? ""}`.trim(),
    );
  }

  startChild("cp", cpBin, ["serve"], cpEnv);
  const httpUrl = `http://127.0.0.1:${httpPort}`;
  await waitFor(`${httpUrl}/healthz`, () => httpOk(`${httpUrl}/healthz`));

  const login = await api(httpUrl, "POST", "/admin/login", undefined, {
    email: OPERATOR_EMAIL,
    password,
  });
  const cookie = (login.res.headers.get("set-cookie") ?? "").split(";")[0];
  const environment = await api(httpUrl, "GET", "/admin/environment", cookie);

  const out = {
    ready: true,
    httpUrl,
    dpUrl: `https://127.0.0.1:${dpPort}`,
    envId: environment.body.env_id,
    kineEndpoint,
    etcdPrefix,
    operator: { email: OPERATOR_EMAIL, password },
  };

  if (args.gateway !== undefined) {
    const name = args["gateway-name"] ?? "stack-gw";
    const proxyPort = port(args["proxy-port"], "proxy-port") ?? (await freePort());
    const metricsPort = await freePort();
    const issued = await api(httpUrl, "POST", "/admin/gateways/credentials", cookie, { name });
    const gwDir = join(tempDir, "gateway");
    const gwEnv = { ...baseEnv() };
    for (const [k, v] of Object.entries(issued.body.env)) {
      // The image's entrypoint reads it; the binary is given --config here.
      if (k !== "AISIX_CONFIG_PATH") gwEnv[k] = v;
    }
    // Local overrides. The bundle names paths under /var/lib/aisix and the
    // image's fixed ports, which a developer's machine (or two stacks side by
    // side) cannot give every gateway. The same list is in
    // tests/e2e/src/harness/control-plane.ts.
    Object.assign(gwEnv, {
      AISIX_MANAGED__MTLS_DIR: join(gwDir, "mtls"),
      AISIX_MANAGED__DP_ID_FILE: join(gwDir, "dp_id"),
      AISIX_MANAGED__SNAPSHOT_CACHE_PATH: join(gwDir, "config_cache.json"),
      AISIX_MANAGED__HEARTBEAT_INTERVAL_SECS: "5",
      AISIX_PROXY__ADDR: `127.0.0.1:${proxyPort}`,
      AISIX_OBSERVABILITY__METRICS__PROMETHEUS__ADDR: `127.0.0.1:${metricsPort}`,
      AISIX_OBSERVABILITY__DEBUG__ENABLED: "false",
      AISIX_OBSERVABILITY__HEAP_PROFILING__AUTO_DUMP__DIR: join(gwDir, "heap"),
      AISIX_SHUTDOWN__MIN_DRAIN_SECS: "0",
    });
    startChild("gw", resolve(args.gateway), ["--config", join(REPO, "config.managed.yaml")], gwEnv);
    const proxyUrl = `http://127.0.0.1:${proxyPort}`;
    await waitFor(`${proxyUrl}/livez`, () => httpOk(`${proxyUrl}/livez`));
    // Registered too: a control plane with a release stamp judges key writes
    // by the versions registered gateways report, so whoever uses the stack
    // must not mint keys before this gateway has sent its first heartbeat.
    const dpId = issued.body.dp_id;
    await waitFor(`the gateway ${dpId} to register`, async () => {
      const instances = await api(httpUrl, "GET", "/admin/gateways/instances", cookie);
      return instances.body.items.some((i) => i.dp_id === dpId);
    });
    out.gateway = { proxyUrl, dpId, name };
  }

  process.stdout.write(`${JSON.stringify(out)}\n`);
}

main().catch((e) => void fail(e instanceof Error ? e.message : String(e)));
