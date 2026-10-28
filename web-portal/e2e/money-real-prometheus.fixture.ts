import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { createServer, request, type Server } from "node:http";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ADMIN_TOKEN,
  CALLER_KEY,
  GATEWAY_BIN,
  PORTAL_BIN,
  assertMoneyBinariesFresh,
  freePort,
  sha256Hex,
  waitReady,
} from "./money.fixture";

/**
 * 钱路夹具的**真 Prometheus** 版：门户的计费查询由一个真 Prometheus 回答，它按
 * 固定间隔抓取 e2e 网关的指标口。
 *
 * `money.fixture.ts` 用的是 PromQL 垫片：它用正则解析门户发的那一种查询，自己
 * 重新实现 `resets()`、`offset`、`or`、`and on(job, instance)` 与回看窗口。查询串
 * 写错了它照样答得对 —— 当初「$5 只扣到 $0」就是被垫片这样盖住的。这一套补的就是
 * 这个洞：查询文本、Prometheus 的求值语义、抓取时机全都是真的。
 *
 * 只有上游 LLM 是桩（付费接口打不出确定性的用量）。
 *
 * 多副本时门户前面放一个轮询转发：高可用的两个 Prometheus 各自抓取，门户每次
 * 查询轮到哪个就读到哪个的视图 —— 生产里两个副本挂在同一个负载均衡后面就是这样。
 */

/** 钉死的版本。CI 下载的就是它；本地版本不同就报错，别让别的版本的结果冒充证据。 */
export const PROMETHEUS_VERSION = "2.53.4";

/** 每次调用 1000 输入 + 1000 输出 token，两边都是 $0.5/1k → $1.00。 */
export const PRICE_PER_CALL = 1_000_000;

/** 夹具开局就发给用户的额度。远大于任何一条用例的花费，让额度闸不成为变量。 */
export const GRANT = 1_000_000_000;

/** 门户对账周期（秒）。抓取间隔取得不比它短 —— 当初的漏扣就出在这种配比上。 */
const TICK_SECS = 1;

export interface RealPromMoney {
  portalUrl: string;
  userId: string;
  /** 打一次真实 chat，返回状态码。 */
  chat(): Promise<number>;
  /** 网关至今答成 200 的调用所对应的花费 —— 门户该扣的总数。 */
  spent(): number;
  /** 真重启网关进程（SIGTERM，等它退出，在同一组端口上重新拉起）。 */
  restartGateway(): Promise<void>;
  /** 当前网关进程自报的启动时刻（秒），取自它自己的 /metrics。 */
  gatewayStarted(): Promise<number>;
  /** 门户账面：余额，以及消费流水之和（取正）。 */
  ledger(): Promise<{ balance: number; consumed: number }>;
  /** 门户对账环跑完的轮数。 */
  portalTicks(): Promise<number>;
  /** 某个副本眼里这个用户的花费累计（所有序列之和）；没有序列时为 `null`。 */
  promSpend(replica: number): Promise<number | null>;
  /** 某个副本眼里网关进程的启动时刻（秒）；没有时为 `null`。 */
  promStarted(replica: number): Promise<number | null>;
  /** 某个副本在最近 10 分钟里看到这个用户的花费序列变小过几次（`resets()`）。 */
  promResets(replica: number): Promise<number>;
  /** 某个副本最近一次抓取网关的开始时刻（毫秒）；还没抓过时为 0。 */
  lastScrape(replica: number): Promise<number>;
  /** `since`（毫秒）之后，门户的查询有几次被转给了这个副本。只在多副本时有意义。 */
  readsSince(replica: number, since: number): number;
  stop(): Promise<void>;
}

/**
 * 定位 Prometheus。**没有就失败，不跳过** —— 跳过的话这一套在 CI 上静默变绿，门户
 * 的查询又只剩垫片在回答，而那正是它要补的洞。
 */
