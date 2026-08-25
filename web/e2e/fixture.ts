import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { createServer } from "node:net";
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const REPO = resolve(import.meta.dirname, "../..");
const CONSOLE_BIN = join(REPO, "target/release/aisix-console");
const GATEWAY_BIN = join(REPO, "target/release/aisix");
export const PASSWORD = "e2e-test-password";

export interface Fixture {
  dir: string;
  resourcesPath: string;
  consoleUrl: string;
  previewUrl: string;
  read(): string;
  stop(): void;
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
export async function start(): Promise<Fixture> {
  const dir = mkdtempSync(join(tmpdir(), "aisix-console-e2e-"));
  const resourcesPath = join(dir, "resources.yaml");
  writeFileSync(resourcesPath, SEED);

  const hashed = spawnSync(CONSOLE_BIN, ["hash", PASSWORD], { encoding: "utf8" });
  if (hashed.status !== 0) throw new Error(`hash 子命令失败: ${hashed.stderr}`);

  const apiPort = await freePort();
  const webPort = await freePort();
  const procs: ChildProcess[] = [];
  const api = spawn(CONSOLE_BIN, [], {
    env: {
      ...process.env,
      CONSOLE_PASSWORD_HASH: hashed.stdout.trim(),
      AISIX_ADMIN_KEY_FOR_CONSOLE: "e2e-admin-key",
      // 网关管理 API 不需要真的可达：本套用例只驱动配置读写这条路径，
      // 而那条路径只用到 resources.yaml 和 `aisix validate`。
      AISIX_ADMIN_URL: "http://127.0.0.1:1",
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
  });
  procs.push(preview);

  await waitFor(`http://127.0.0.1:${apiPort}/healthz`);
  await waitFor(`http://127.0.0.1:${webPort}/`);

  return {
    dir,
    resourcesPath,
    consoleUrl: `http://127.0.0.1:${apiPort}`,
    previewUrl: `http://127.0.0.1:${webPort}`,
    read: () => readFileSync(resourcesPath, "utf8"),
    stop() {
      for (const p of procs) p.kill("SIGTERM");
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
