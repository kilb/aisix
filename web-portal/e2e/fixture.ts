import { spawn, type ChildProcess } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer as netServer } from "node:net";

/**
 * 拉起**真实**的 aisix-portal 进程与 vite preview。
 *
 * 与 `web/e2e/fixture.ts` 同一形状：真后端、真 HTTP、随机空闲端口、绑
 * 127.0.0.1（vite preview 默认监听 [::1]，探 127.0.0.1 会一直超时）。
 */

const PORTAL_BIN = join(process.cwd(), "..", "target", "debug", "aisix-portal");
export const PASSWORD = "correct horse battery";
export const ADMIN_TOKEN = "e2e-portal-admin-token";

async function freePort(): Promise<number> {
  return new Promise((res, rej) => {
    const s = netServer();
    s.once("error", rej);
    s.listen(0, "127.0.0.1", () => {
      const a = s.address();
      if (a && typeof a === "object") {
        const p = a.port;
        s.close(() => res(p));
      } else rej(new Error("拿不到端口"));
    });
  });
}

async function waitReady(url: string, label: string): Promise<void> {
  for (let i = 0; i < 150; i++) {
    try {
      const r = await fetch(url);
      if (r.status < 500) return;
    } catch {
      /* 还没起来 */
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`${label} 在 30 秒内没起来: ${url}`);
}

export interface Fixture {
  portalUrl: string;
  previewUrl: string;
  adminToken: string;
  /** 读当前的 resources.yaml —— 断言密钥启停用得到它。 */
  readResources(): string;
  /** 写 resources.yaml —— 注册后才知道 user_id，密钥要在那之后才绑得上。 */
  writeResources(body: string): void;
  stop(): void;
}

export async function start(): Promise<Fixture> {
  const dir = mkdtempSync(join(tmpdir(), "aisix-portal-e2e-"));
  const resourcesPath = join(dir, "resources.yaml");
  writeFileSync(resourcesPath, "api_keys: []\n");

  const procs: ChildProcess[] = [];
  const apiPort = await freePort();
  const webPort = await freePort();
  const promPort = await freePort();

  // 一个安静的 Prometheus：应答合法但没有数据。
  //
  // 这一层测的是「人」的路径 —— 注册、登录、看到自己的余额与用量。真实消费
  // 由另一层（tests/e2e，真网关真流量）负责。这里给一个空应答，是为了让
  // 「读不到」与「是零」两种状态都能被驱动到，而不是让门户对着一个挂掉的
  // 后端跑。
  const prom = createServer((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ status: "success", data: { resultType: "vector", result: [] } }));
  });
  await new Promise<void>((r) => prom.listen(promPort, "127.0.0.1", r));

  procs.push(
    spawn(PORTAL_BIN, [], {
      env: {
        ...process.env,
        PORTAL_DB: `sqlite:${join(dir, "portal.db")}`,
        PORTAL_ADDR: `127.0.0.1:${apiPort}`,
        PORTAL_ADMIN_TOKEN: ADMIN_TOKEN,
        PROMETHEUS_URL: `http://127.0.0.1:${promPort}`,
        AISIX_RESOURCES: resourcesPath,
      },
      stdio: "inherit",
    }),
  );
  const portalUrl = `http://127.0.0.1:${apiPort}`;
  await waitReady(`${portalUrl}/api/session`, "aisix-portal");

  procs.push(
    spawn(
      "npx",
      ["vite", "preview", "--port", String(webPort), "--strictPort", "--host", "127.0.0.1"],
      { env: { ...process.env, PORTAL_ORIGIN: portalUrl }, stdio: "inherit" },
    ),
  );
  const previewUrl = `http://127.0.0.1:${webPort}`;
  await waitReady(previewUrl, "vite preview");

  return {
    portalUrl,
    previewUrl,
    adminToken: ADMIN_TOKEN,
    readResources: () => readFileSync(resourcesPath, "utf8"),
    writeResources: (b: string) => writeFileSync(resourcesPath, b),
    stop: () => {
      for (const p of procs) p.kill("SIGTERM");
      prom.close();
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