function prometheusBin(): string {
  const bin = process.env.PROMETHEUS_BIN;
  const fetchHint =
    `从 https://github.com/prometheus/prometheus/releases/tag/v${PROMETHEUS_VERSION} ` +
    `下载 prometheus-${PROMETHEUS_VERSION}.linux-amd64.tar.gz，按同一页的 sha256sums.txt ` +
    `校验后解开，把 PROMETHEUS_BIN 指到里面的 prometheus 可执行文件。`;
  if (!bin) {
    throw new Error(
      `PROMETHEUS_BIN 没有设置。这一套必须对着真 Prometheus ${PROMETHEUS_VERSION} 跑，` +
        `缺了它就该失败而不是跳过。\n${fetchHint}`,
    );
  }
  const v = spawnSync(bin, ["--version"], { encoding: "utf8" });
  if (v.error || v.status !== 0) {
    throw new Error(
      `PROMETHEUS_BIN=${bin} 跑不起来：${v.error?.message ?? v.stderr}\n${fetchHint}`,
    );
  }
  if (!`${v.stdout}${v.stderr}`.includes(`version ${PROMETHEUS_VERSION} `)) {
    throw new Error(
      `PROMETHEUS_BIN=${bin} 不是 ${PROMETHEUS_VERSION}：\n${v.stdout}${v.stderr}\n${fetchHint}`,
    );
  }
  return bin;
}

/** 进程退出（已经退了就立刻返回）。 */
function exited(p: ChildProcess): Promise<void> {
  if (p.exitCode !== null || p.signalCode !== null) return Promise.resolve();
  return new Promise((r) => p.once("exit", () => r()));
}

/**
 * 拉起门户 + 网关 + `scrapeIntervals.length` 个真 Prometheus（各用自己的抓取间隔，
 * 都抓同一个网关）。用户开局就拿到 [`GRANT`]，预置密钥打通过一次（花了一次的钱）。
 */
