import { spawn, type ChildProcess } from "node:child_process";
import { createServer, type Server } from "node:http";
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer as netServer } from "node:net";

/**
 * 钱路夹具：**真网关 + 真流量 + 真指标 + 真门户**。
 *
 * 仓库既有的 `tests/e2e` harness 依赖 etcd。走文件模式（`resources_file`）
 * 之后不需要 etcd，而门户本来就是写这个文件的 —— 于是这一层测的正是生产里
 * 那条链路：请求打进网关 → 网关按定价算出花费并打到指标 → 门户对账扣余额 →
 * 余额归零把密钥写成 disabled → 网关据此拒绝。
 *
 * 只有上游 LLM 是桩（对着付费接口打不出确定性的流量），这与仓库里
 * `startOpenAiUpstream` 的做法一致。
 */

const GATEWAY_BIN = join(process.cwd(), "..", "target", "release", "aisix");
const PORTAL_BIN = join(process.cwd(), "..", "target", "debug", "aisix-portal");
export const ADMIN_TOKEN = "e2e-money-admin";
export const CALLER_KEY = "e2e-caller-plaintext-key-0001";

/**
 * 停掉一个子进程连同它 fork 出来的。
 *
 * `detached: true` 让子进程自成进程组，`kill(-pid)` 才能把整组带走。只杀 pid
 * 的话 `npx` 外壳下面那个真正的 server 会活下来变成孤儿。
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

async function waitReady(url: string, label: string, ok = (s: number) => s < 500) {
  for (let i = 0; i < 200; i++) {
    try {
      if (ok((await fetch(url)).status)) return;
    } catch {
      /* 还没起来 */
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`${label} 在 40 秒内没就绪: ${url}`);
}

export interface MoneyFixture {
  proxyUrl: string;
  metricsUrl: string;
  portalUrl: string;
  resourcesPath: string;
  userId: string;
  readResources(): string;
  writeResources(body: string): void;
  /** 用夹具预置的密钥打一次真实 chat，返回状态码。 */
  chat(): Promise<number>;
  /** 用任意明文密钥打一次真实 chat —— 用来验自助建的那把能不能调通。 */
  chatWith(plaintext: string): Promise<number>;
  /** 门户会话 cookie，用来调 /api/keys。 */
  sessionCookie(): Promise<string>;
  stop(): void;
}

/** sha256 —— 与网关 `key_hash` 的算法一致。 */
async function sha256Hex(s: string): Promise<string> {
  const b = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s));
  return [...new Uint8Array(b)].map((x) => x.toString(16).padStart(2, "0")).join("");
}

