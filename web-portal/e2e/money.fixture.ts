import { spawn, type ChildProcess } from "node:child_process";
import { createServer, type Server } from "node:http";
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer as netServer } from "node:net";
import { createHash } from "node:crypto";
import { statSync, readdirSync } from "node:fs";

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
  /** 门户自己的指标口。与主端口分开，不经 nginx 暴露。 */
  portalMetricsUrl: string;
  resourcesPath: string;
  userId: string;
  readResources(): string;
  writeResources(body: string): void;
  /** 用夹具预置的密钥打一次真实 chat，返回状态码。 */
  chat(): Promise<number>;
  /** 用任意明文密钥打一次真实 chat —— 用来验自助建的那把能不能调通。 */
  chatWith(plaintext: string): Promise<number>;
  /** 打一次**流式** chat。流式的记账走的是另一条路径。 */
  chatStream(plaintext: string): Promise<number>;
  /** 打一次 `/v1/embeddings`。这个端点的记账路径与 chat 完全不同。 */
  embed(plaintext: string, model?: string): Promise<number>;
  /** 用指定模型打一次 chat（比如语义路由）。`stream` 为真时走 SSE。 */
  chatModel(plaintext: string, model: string, stream?: boolean): Promise<number>;
  /** 桩上游至今收到的嵌入调用次数。 */
  embedCallCount(): number;
  /**
   * 让门户对某把密钥的花费**失明**：PromQL 垫片对这把密钥的查询恒回 0。
   *
   * 用来把两道闸拆开测。门户的持久兜底（累计花费越过额度就把密钥写成停用）会在
   * 一轮内接管，于是「网关自己有没有记账」这件事被盖住 —— 网关侧的记账完全坏掉，
   * 用例照样绿。失明之后只剩网关那道闸，它坏了就立刻看得出来。
   */
  blindPortalTo(keyName: string): void;
  /**
   * 打一次流式 `/v1/messages`（Anthropic 形状）或 `/v1/responses`（Codex 形状）。
   *
   * 这三个端点各有自己的流式记账调用点。只驱动 chat 的话，另两个端点上的同类
   * 漏洞会一直绿着 —— 而它们分别是 Claude Code 与 Codex 的流量入口。
   */
  streamOn(
    endpoint: "messages" | "responses",
    plaintext: string,
    model?: string,
    stream?: boolean,
  ): Promise<number>;
  /**
   * 打一次**非流式**请求，三个端点同形状 —— 用来把同一件事在三条家族路径上
   * 各量一遍。计费问题往往只落在其中一条上，而另两条静默地不一样。
   */
  /**
   * 发一发内容自定的请求。存根会把它回显进应答，所以这是驱动**输出**护栏的
   * 手段：输入护栏在派发前就拦了，量不到「上游已经付过钱之后被拦」。
   */
  saying(
    endpoint: "chat" | "messages" | "responses",
    plaintext: string,
    text: string,
    model?: string,
  ): Promise<number>;
  /**
   * 取一个批处理任务。存根回的是「已完成」，于是网关会去下载输出并事后落账
   * —— 那是与请求路径完全不同的一条计费代码。
   */
  retrieveBatch(plaintext: string, batchId: string, model?: string): Promise<number>;
  /**
   * 同 `saying`，但把应答正文也带回来 —— 用来检查错误信封的**形状**，那是每个
   * 端点自己的协议契约，和状态码是两回事。
   */
  sayingBody(
    endpoint: "chat" | "messages" | "responses",
    plaintext: string,
    text: string,
    model?: string,
  ): Promise<{ status: number; body: string }>;
  /** 同 `saying`，但走流式 —— 流式记账是另一条代码路径。 */
  sayingStream(
    endpoint: "chat" | "messages" | "responses",
    plaintext: string,
    text: string,
    model?: string,
  ): Promise<number>;
  callOn(
    endpoint: "chat" | "messages" | "responses",
    plaintext: string,
    model: string,
  ): Promise<number>;
  /** 真重启网关进程（不是 SIGHUP）。用来量进程内状态在重启后还剩什么。 */
  restartGateway(): Promise<void>;
  /**
   * 给网关发 SIGHUP，让它重读配置。
   *
   * 用例直接改 resources.yaml 时必须自己发这个信号 —— 平时是门户的对账环在
   * 发（它写完配置就发），而直接写文件的用例没有那一步。少了它文件改了网关
   * 不知道，症状是「闸没生效」，跟真 bug 一模一样。
   */
  reloadGateway(): void;
  /** 与 chat 相同，但明确返回状态码供轮询用。 */
  chatWithStatus(): Promise<number>;
  /** 门户会话 cookie，用来调 /api/keys。 */
  sessionCookie(): Promise<string>;
  stop(): void;
}