export async function startRealPromMoney(scrapeIntervals: string[]): Promise<RealPromMoney> {
  const promBin = prometheusBin();
  assertMoneyBinariesFresh();

  const dir = mkdtempSync(join(tmpdir(), "aisix-money-prom-e2e-"));
  const resourcesPath = join(dir, "resources.yaml");
  const procs: ChildProcess[] = [];
  const servers: Server[] = [];
  let spent = 0;

  const upPort = await freePort();
  const proxyPort = await freePort();
  const metricsPort = await freePort();
  const portalPort = await freePort();
  const portalMetricsPort = await freePort();
  const adminPort = await freePort();
  const metricsUrl = `http://127.0.0.1:${metricsPort}/metrics`;

  const stop = async () => {
    for (const p of procs) p.kill("SIGTERM");
    for (const s of servers) s.close();
    // 等进程真退了再删目录：Prometheus 退出时还在往 TSDB 里写，边写边删会失败。
    await Promise.race([
      Promise.all(procs.map(exited)),
      new Promise((r) => setTimeout(r, 10_000)),
    ]);
    rmSync(dir, { recursive: true, force: true });
  };

  try {
    // 桩上游：固定 usage，让每次调用的花费可预期。
    const upstream = createServer((req, res) => {
      req.resume();
      req.on("end", () => {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            id: "chatcmpl-e2e",
            object: "chat.completion",
            created: 0,
            model: "gpt-4o-mini",
            choices: [
              { index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" },
            ],
            usage: { prompt_tokens: 1000, completion_tokens: 1000, total_tokens: 2000 },
          }),
        );
      });
    });
    await new Promise<void>((r) => upstream.listen(upPort, "127.0.0.1", r));
    servers.push(upstream);

    // 真 Prometheus，每个副本一份配置、一个 TSDB 目录。job 与 instance 两边一样
    // —— 抓的是同一个目标，所以同一条序列在两个副本里标签完全相同，门户把它们
    // 当成同一条序列的两个视图，这正是高可用部署的形态。
    const promUrls: string[] = [];
    for (const [i, interval] of scrapeIntervals.entries()) {
      const port = await freePort();
      const cfg = join(dir, `prometheus-${i}.yml`);
      writeFileSync(
        cfg,
        `global:
  scrape_interval: ${interval}
  scrape_timeout: ${interval}
scrape_configs:
  - job_name: aisix
    static_configs:
      - targets: ["127.0.0.1:${metricsPort}"]
`,
      );
      procs.push(
        spawn(
          promBin,
          [
            `--config.file=${cfg}`,
            `--storage.tsdb.path=${join(dir, `prometheus-${i}-data`)}`,
            `--web.listen-address=127.0.0.1:${port}`,
            "--log.level=warn",
          ],
          { stdio: "inherit" },
        ),
      );
      promUrls.push(`http://127.0.0.1:${port}`);
    }
    for (const u of promUrls) await waitReady(`${u}/-/ready`, "prometheus", (s) => s === 200);

    // 门户查询的入口。一个副本时直连；多个副本时经轮询转发，并记下每次转给了谁。
    const reads: number[][] = promUrls.map(() => []);
    let promEntry = promUrls[0]!;
    if (promUrls.length > 1) {
      let next = 0;
      const rrPort = await freePort();
      const rr = createServer((req, res) => {
        const i = next++ % promUrls.length;
        reads[i]!.push(Date.now());
        fetch(`${promUrls[i]}${req.url ?? "/"}`)
          .then(async (r) => {
            res.writeHead(r.status, {
              "content-type": r.headers.get("content-type") ?? "application/json",
            });
            res.end(Buffer.from(await r.arrayBuffer()));
          })
          .catch(() => {
            res.writeHead(502);
            res.end();
          });
      });
      await new Promise<void>((r) => rr.listen(rrPort, "127.0.0.1", r));
      servers.push(rr);
      promEntry = `http://127.0.0.1:${rrPort}`;
    }

    const portalUrl = `http://127.0.0.1:${portalPort}`;
    writeFileSync(resourcesPath, "api_keys: []\n");
    procs.push(
      spawn(PORTAL_BIN, [], {
        env: {
          ...process.env,
          PORTAL_DB: `sqlite:${join(dir, "portal.db")}`,
          PORTAL_ADDR: `127.0.0.1:${portalPort}`,
          PORTAL_ADMIN_TOKEN: ADMIN_TOKEN,
          PROMETHEUS_URL: promEntry,
          PORTAL_TICK_SECS: String(TICK_SECS),
          PORTAL_METRICS_ADDR: `127.0.0.1:${portalMetricsPort}`,
          AISIX_RESOURCES: resourcesPath,
        },
        stdio: "inherit",
      }),
    );
    await waitReady(`${portalUrl}/api/session`, "aisix-portal");

    const email = "money-prom@e2e.test";
    const password = "correct horse battery";
    const reg = await fetch(`${portalUrl}/api/register`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ email, password }),
    });
    if (reg.status !== 201) throw new Error(`注册失败: ${reg.status}`);
    const userId: string = (await reg.json()).user_id;

    // 先写配置、后发额度：发额度之后门户那一轮会改写这个文件，抢在它前面写
    // 才不会被它拿旧文档覆盖掉。
    writeFileSync(
      resourcesPath,
      `_format_version: "1"
provider_keys:
  - display_name: stub
    provider: openai
    api_key: sk-stub
    api_base: "http://127.0.0.1:${upPort}/v1"
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
    key_hash: "${await sha256Hex(CALLER_KEY)}"
    user_id: "${userId}"
    allowed_models: ["*"]
    disabled: false
`,
    );
    const g = await fetch(`${portalUrl}/admin/users/${userId}/grant`, {
      method: "POST",
      headers: { authorization: `Bearer ${ADMIN_TOKEN}`, "content-type": "application/json" },
      body: JSON.stringify({ micro_usd: GRANT, note: "e2e" }),
    });
    if (g.status !== 200) throw new Error(`发放额度失败: ${g.status}`);

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
    const spawnGateway = () => spawn(GATEWAY_BIN, ["--config", cfgPath], { stdio: "inherit" });
    let gateway = spawnGateway();
    procs.push(gateway);
    await waitReady(metricsUrl, "aisix 网关指标口", (s) => s === 200);

    /**
     * 打一次 chat。每次新建连接：重启前留在连接池里的旧连接在重启后是死的，
     * 复用它会在一个说不清「这一发到底被网关处理了没有」的地方报错 —— 而这一套
     * 要的正是每一发花了多少钱都确定。
     *
     * 连接被拒（刚重启、代理口还没开）说明请求根本没进网关，可以放心重试。
     */
    const chatOnce = (): Promise<number> =>
      new Promise((resolve, reject) => {
        const body = JSON.stringify({
          model: "gpt-4o-mini",
          messages: [{ role: "user", content: "hi" }],
        });
        const req = request(
          {
            host: "127.0.0.1",
            port: proxyPort,
            method: "POST",
            path: "/v1/chat/completions",
            agent: false,
            headers: {
              authorization: `Bearer ${CALLER_KEY}`,
              "content-type": "application/json",
              "content-length": Buffer.byteLength(body),
            },
          },
          (res) => {
            res.resume();
            res.on("end", () => resolve(res.statusCode ?? 0));
            res.on("error", reject);
          },
        );
        req.on("error", reject);
        req.end(body);
      });
    const chat = async (): Promise<number> => {
      for (let i = 0; ; i++) {
        try {
          const status = await chatOnce();
          if (status === 200) spent += PRICE_PER_CALL;
          return status;
        } catch (e) {
          if ((e as NodeJS.ErrnoException).code !== "ECONNREFUSED" || i >= 50) throw e;
          await new Promise((r) => setTimeout(r, 100));
        }
      }
    };

    // 预置密钥要等门户那一轮把它定为启用（发额度之前那一轮可能先把它停了）。
    // 被拒的调用不进上游、不花钱；打通的那一发记进 `spent`。
    for (let i = 0; ; i++) {
      if ((await chat()) === 200) break;
      if (i >= 200) throw new Error("预置密钥 40 秒内没被启用");
      await new Promise((r) => setTimeout(r, 200));
    }

    const login = await fetch(`${portalUrl}/api/login`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ email, password }),
    });
    const cookie = (login.headers.getSetCookie?.() ?? []).map((c) => c.split(";")[0]).join("; ");
    if (!cookie.includes("aisix_portal=")) throw new Error("拿不到门户会话 cookie");

    const promQuery = async (
      replica: number,
      q: string,
    ): Promise<{ metric: Record<string, string>; value: [number, string] }[]> => {
      const r = await fetch(`${promUrls[replica]}/api/v1/query?query=${encodeURIComponent(q)}`);
      const j = await r.json();
      if (j.status !== "success") throw new Error(`Prometheus 查询失败: ${q}: ${JSON.stringify(j)}`);
      return j.data.result;
    };
    const spendSel = `aisix_llm_spend_micro_usd_total{user_id="${userId}"}`;
    const metricLine = async (url: string, name: string): Promise<number> => {
      const text = await fetch(url).then((r) => r.text());
      const line = text.split("\n").find((l) => l.startsWith(`${name} `));
      if (line === undefined) throw new Error(`${url} 里没有 ${name}`);
      return Number(line.slice(name.length).trim());
    };

    return {
      portalUrl,
      userId,
      chat,
      spent: () => spent,
      async restartGateway() {
        gateway.kill("SIGTERM");
        await exited(gateway);
        const old = procs.indexOf(gateway);
        gateway = spawnGateway();
        procs[old] = gateway;
        await waitReady(metricsUrl, "aisix 网关指标口（重启后）", (s) => s === 200);
      },
      gatewayStarted: () => metricLine(metricsUrl, "process_start_time_seconds"),
      async ledger() {
        const b = await fetch(`${portalUrl}/api/balance`, { headers: { cookie } }).then((r) =>
          r.json(),
        );
        const consumed = (b.entries as { delta_micro_usd: number; source: string }[])
          .filter((e) => e.source === "consumption")
          .reduce((n, e) => n - e.delta_micro_usd, 0);
        return { balance: b.balance_micro_usd as number, consumed };
      },
      portalTicks: () =>
        metricLine(
          `http://127.0.0.1:${portalMetricsPort}/metrics`,
          "aisix_portal_reconcile_ticks_total",
        ),
      async promSpend(replica) {
        const r = await promQuery(replica, spendSel);
        return r.length === 0 ? null : r.reduce((n, s) => n + Number(s.value[1]), 0);
      },
      async promStarted(replica) {
        const r = await promQuery(replica, `process_start_time_seconds{job="aisix"}`);
        return r.length === 0 ? null : Number(r[0]!.value[1]);
      },
      async promResets(replica) {
        const r = await promQuery(replica, `resets(${spendSel}[10m])`);
        return r.reduce((n, s) => n + Number(s.value[1]), 0);
      },
      async lastScrape(replica) {
        const j = await fetch(`${promUrls[replica]}/api/v1/targets?state=active`).then((r) =>
          r.json(),
        );
        const t = j.data.activeTargets[0]?.lastScrape as string | undefined;
        const ms = t === undefined ? NaN : Date.parse(t);
        return Number.isFinite(ms) && ms > 0 ? ms : 0;
      },
      readsSince: (replica, since) => reads[replica]!.filter((t) => t > since).length,
      stop,
    };
  } catch (e) {
    await stop();
    throw e;
  }
}
