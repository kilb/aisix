import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:net";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  writeFileSync,
  readFileSync,
  rmSync,
  readdirSync,
  statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const REPO = resolve(import.meta.dirname, "../..");
const CONSOLE_BIN = join(REPO, "target/release/aisix-console");
const GATEWAY_BIN = join(REPO, "target/release/aisix");
const PORTAL_BIN = join(REPO, "target/release/aisix-portal");
export const PORTAL_TOKEN_SENTINEL = "e2e-portal-token-must-never-leak";
export const PASSWORD = "e2e-test-password";
/** 带网关的夹具里那把调用方密钥的明文。 */
export const CALLER_KEY = "e2e-console-caller-key";
/** 只有网关进程才有的环境变量：控制台与网关的环境本来就可以不同。 */
export const GATEWAY_ONLY_VAR = "E2E_GATEWAY_ONLY_KEY";
/** 只有控制台进程才有的环境变量：`aisix validate` 看得到，网关看不到。 */
export const CONSOLE_ONLY_VAR = "E2E_CONSOLE_ONLY_KEY";

export interface Fixture {
  dir: string;
  resourcesPath: string;
  consoleUrl: string;
  previewUrl: string;
  /** 真门户的地址；只有 `start({ withPortal: true })` 才有。 */
  portalUrl?: string;
  /** 真网关的代理口；只有 `start({ withGateway: true })` 才有。 */
  proxyUrl?: string;
  read(): string;
  stop(): void;
}

export interface StartOptions {
  /**
   * 顺带起一个**真的**门户进程，让控制台的「门户用户」页有真后端可打。
   *
   * 默认不起：默认那套用例要的正是「门户不可达」时的表现。两种状态互斥，
   * 所以分成两个夹具实例，而不是在一个里面切。
   */
  withPortal?: boolean;
  /**
   * 顺带起一个**真的**网关，读同一份 resources.yaml，让「保存后网关到底有没有
   * 用上」和逐请求日志有真东西可看。
   *
   * 默认不起：默认那套用例要的正是「管理 API 不可达」时的表现。
   */
  withGateway?: boolean;
}

const SEED = `_format_version: '1'
provider_keys:
- display_name: seed-pk
  provider: openai
  api_key: sk-seed
models:
- display_name: seed-model
  provider: openai
  provider_key: seed-pk
  model_name: gpt-4o-mini
api_keys:
- display_name: seed-key
  key_hash: ${"0".repeat(64)}
  allowed_models:
  - '*'
`;

/** 取一个空闲端口。固定端口会和上一次没清干净的进程撞上。 */
async function freePort(): Promise<number> {
  return new Promise((res, rej) => {
    const srv = createServer();
    srv.once("error", rej);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (addr && typeof addr === "object") {
        const port = addr.port;
        srv.close(() => res(port));
      } else {
        rej(new Error("拿不到端口"));
      }
    });
  });
}

async function waitFor(url: string, tries = 120): Promise<void> {
  for (let i = 0; i < tries; i++) {
    try {
      const r = await fetch(url);
      if (r.ok || r.status === 401 || r.status === 404) return;
    } catch {
      /* 还没起来 */
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`${url} 没能在超时内就绪`);
}

/** 起一个真的控制台 + 一个服务构建产物的静态服务器。 */
/**
 * 停掉一个子进程连同它 fork 出来的。
 *
 * `npx` 是外壳，真正的 server 在它底下。只 kill pid 的话那个 server 活下来变成
 * 孤儿 —— 每跑一次漏一个。攒到几十个之后机器变慢，E2E 开始超时，而那种失败看
 * 起来跟真 bug 一模一样（真的排查过一次）。
 */
function killTree(p: ChildProcess): void {
  if (p.pid === undefined) return;
  try {
    process.kill(-p.pid, "SIGTERM");
  } catch {
    try {
      p.kill("SIGTERM");
    } catch {
      /* 已经退了 */
    }
  }
}

/// Refuse to run against a binary older than the sources it was built from.
///
/// A twin of `web-portal/e2e/money.fixture.ts`'s `assertFresh`, kept as a copy
/// because that lives in a different npm package with its own tsconfig and
/// reaching across the boundary in a Playwright fixture is more fragile than
/// eight lines of `statSync`.
///
/// This suite is the one that most needs it: it spawns THREE binaries and all
/// three are RELEASE builds, which nobody rebuilds casually. A stale one turns
/// 30 green tests into a description of whatever was last compiled. That is
/// not hypothetical — the sibling check found exactly that on its first run
/// against the portal binary.
function assertFresh(bin: string, crateDirs: string[]): void {
  let binTime: number;
  try {
    binTime = statSync(bin).mtimeMs;
  } catch {
    throw new Error(
      `${bin} not found. Build it first: cargo build --release -p aisix-console -p aisix-portal -p aisix-server`,
    );
  }
  let newest = 0;
  let newestFile = "";
  const walk = (dir: string) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      const full = join(dir, e.name);
      if (e.isDirectory()) {
        // `src/bin/` holds a crate's side executables; nothing under test
        // links them, so touching one must not condemn this binary.
        if (e.name !== "bin") walk(full);
      } else if (e.name.endsWith(".rs")) {
        const t = statSync(full).mtimeMs;
        if (t > newest) {
          newest = t;
          newestFile = full;
        }
      }
    }
  };
  for (const d of crateDirs) walk(join(d, "src"));
  if (newest > binTime) {
    throw new Error(
      `${bin} is older than ${newestFile} — this run would describe the last ` +
        `build, not the current tree.\nRebuild: cargo build --release -p ` +
        `aisix-console -p aisix-portal -p aisix-server`,
    );
  }
}