/**
 * 与网关一致的 api_key 派生 id：`uuid5(命名空间, "api_keys/<display_name>")`。
 *
 * 命名空间与算法抄自 `aisix-core` 的 `derive_id`；这里只在测试里用来指认一把
 * 密钥，不参与任何生产路径。
 */
function deriveKeyId(displayName: string): string {
  const NS = Buffer.from("63e50ab2677a54d38d1ec0cb29ceae94", "hex");
  const h = createHash("sha1");
  h.update(NS);
  h.update(Buffer.from(`api_keys/${displayName}`, "utf8"));
  const b = h.digest();
  b[6] = (b[6]! & 0x0f) | 0x50;
  b[8] = (b[8]! & 0x3f) | 0x80;
  const hex = b.subarray(0, 16).toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}

/** sha256 —— 与网关 `key_hash` 的算法一致。 */
async function sha256Hex(s: string): Promise<string> {
  const b = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(s));
  return [...new Uint8Array(b)].map((x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * 二进制比源码新，否则立刻报错。
 *
 * 这一条咬过三次：改完 Rust 只重建了 release 而夹具跑的是 debug、变异让 TS 编译
 * 失败而测试跑的是上一次的产物……症状都一样 —— 测试结果看起来在讲代码，其实在讲
 * 一个旧文件。而它长得跟真 bug 一模一样，每次都要花很久才想到。
 *
 * 与其记得重建，不如让它自己说出来。
 */
function assertFresh(bin: string, crateDirs: string[]): void {
  let binTime: number;
  try {
    binTime = statSync(bin).mtimeMs;
  } catch {
    throw new Error(`找不到 ${bin}\n先构建它，再跑 e2e。`);
  }
  let newest = 0;
  let newestFile = "";
  const walk = (dir: string) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return; // 这个 crate 没有这一层目录
    }
    for (const e of entries) {
      const full = join(dir, e.name);
      // `src/bin/` 是这个 crate 自己的附属可执行文件，没有别的 crate 链接它们
      // —— 被测的二进制都从 `src/main.rs` 进。走进去的话，改一句
      // `dump-schema` 的注释就会说门户二进制过期（它俩毫无关系），而重建也
      // 消不掉：`cargo build -p aisix-portal` 不会碰那个文件。
      if (e.isDirectory()) {
        if (e.name !== "bin") walk(full);
      } else if (
        e.name.endsWith(".rs") ||
        e.name.endsWith(".sql") ||
        e.name.endsWith(".json")
      ) {
        const t = statSync(full).mtimeMs;
        if (t > newest) {
          newest = t;
          newestFile = full;
        }
      }
    }
  };
  // **只看进二进制的那些文件。**
  //
  // 原先是把整个 crate 目录走一遍，于是 `tests/` 里的文件也算进来 —— 改一个
  // 集成测试就会说网关二进制过期，逼着重建，而那个文件压根不进二进制。这条误报
  // 真的挡掉过整套钱路的 16 条用例：闸响了，但讲的不是事实。
  //
  // `Cargo.toml` 要算：换依赖确实会改二进制。`migrations/` 也算：门户把它编进去。
  //
  // `schemas/resources/*.schema.json` 同样要算 —— 它们是被 `include_str!` 编进
  // 二进制的，改了 schema 而不重建，配置校验的行为已经变了而闸不会响。
  //
  // 反过来，`src/bin/` 不算（见 `walk` 里的判断）。这条闸的价值全在「它响的时候
  // 讲的是事实」；误报一次就会被绕过一次，之后真的过期也不会有人信它。
  walk(join(process.cwd(), "..", "schemas"));
  for (const d of crateDirs) {
    walk(join(d, "src"));
    walk(join(d, "migrations"));
    try {
      const t = statSync(join(d, "Cargo.toml")).mtimeMs;
      if (t > newest) {
        newest = t;
        newestFile = join(d, "Cargo.toml");
      }
    } catch {
      /* 不是 crate 目录 */
    }
  }
  if (newest > binTime) {
    throw new Error(
      `${bin} 比源码旧（${newestFile} 更新）——` +
        `跑的是上一次构建的产物，结果讲的不是当前代码。\n` +
        `先重建：cargo build${bin.includes("/release/") ? " --release" : ""} -p <crate>`,
    );
  }
}