export async function startMoney(): Promise<MoneyFixture> {
  const dir = mkdtempSync(join(tmpdir(), "aisix-money-e2e-"));
  const resourcesPath = join(dir, "resources.yaml");
  const procs: ChildProcess[] = [];
  const servers: Server[] = [];

  const upPort = await freePort();
  const proxyPort = await freePort();
  const metricsPort = await freePort();
  const promPort = await freePort();
  const portalPort = await freePort();

  // 桩上游：OpenAI 兼容的应答，带固定 usage 让花费可预期。
  const upstream = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          id: "chatcmpl-e2e",
          object: "chat.completion",
          created: 0,
          model: "gpt-4o-mini",
          choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
          // 每次调用 1000 输入 + 1000 输出 token。配上下面的定价，
          // 单次花费 = 1000/1000*0.5 + 1000/1000*0.5 = $1.00 = 1_000_000 micro。
          usage: { prompt_tokens: 1000, completion_tokens: 1000, total_tokens: 2000 },
        }),
      );
    });
  });
  await new Promise<void>((r) => upstream.listen(upPort, "127.0.0.1", r));
  servers.push(upstream);

  // PromQL 垫片：只回答 sweeper 发的那一种查询。
  //
  // Prometheus 本身是我们不发布的基础设施，而这一层需要**有个东西**回答
  // PromQL。仓库里已有 otlp-mock / sls-mock / jwks-mock 的先例，这是同一类
  // 东西。真网关、真请求、真指标、真定价、真账本、真停用都在链路上；只有
  // PromQL 引擎是垫片。
  //
  // increase() 在连续窗口上的语义就是「本次累计值 − 上次累计值」，垫片按这个
  // 记账（无重置场景下与真 Prometheus 等价）。
  const lastSeen = new Map<string, number>();
  const prom = createServer((req, res) => {
    void (async () => {
      const u = new URL(req.url ?? "/", "http://x");
      const q = u.searchParams.get("query") ?? "";
      const uid = /user_id="([^"]+)"/.exec(q)?.[1] ?? "";
      let cumulative = 0;
      try {
        const text = await fetch(`http://127.0.0.1:${metricsPort}/metrics`).then((r) => r.text());
        for (const line of text.split("\n")) {
          if (!line.startsWith("aisix_llm_spend_micro_usd_total{")) continue;
          if (!line.includes(`user_id="${uid}"`)) continue;
          cumulative += Number(line.slice(line.lastIndexOf("}") + 1).trim()) || 0;
        }
      } catch {
        /* 网关还没起来 */
      }
      const prev = lastSeen.get(uid) ?? 0;
      const delta = Math.max(0, cumulative - prev);
      lastSeen.set(uid, cumulative);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          status: "success",
          data: {
            resultType: "vector",
            result: [{ metric: {}, value: [0, String(delta)] }],
          },
        }),
      );
    })();
  });
  await new Promise<void>((r) => prom.listen(promPort, "127.0.0.1", r));
  servers.push(prom);

  // 门户先起来：密钥要绑的 user_id 是它注册时铸的 uuid，编不出来。
  // 这个顺序本身就是一期的真实形态 —— 管理员在有了用户之后才建密钥。
  const portalUrl = `http://127.0.0.1:${portalPort}`;
  writeFileSync(resourcesPath, "api_keys: []\n");
  procs.push(
    spawn(PORTAL_BIN, [], {
      env: {
        ...process.env,
        PORTAL_DB: `sqlite:${join(dir, "portal.db")}`,
        PORTAL_ADDR: `127.0.0.1:${portalPort}`,
        PORTAL_ADMIN_TOKEN: ADMIN_TOKEN,
        PROMETHEUS_URL: `http://127.0.0.1:${promPort}`,
        // 一条用例要跨四个状态、每个等一轮对账。15 秒一轮会把超时预算吃光，
        // 而那种失败长得跟真 bug 一样。调快是让测试更确定，不是放宽断言。
        PORTAL_TICK_SECS: "2",
        AISIX_RESOURCES: resourcesPath,
      },
      stdio: "inherit",
    }),
  );
  await waitReady(`${portalUrl}/api/session`, "aisix-portal");

  const reg = await fetch(`${portalUrl}/api/register`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: "money@e2e.test", password: "correct horse battery" }),
  });
  if (reg.status !== 201) throw new Error(`注册失败: ${reg.status}`);
  const userId: string = (await reg.json()).user_id;

  const keyHash = await sha256Hex(CALLER_KEY);
  writeResources(resourcesPath, keyHash, userId, `http://127.0.0.1:${upPort}/v1`, false);

  const adminPort = await freePort();
  const cfgPath = join(dir, "config.yaml");
  writeFileSync(
    cfgPath,
    `resources_file: "${resourcesPath}"
proxy:
  addr: "127.0.0.1:${proxyPort}"
admin:
  addr: "127.0.0.1:${adminPort}"
  admin_keys: ["e2e-gateway-admin"]
observability:
  metrics:
    prometheus:
      enabled: true
      path: "/metrics"
      addr: "127.0.0.1:${metricsPort}"
`,
  );

  procs.push(spawn(GATEWAY_BIN, ["--config", cfgPath], { stdio: "inherit" }));
  const metricsUrl = `http://127.0.0.1:${metricsPort}/metrics`;
  await waitReady(metricsUrl, "aisix 网关指标口", (s) => s === 200);

  return {
    proxyUrl: `http://127.0.0.1:${proxyPort}`,
    metricsUrl,
    portalUrl,
    resourcesPath,
    userId,
    readResources: () => readFileSync(resourcesPath, "utf8"),
    writeResources: (b: string) => writeFileSync(resourcesPath, b),
    async chat() {
      return chatWith(proxyPort, CALLER_KEY);
    },
    async chatWith(plaintext: string) {
      return chatWith(proxyPort, plaintext);
    },
    async sessionCookie() {
      const r = await fetch(`${portalUrl}/api/login`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ email: "money@e2e.test", password: "correct horse battery" }),
      });
      const raw = r.headers.getSetCookie?.() ?? [];
      const c = raw.map((x) => x.split(";")[0]).join("; ");
      if (!c.includes("aisix_portal=")) throw new Error("拿不到门户会话 cookie");
      return c;
    },
    stop() {
      for (const p of procs) killTree(p);
      for (const s of servers) s.close();
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

/** 打一次真实的 chat。 */
async function chatWith(proxyPort: number, plaintext: string): Promise<number> {
  const r = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o-mini", messages: [{ role: "user", content: "hi" }] }),
  });
  return r.status;
}

function writeResources(
  path: string,
  keyHash: string,
  userId: string,
  upstreamBase: string,
  disabled: boolean,
): void {
  writeFileSync(
    path,
    `_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
    api_base: "${upstreamBase}"
models:
  - display_name: gpt-4o-mini
    provider: openai
    provider_key: stub
    model_name: gpt-4o-mini
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
api_keys:
  - display_name: e2e-caller
    key_hash: "${keyHash}"
    user_id: "${userId}"
    allowed_models: ["*"]
    disabled: ${disabled}
`,
  );
}