/// Which crates each binary is actually built from. Comparing against all of
/// `crates/` would condemn the console binary for a gateway-only edit and
/// force a pointless release rebuild; comparing against only its own
/// directory would miss `aisix-core`, which all three link.
function crateDirsFor(re: RegExp): string[] {
  const crates = resolve(REPO, "crates");
  return readdirSync(crates, { withFileTypes: true })
    .filter((e) => e.isDirectory() && re.test(e.name))
    .map((e) => join(crates, e.name));
}

export async function start(opts: StartOptions = {}): Promise<Fixture> {
  assertFresh(CONSOLE_BIN, crateDirsFor(/^aisix-(console|core)$/));
  assertFresh(PORTAL_BIN, crateDirsFor(/^aisix-(portal|core)$/));
  assertFresh(GATEWAY_BIN, crateDirsFor(/^aisix-(?!portal$|console$)/));

  const dir = mkdtempSync(join(tmpdir(), "aisix-console-e2e-"));
  const resourcesPath = join(dir, "resources.yaml");
  writeFileSync(resourcesPath, SEED);

  const hashed = spawnSync(CONSOLE_BIN, ["hash", PASSWORD], { encoding: "utf8" });
  if (hashed.status !== 0) throw new Error(`hash 子命令失败: ${hashed.stderr}`);

  const apiPort = await freePort();
  const webPort = await freePort();
  const procs: ChildProcess[] = [];

  // 真网关（可选）。起在控制台之前 —— 控制台启动时就要拿到它的地址。
  let proxyUrl: string | undefined;
  let adminUrl = "http://127.0.0.1:1";
  let statusUrl: string | undefined;
  let consolePath = process.env.PATH ?? "";
  if (opts.withGateway) {
    const [proxyPort, adminPort, metricsPort] = [
      await freePort(),
      await freePort(),
      await freePort(),
    ];
    proxyUrl = `http://127.0.0.1:${proxyPort}`;
    adminUrl = `http://127.0.0.1:${adminPort}`;
    statusUrl = `http://127.0.0.1:${metricsPort}`;
    const callerHash = createHash("sha256").update(CALLER_KEY).digest("hex");
    writeFileSync(
      resourcesPath,
      SEED.replace("api_key: sk-seed", `api_key: \${${GATEWAY_ONLY_VAR}}`).replace(
        "0".repeat(64),
        callerHash,
      ),
    );
    const cfgPath = join(dir, "config.yaml");
    writeFileSync(
      cfgPath,
      `resources_file: "${resourcesPath}"
proxy:
  addr: "127.0.0.1:${proxyPort}"
admin:
  addr: "127.0.0.1:${adminPort}"
  admin_keys: ["e2e-admin-key"]
observability:
  log_level: info
  metrics:
    prometheus:
      enabled: true
      path: "/metrics"
      addr: "127.0.0.1:${metricsPort}"
`,
    );
    const gatewayLog = join(dir, "gateway.log");
    const logFd = openSync(gatewayLog, "a");
    procs.push(
      spawn(GATEWAY_BIN, ["--config", cfgPath], {
        env: { ...process.env, [GATEWAY_ONLY_VAR]: "sk-only-the-gateway-has-this" },
        stdio: ["ignore", logFd, logFd],
      }),
    );
    await waitFor(`${statusUrl}/metrics`);
    // 这里没有 systemd。控制台的日志页读 journald，所以给它一个读法：网关真实写
    // 出来的日志原样给它，内容没有任何一个字是编的。
    const bin = join(dir, "bin");
    mkdirSync(bin);
    const shim = join(bin, "journalctl");
    writeFileSync(shim, `#!/bin/sh\nexec tail -n 3000 "${gatewayLog}"\n`);
    chmodSync(shim, 0o755);
    consolePath = `${bin}:${consolePath}`;
  }

  // 真门户（可选）。起在控制台之前 —— 控制台启动时就要拿到它的地址。
  let portalUrl: string | undefined;
  if (opts.withPortal) {
    const portalPort = await freePort();
    portalUrl = `http://127.0.0.1:${portalPort}`;
    const portal = spawn(PORTAL_BIN, [], {
      env: {
        ...process.env,
        PORTAL_ADDR: `127.0.0.1:${portalPort}`,
        PORTAL_DB: `sqlite:${join(dir, "portal.db")}`,
        PORTAL_ADMIN_TOKEN: PORTAL_TOKEN_SENTINEL,
        PROMETHEUS_URL: "http://127.0.0.1:1",
        AISIX_RESOURCES: resourcesPath,
        // 对账环调到一小时一轮：本套用例测的是控制台界面，不需要它跑。让它
        // 在中途改写这份 resources.yaml，会跟控制台自己的配置用例互相干扰。
        PORTAL_TICK_SECS: "3600",
      },
      stdio: "ignore",
      detached: true,
    });
    procs.push(portal);
    await waitFor(`${portalUrl}/api/session`);
  }
  const api = spawn(CONSOLE_BIN, [], {
    env: {
      ...process.env,
      PATH: consolePath,
      [CONSOLE_ONLY_VAR]: "sk-only-the-console-has-this",
      ...(statusUrl ? { AISIX_STATUS_URL: statusUrl } : {}),
      CONSOLE_PASSWORD_HASH: hashed.stdout.trim(),
      AISIX_ADMIN_KEY_FOR_CONSOLE: "e2e-admin-key",
      // 哨兵值：用例断言它绝不出现在任何前端产物或接口响应里。
      // 控制台持有它去调门户的管理端；浏览器拿到它就等于拿到了给任何人发放
      // 额度的权力，而门户的管理端**不认控制台的会话**，那把 token 是唯一的钥匙。
      PORTAL_ADMIN_TOKEN: PORTAL_TOKEN_SENTINEL,
      // 指向一个不存在的门户：这一层测的是「凭据不泄漏」与「门户不可达时的
      // 表现」，有门户时的发放流程由 web-portal/e2e 覆盖（那里有真门户）。
      PORTAL_URL: portalUrl ?? "http://127.0.0.1:1",
      // 网关管理 API 不需要真的可达：本套用例只驱动配置读写这条路径，
      // 而那条路径只用到 resources.yaml 和 `aisix validate`。
      AISIX_ADMIN_URL: adminUrl,
      PROMETHEUS_URL: "http://127.0.0.1:1",
      AISIX_RESOURCES: resourcesPath,
      AISIX_BIN: GATEWAY_BIN,
      CONSOLE_ADDR: `127.0.0.1:${apiPort}`,
    },
    stdio: "ignore",
  });
  procs.push(api);

  const preview = spawn("npx", [
      "vite",
      "preview",
      "--port",
      String(webPort),
      "--strictPort",
      // 显式绑 IPv4：preview 默认只听 localhost，在双栈机器上解析成
      // [::1]，而探测和 Playwright 用的是 127.0.0.1 —— 于是永远等不到。
      "--host",
      "127.0.0.1",
    ], {
    cwd: resolve(import.meta.dirname, ".."),
    // preview 的代理指向本套用例起的那个控制台，不是开发用的 8090。
    env: { ...process.env, CONSOLE_ORIGIN: `http://127.0.0.1:${apiPort}` },
    stdio: "ignore",
    // 自成进程组，收尾时才能把 npx 底下那个真正的 server 一起带走。见 killTree。
    detached: true,
  });
  procs.push(preview);

  await waitFor(`http://127.0.0.1:${apiPort}/healthz`);
  await waitFor(`http://127.0.0.1:${webPort}/`);

  return {
    dir,
    resourcesPath,
    consoleUrl: `http://127.0.0.1:${apiPort}`,
    previewUrl: `http://127.0.0.1:${webPort}`,
    portalUrl,
    proxyUrl,
    read: () => readFileSync(resourcesPath, "utf8"),
    stop() {
      for (const p of procs) killTree(p);
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