export async function startMoney(): Promise<MoneyFixture> {
  // 按**实际依赖**分开比，不是笼统地比整个 `crates/`。
  //
  // 只盯自己那一个目录会漏：网关受 aisix-core、aisix-ratelimit 影响（那次流式
  // 记账的修复就在 ratelimit 里）。笼统地比整个 crates/ 又会误报：改门户跟网关
  // 二进制没关系，却逼你重建它。
  const crates = join(process.cwd(), "..", "crates");
  const all = readdirSync(crates, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => join(crates, e.name));
  const portalDeps = all.filter((d) =>
    /aisix-(portal|core)$/.test(d),
  );
  // 网关不依赖门户与控制台，其余都算上。
  const gatewayDeps = all.filter((d) => !/aisix-(portal|console)$/.test(d));
  assertFresh(PORTAL_BIN, portalDeps);
  assertFresh(GATEWAY_BIN, gatewayDeps);

  const dir = mkdtempSync(join(tmpdir(), "aisix-money-e2e-"));
  const resourcesPath = join(dir, "resources.yaml");
  const procs: ChildProcess[] = [];
  const servers: Server[] = [];

  const upPort = await freePort();
  const proxyPort = await freePort();
  const metricsPort = await freePort();
  const promPort = await freePort();
  const portalPort = await freePort();
  const portalMetricsPort = await freePort();

  /** 桩上游收到的嵌入调用次数。语义路由每次分类都会打一发。 */
  let embedCalls = 0;

  // 桩上游：OpenAI 兼容的应答，带固定 usage 让花费可预期。
  //
  // 请求带 `stream: true` 时回 SSE。流式与非流式在网关里走的是**两条**记账
  // 路径（流结束的回调 vs 拿到完整应答时提交），只测一条就会让另一条上的漏洞
  // 一直绿着 —— 而流式是 LLM 客户端的主流模式。
  const upstream = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      let wantsStream = false;
      try {
        wantsStream = JSON.parse(body || "{}").stream === true;
      } catch {
        /* 不是 JSON 就按非流式回 */
      }
      // 应答回显请求里的用户内容：输出护栏检的是应答，不回显就没法只让某一发
      // 触发它 —— 固定回 "ok" 的话，要么全部拦、要么一条都拦不到。三种入站形状
      // 各有各的取法（chat/messages 是 messages[]，responses 是 input）。
      let echoed = "ok";
      try {
        const j = JSON.parse(body || "{}");
        const msgs = j.messages;
        const last = Array.isArray(msgs) ? msgs[msgs.length - 1] : undefined;
        if (typeof last?.content === "string") echoed = last.content;
        else if (typeof j.input === "string") echoed = j.input;
      } catch {
        /* 不是 JSON 就回 ok */
      }
      // 批处理：retrieve 回一个已完成的批次，输出文件里每行带自己的 usage。
      // 网关首次看到 completed 时会把输出下载下来聚合 —— 这条路径上的钱是
      // 「事后落账」的，和请求路径完全不同。
      if ((req.url ?? "").includes("/batches/")) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            id: "batch_e2e",
            object: "batch",
            status: "completed",
            output_file_id: "file-out-e2e",
            endpoint: "/v1/chat/completions",
          }),
        );
        return;
      }
      if ((req.url ?? "").includes("/files/file-out-e2e/content")) {
        res.writeHead(200, { "content-type": "application/x-ndjson" });
        // 两行，各 1000 输入 + 1000 输出 —— 配上 $0.5/1k 的定价，整批 $2.00。
        const line = (i: number) =>
          JSON.stringify({
            id: `req-${i}`,
            response: {
              status_code: 200,
              body: {
                model: "gpt-4o-mini",
                usage: { prompt_tokens: 1000, completion_tokens: 1000, total_tokens: 2000 },
              },
            },
          });
        res.end(`${line(1)}\n${line(2)}\n`);
        return;
      }
      // 只有这个模型的应答带缓存计数 —— 见下面 usage 处的说明。
      let wantsAnthropicCacheCounters = false;
      try {
        wantsAnthropicCacheCounters =
          JSON.parse(body || "{}").model === "claude-cached";
      } catch {
        /* 不是 JSON 就当没有 */
      }
      // Anthropic 原生的流式：事件名和 OpenAI 那套完全不同，用量在
      // message_start（输入）与 message_delta（输出）两处。回 OpenAI 形状的话
      // 网关解析不出用量，会退到估算 —— 金额变成几千 micro，测不出真问题。
      if ((req.url ?? "").includes("/v1/messages") && wantsStream) {
        res.writeHead(200, {
          "content-type": "text/event-stream",
          "cache-control": "no-cache",
        });
        const ev = (type: string, data: unknown) =>
          res.write(`event: ${type}\ndata: ${JSON.stringify(data)}\n\n`);
        ev("message_start", {
          type: "message_start",
          message: {
            id: "msg-e2e",
            type: "message",
            role: "assistant",
            model: "claude-sonnet-4",
            content: [],
            usage: { input_tokens: 1000, output_tokens: 0 },
          },
        });
        ev("content_block_start", {
          type: "content_block_start",
          index: 0,
          content_block: { type: "text", text: "" },
        });
        ev("content_block_delta", {
          type: "content_block_delta",
          index: 0,
          delta: { type: "text_delta", text: echoed },
        });
        ev("content_block_stop", { type: "content_block_stop", index: 0 });
        ev("message_delta", {
          type: "message_delta",
          delta: { stop_reason: "end_turn" },
          usage: { output_tokens: 1000 },
        });
        ev("message_stop", { type: "message_stop" });
        res.end();
        return;
      }
      // Anthropic 原生形状：content 是块数组，usage 叫 input_tokens/output_tokens。
      // 回 OpenAI 形状的话网关解析不出用量，会退到估算，金额对不上。
      if ((req.url ?? "").includes("/v1/messages") && !wantsStream) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            id: "msg-e2e",
            type: "message",
            role: "assistant",
            model: "claude-sonnet-4",
            content: [{ type: "text", text: echoed }],
            stop_reason: "end_turn",
            // 缓存计数只给专用的那个模型：共用的 anthropic-direct 是别的用例
            // 用来量「一次普通调用花多少」的基准，给它加上计数会把那些用例的
            // 金额一起改掉（第一版就是这么写的，把护栏计费那条弄红了）。
            //
            // Anthropic 把缓存读/写记在**独立**的计数器里（不像 OpenAI 是
            // prompt_tokens 的子集）。
            usage: {
                input_tokens: 1000,
                output_tokens: 1000,
                ...(wantsAnthropicCacheCounters
                  ? { cache_read_input_tokens: 2000, cache_creation_input_tokens: 400 }
                  : {}),
            },
          }),
        );
        return;
      }
      // 嵌入接口是另一种形状：没有 choices，usage 只有 prompt/total。
      if ((req.url ?? "").includes("/embeddings")) {
        embedCalls += 1;
        // 一个「坏掉的」嵌入模型：用来量分类失败之后的降级路径。上游报错时
        // 网关拿不到任何用量，所以那一发不该记任何花费。
        try {
          if (JSON.parse(body || "{}").model === "text-embedding-broken") {
            res.writeHead(500, { "content-type": "application/json" });
            res.end(JSON.stringify({ error: { message: "embedding upstream is down" } }));
            return;
          }
        } catch {
          /* 不是 JSON 就按正常走 */
        }
        // **每个输入回一个向量**：语义路由会把 prompt 与所有未缓存的样例批在
        // 一次调用里，回少了网关会正确地判成畸形应答并整条降级 —— 那样就测不到
        // 分类路径了。
        let n = 1;
        try {
          const inp = JSON.parse(body || "{}").input;
          n = Array.isArray(inp) ? inp.length : 1;
        } catch {
          /* 按 1 个算 */
        }
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            object: "list",
            model: "embed-1",
            data: Array.from({ length: n }, (_, i) => ({
              object: "embedding",
              index: i,
              // 全部相同：余弦相似度恒为 1，于是必定命中那条路由。
              embedding: [0.1, 0.2, 0.3],
            })),
            // 1000 输入 token，配上 $0.5/1k 的定价 → 单次 $0.50。
            usage: { prompt_tokens: 1000, total_tokens: 1000 },
          }),
        );
        return;
      }
      // 非流式的 `/v1/responses` 也是自己一套形状：usage 在顶层、叫
      // input_tokens/output_tokens。回 chat 形状的话网关解析不到，会退到 token
      // 估算 —— 金额变成几千 micro，于是「按父行定价恒 0」这种问题在对照里
      // 看不出来（两边都是估算值，还相等）。
      if ((req.url ?? "").includes("/responses") && !wantsStream) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            id: "resp-e2e",
            object: "response",
            created_at: 0,
            model: "gpt-4o-mini",
            status: "completed",
            output: [
              {
                type: "message",
                id: "msg-e2e",
                role: "assistant",
                status: "completed",
                content: [{ type: "output_text", text: echoed }],
              },
            ],
            usage: { input_tokens: 1000, output_tokens: 1000, total_tokens: 2000 },
          }),
        );
        return;
      }
      if (wantsStream) {
        res.writeHead(200, {
          "content-type": "text/event-stream",
          "cache-control": "no-cache",
        });
        // Responses API 的流式是另一套事件：usage 挂在 `response.completed`
        // 事件里的 `response` 对象下。回 chat 形状的分块，网关解析不到 usage，
        // 会退到 token 估算 —— 花费从 $1.00 变成 $0.004，于是额度用例永远等不
        // 到那个 429，看起来像「这个端点漏了记账」。
        if ((req.url ?? "").includes("/responses")) {
          res.write(
            `data: ${JSON.stringify({
              type: "response.output_text.delta",
              delta: "ok",
            })}\n\n`,
          );
          res.end(
            `data: ${JSON.stringify({
              type: "response.completed",
              response: {
                id: "resp-e2e",
                model: "gpt-4o-mini",
                status: "completed",
                output: [
                  {
                    type: "message",
                    role: "assistant",
                    content: [{ type: "output_text", text: echoed }],
                  },
                ],
                usage: {
                  input_tokens: 1000,
                  output_tokens: 1000,
                  total_tokens: 2000,
                },
              },
            })}\n\n`,
          );
          return;
        }
        const chunk = (delta: Record<string, unknown>, extra = {}) =>
          `data: ${JSON.stringify({
            id: "chatcmpl-e2e",
            object: "chat.completion.chunk",
            created: 0,
            model: "gpt-4o-mini",
            choices: [{ index: 0, delta, finish_reason: null }],
            ...extra,
          })}\n\n`;
        res.write(chunk({ role: "assistant", content: echoed }));
        // 末块带 usage：与非流式同一组数字，所以单次花费同样是 $1.00。
        res.write(
          `data: ${JSON.stringify({
            id: "chatcmpl-e2e",
            object: "chat.completion.chunk",
            created: 0,
            model: "gpt-4o-mini",
            choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
            usage: { prompt_tokens: 1000, completion_tokens: 1000, total_tokens: 2000 },
          })}\n\n`,
        );
        res.end("data: [DONE]\n\n");
        return;
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          id: "chatcmpl-e2e",
          object: "chat.completion",
          created: 0,
          model: "gpt-4o-mini",
          choices: [
            { index: 0, message: { role: "assistant", content: echoed }, finish_reason: "stop" },
          ],
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
  /** 门户对这些 api_key_id 的花费查询恒回 0。见 blindPortalTo。 */
  const blind = new Set<string>();
  const prom = createServer((req, res) => {
    void (async () => {
      const u = new URL(req.url ?? "/", "http://x");
      const q = u.searchParams.get("query") ?? "";
      // 门户会按 user_id 查（用户级）也会按 api_key_id 查（密钥级持久兜底）。
      const m =
        /user_id="([^"]+)"/.exec(q) ?? /api_key_id="([^"]+)"/.exec(q);
      const label = q.includes("api_key_id=") ? "api_key_id" : "user_id";
      const id = m?.[1] ?? "";
      let cumulative = 0;
      // 有没有匹配到任何序列。真 Prometheus 在一条都没匹配到时回**空** result，
      // 而不是一条值为 0 的序列 —— 那两种情况在门户侧的处置完全不同（「这个人
      // 从来没调过」vs「读不到」）。垫片必须照着真的来，否则那条分支永远测不到。
      if (label === "api_key_id" && blind.has(id)) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            status: "success",
            data: { resultType: "vector", result: [{ metric: {}, value: [0, "0"] }] },
          }),
        );
        return;
      }
      let matched = false;
      try {
        const text = await fetch(`http://127.0.0.1:${metricsPort}/metrics`).then((r) => r.text());
        for (const line of text.split("\n")) {
          if (!line.startsWith("aisix_llm_spend_micro_usd_total{")) continue;
          if (!line.includes(`${label}="${id}"`)) continue;
          matched = true;
          cumulative += Number(line.slice(line.lastIndexOf("}") + 1).trim()) || 0;
        }
      } catch {
        /* 网关还没起来 */
      }
      if (!matched) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            status: "success",
            data: { resultType: "vector", result: [] },
          }),
        );
        return;
      }
      const prev = lastSeen.get(`${label}:${id}`) ?? 0;
      const delta = Math.max(0, cumulative - prev);
      lastSeen.set(`${label}:${id}`, cumulative);
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
        // 这一套给同一个用户连着铸十几把密钥，生产默认的每分钟十把会把它卡住。
        // 限频本身由门户的单测覆盖，这里放开只是为了让钱路用例跑得完。
        PORTAL_MINT_MAX_PER_MINUTE: "1000",
        PORTAL_METRICS_ADDR: `127.0.0.1:${portalMetricsPort}`,
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

  // 按名字持有，不靠下标：门户是先起的（要先注册用户才知道 user_id），
  // 下标取错的症状是「SIGHUP 发给了门户」，而门户不重读网关配置 —— 表现成
  // 「闸没生效」，跟真 bug 一模一样。
  let gateway = spawn(GATEWAY_BIN, ["--config", cfgPath], { stdio: "inherit" });
  procs.push(gateway);
  const metricsUrl = `http://127.0.0.1:${metricsPort}/metrics`;
  await waitReady(metricsUrl, "aisix 网关指标口", (s) => s === 200);

  const sayingOn = async (
    endpoint: "chat" | "messages" | "responses",
    plaintext: string,
    text: string,
    model: string,
    stream: boolean,
  ): Promise<number> => {
    const [path, payload] =
      endpoint === "chat"
        ? [
            "/v1/chat/completions",
            { model, messages: [{ role: "user", content: text }], ...(stream ? { stream: true } : {}) },
          ]
        : endpoint === "messages"
          ? [
              "/v1/messages",
              {
                model,
                max_tokens: 16,
                messages: [{ role: "user", content: text }],
                ...(stream ? { stream: true } : {}),
              },
            ]
          : ["/v1/responses", { model, input: text, ...(stream ? { stream: true } : {}) }];
    const r = await fetch(`http://127.0.0.1:${proxyPort}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json" },
      body: JSON.stringify(payload),
    });
    // 记账在流结束的回调里，所以必须把 body 读完。
    await r.text();
    return r.status;
  };

  const callNonChat = async (
    endpoint: "messages" | "responses",
    plaintext: string,
    model: string,
    stream: boolean,
  ): Promise<number> => {
    const body =
      endpoint === "messages"
        ? {
            model,
            max_tokens: 16,
            ...(stream ? { stream: true } : {}),
            messages: [{ role: "user", content: "hi" }],
          }
        : { model, ...(stream ? { stream: true } : {}), input: "hi" };
    const r = await fetch(`http://127.0.0.1:${proxyPort}/v1/${endpoint}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${plaintext}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    // 记账在流结束的回调里，所以必须把 body 读完。
    await r.text();
    return r.status;
  };

  return {
    proxyUrl: `http://127.0.0.1:${proxyPort}`,
    metricsUrl,
    portalUrl,
    portalMetricsUrl: `http://127.0.0.1:${portalMetricsPort}/metrics`,
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
    async chatStream(plaintext: string) {
      return chatWith(proxyPort, plaintext, true);
    },
    async retrieveBatch(plaintext: string, batchId: string, model = "gpt-4o-mini") {
      const r = await fetch(
        `http://127.0.0.1:${proxyPort}/v1/batches/${batchId}?model=${model}`,
        { headers: { authorization: `Bearer ${plaintext}` } },
      );
      await r.text();
      return r.status;
    },
    async sayingStream(
      endpoint: "chat" | "messages" | "responses",
      plaintext: string,
      text: string,
      model = "gpt-4o-mini",
    ) {
      return await sayingOn(endpoint, plaintext, text, model, true);
    },
    async saying(
      endpoint: "chat" | "messages" | "responses",
      plaintext: string,
      text: string,
      model = "gpt-4o-mini",
    ) {
      return await sayingOn(endpoint, plaintext, text, model, false);
    },
    async sayingBody(
      endpoint: "chat" | "messages" | "responses",
      plaintext: string,
      text: string,
      model = "gpt-4o-mini",
    ) {
      const [path, payload] =
        endpoint === "chat"
          ? ["/v1/chat/completions", { model, messages: [{ role: "user", content: text }] }]
          : endpoint === "messages"
            ? [
                "/v1/messages",
                { model, max_tokens: 16, messages: [{ role: "user", content: text }] },
              ]
            : ["/v1/responses", { model, input: text }];
      const r = await fetch(`http://127.0.0.1:${proxyPort}${path}`, {
        method: "POST",
        headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json" },
        body: JSON.stringify(payload),
      });
      return { status: r.status, body: await r.text() };
    },
    async chatModel(plaintext: string, model: string, stream = false) {
      const r = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${plaintext}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model,
          messages: [{ role: "user", content: "hello" }],
          ...(stream ? { stream: true } : {}),
        }),
      });
      await r.text();
      return r.status;
    },
    embedCallCount: () => embedCalls,
    async embed(plaintext: string, model = "embed-1") {
      const r = await fetch(`http://127.0.0.1:${proxyPort}/v1/embeddings`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${plaintext}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ model, input: "hello" }),
      });
      await r.text();
      return r.status;
    },
    blindPortalTo(keyName: string) {
      blind.add(deriveKeyId(keyName));
    },
    async callOn(endpoint: "chat" | "messages" | "responses", plaintext: string, model: string) {
      if (endpoint !== "chat") return await callNonChat(endpoint, plaintext, model, false);
      const r = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${plaintext}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ model, messages: [{ role: "user", content: "hello" }] }),
      });
      await r.text();
      return r.status;
    },
    async streamOn(endpoint, plaintext, model = "gpt-4o-mini", stream = true) {
      return await callNonChat(endpoint, plaintext, model, stream);
    },
    reloadGateway() {
      gateway.kill("SIGHUP");
    },
    async restartGateway() {
      // 真重启，不是 SIGHUP：SIGHUP 只让进程重读配置，进程内的计数器原样还在。
      // 累计额度那个「只增不减」的计数器住在哪、活多久，只有真重启才量得出来。
      const old = procs.indexOf(gateway);
      gateway.kill("SIGTERM");
      await new Promise<void>((r) => gateway.once("exit", () => r()));
      gateway = spawn(GATEWAY_BIN, ["--config", cfgPath], { stdio: "inherit" });
      if (old >= 0) procs[old] = gateway;
      else procs.push(gateway);
      await waitReady(metricsUrl, "aisix 网关指标口（重启后）", (st) => st === 200);
    },
    async chatWithStatus() {
      return chatWith(proxyPort, CALLER_KEY);
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

/** 打一次真实的 chat。`stream` 为真时走 SSE。 */
async function chatWith(
  proxyPort: number,
  plaintext: string,
  stream = false,
): Promise<number> {
  const r = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json" },
    body: JSON.stringify({
      model: "gpt-4o-mini",
      messages: [{ role: "user", content: "hi" }],
      ...(stream ? { stream: true } : {}),
    }),
  });
  // 流式必须把 body 读完：网关的记账发生在**流结束**的回调里，提前放手就
  // 什么都没记，而断言会因此在一个不确定的时刻通过或失败。
  await r.text();
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
  # Anthropic 原生上游：打到它的 /v1/messages 走的是穿透分派，与打到
  # OpenAI 模型时的跨供应商桥接是两条代码路径。Claude Code 的真实形状是这条。
  - display_name: stub-anthropic
    provider: anthropic
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
  - display_name: embed-1
    provider: openai
    provider_key: stub
    model_name: text-embedding-3-small
    embedding:
      dimensions: 3
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  - display_name: routed-1
    routing:
      strategy: round_robin
      targets:
        - model: gpt-4o-mini
  - display_name: semantic-cached
    semantic:
      embedding_model: embed-1
      default: gpt-4o-mini
      match:
        threshold: 0.99
      routes:
        - name: only
          target: gpt-4o-mini
          examples: ["hello"]
  # 没配价的直连模型 + 一个指到它的路由组：用来量「通过虚拟父级调度到未定价
  # 目标」这个盲区。
  - display_name: unpriced-1
    provider: openai
    provider_key: stub
    model_name: gpt-4o-mini
  # 两个目标的路由组：用来量「被输出护栏拦下时会不会向下一个目标转移」——
  # 会的话一次被拦的请求就打了多次上游，钱是双倍的。
  - display_name: routed-two
    routing:
      strategy: round_robin
      targets:
        - model: gpt-4o-mini
        - model: cached-direct
  - display_name: routed-unpriced
    routing:
      strategy: round_robin
      targets:
        - model: unpriced-1
  # 另起一个开缓存的嵌入模型：embed-1 兼着语义路由的分类调用，给它开缓存会让
  # 「分类那一发照收」那条用例的第二发命中，量的就不是同一件事了。
  - display_name: anthropic-direct
    provider: anthropic
    provider_key: stub-anthropic
    model_name: claude-sonnet-4
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  # 专供「缓存读/写各按各价」那条用例。缓存读比输入便宜、缓存写比输入贵 ——
  # 两个方向都给个和输入不同的数，这样「按输入价一刀切」会落在一个错的金额
  # 上，而不是碰巧相等。
  - display_name: anthropic-cached
    provider: anthropic
    provider_key: stub-anthropic
    model_name: claude-cached
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
      cached_input_per_1k: 0.05
      cache_write_per_1k: 1.25
  - display_name: embed-cached
    provider: openai
    provider_key: stub
    model_name: text-embedding-3-small
    embedding:
      dimensions: 3
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  - display_name: cached-direct
    provider: openai
    provider_key: stub
    model_name: gpt-4o-mini
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  # 带自身上限的分类模型 + 用它的语义路由：量「分类那一发过不过它自己的闸」。
  # 每次分类调用 1000 提示 token，所以 tpm 1500 只够一发。
  - display_name: embed-capped
    provider: openai
    provider_key: stub
    model_name: text-embedding-3-small
    embedding:
      dimensions: 3
    rate_limit:
      tpm: 1500
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  - display_name: semantic-capped
    semantic:
      embedding_model: embed-capped
      default: gpt-4o-mini
      match:
        threshold: 0.99
      on_embedding_failure: default
      routes:
        - name: only
          target: cached-direct
          examples: ["hello"]
  # 分类会失败的语义路由：嵌入模型的上游固定报 500。
  - display_name: embed-broken
    provider: openai
    provider_key: stub
    model_name: text-embedding-broken
    embedding:
      dimensions: 3
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  - display_name: semantic-broken
    semantic:
      embedding_model: embed-broken
      default: gpt-4o-mini
      match:
        threshold: 0.99
      on_embedding_failure: default
      routes:
        - name: only
          target: cached-direct
          examples: ["hello"]
  # 「按路由名写的策略要管住它的分类子调用」那条用例专用的一组。分类模型自己
  # **不带**内联上限 —— 唯一能管住它的是那条按父级名字匹配的策略，所以这一格
  # 只有在预留带上了父级身份时才会生效。
  - display_name: embed-parented
    provider: openai
    provider_key: stub
    model_name: text-embedding-3-small
    embedding:
      dimensions: 3
    cost:
      input_per_1k: 0.5
      output_per_1k: 0.5
  # 路由目标刻意用 Anthropic 那个模型：策略要只管住分类那一发，而 provider
  # 是唯一没有「父级回落」的维度 —— 分类走 openai、chat 目标走 anthropic，条件
  # 加上 provider 就把两次预留分开了。不分开的话策略会把 chat 目标也拒掉，整条
  # 请求变成 429，而那说明不了父级身份有没有带上。
  - display_name: semantic-parented
    semantic:
      embedding_model: embed-parented
      default: anthropic-direct
      match:
        threshold: 0.99
      on_embedding_failure: default
      routes:
        - name: only
          target: anthropic-direct
          examples: ["hello"]
  - display_name: semantic-1
    semantic:
      embedding_model: embed-1
      default: gpt-4o-mini
      match:
        threshold: 0.99
      routes:
        - name: only
          target: gpt-4o-mini
          examples: ["hello"]
cache_policies:
  # 只作用于 semantic-cached：全局开缓存会让其它用例的重复请求命中缓存，
  # 它们量的是「每次调用记多少钱」，一命中就全变 0。
  - name: cache-semantic-only
    enabled: true
    ttl_seconds: 300
    applies_to: "model:semantic-cached"
  # 一个「普通直连模型 + 开缓存」的格子，专供三个端点对齐用例：语义那条同时
  # 带着分类调用的花费，量不出「命中本身收了多少」。
  - name: cache-direct-only
    enabled: true
    ttl_seconds: 300
    applies_to: "model:cached-direct"
  - name: cache-embed-only
    enabled: true
    ttl_seconds: 300
    applies_to: "model:embed-cached"
guardrails:
  # 文件模式没有附着集合，护栏是全环境生效的 —— 所以这条只能靠"应答里出现
  # 这个短语"来触发，别的用例发的是 hello / hi，不会命中。
  - name: block-that-phrase
    kind: keyword
    hook_point: output
    patterns:
      - kind: literal
        value: forbidden-output-phrase
rate_limit_policies:
  # 条件式策略：带模型属性条件的行会延到「分派目标」阶段再判。它写的是**路由
  # 的名字**，所以只能通过预留里带着的父级身份匹配上 —— 分类子调用自己的模型
  # 名是 embed-parented，对不上。
  - name: only-this-routers-classifier
    conditions:
      - dimension: model_name
        operator: "=="
        value: semantic-parented
      # provider 没有父级回落，所以这一条只有分类那一发满足（它走 openai），
      # chat 目标走 anthropic，被排除在外。
      - dimension: provider
        operator: "=="
        value: openai
    limits:
      tpm: 1500
api_keys:
  - display_name: e2e-caller
    key_hash: "${keyHash}"
    user_id: "${userId}"
    allowed_models: ["*"]
    disabled: ${disabled}
`,
  );
}
