import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  pickFreePort,
  spawnApp,
  startA2aUpstream,
  waitConfigPropagation,
  type A2aUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：A2A 的输出护栏同样管住流式回答与推送通知 —— 换一种方式取回答，绕不过去。
//
// `message/send` 的回答要过输出护栏，但同一个回答还有三条路到调用方手里：
//
// - `message/stream`：agent 边做边推，回答在 artifact 事件里一段段送达；
// - `tasks/resubscribe`：对一个进行中的任务重新订阅它的事件流；
// - 推送通知：调用方在请求里（或用 `tasks/pushNotificationConfig/set`）留一个
//   webhook，agent 直接把任务进展 POST 过去，根本不经过网关。
//
// 流式回答要先审后放：被拦的内容一个字也不能先到调用方手里，拆在两个事件里
// 的被拦词也一样；agent 发完最后一个事件却不关连接时，调用方照样要在那个事件
// 上收到结果，而不是一直等下去。推送通知网关看不到，只能不让它被设上 ——
// 输出护栏在场时，设推送的请求按协议的「不支持推送通知」错误拒掉，agent 收不到。
//
// 两个 wire 版本各跑一遍（1.0 的方法名是 PascalCase，结果包在 payload 里）。
// 每个拒绝都有对照：干净的流照常完整送达；没有输出护栏时推送设置照常转发。

const KEY = "sk-a2a-guardrail-stream";
const MARKER = "a2astreamforbiddenword";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

type Wire = "0.3" | "1.0";
const WIRES: Wire[] = ["0.3", "1.0"];
const SEND: Record<Wire, string> = { "0.3": "message/send", "1.0": "SendMessage" };
const STREAM: Record<Wire, string> = { "0.3": "message/stream", "1.0": "SendStreamingMessage" };
const RESUBSCRIBE: Record<Wire, string> = { "0.3": "tasks/resubscribe", "1.0": "SubscribeToTask" };
const SET_PUSH: Record<Wire, string> = {
  "0.3": "tasks/pushNotificationConfig/set",
  "1.0": "CreateTaskPushNotificationConfig",
};
const GET_PUSH: Record<Wire, string> = {
  "0.3": "tasks/pushNotificationConfig/get",
  "1.0": "GetTaskPushNotificationConfig",
};
/** 请求里设推送的字段：0.3 叫 `pushNotificationConfig`，1.0 叫 `taskPushNotificationConfig`。 */
const PUSH_FIELD: Record<Wire, string> = {
  "0.3": "pushNotificationConfig",
  "1.0": "taskPushNotificationConfig",
};
const WEBHOOK = "https://hooks.example.test/a2a-updates";
/** A2A 规范为「agent 不支持推送通知」定义的 JSON-RPC 错误码。 */
const PUSH_NOT_SUPPORTED = -32003;

const textPart = (v: Wire, text: string) => (v === "0.3" ? { kind: "text", text } : { text });
const message = (v: Wire, text: string) =>
  v === "0.3"
    ? { kind: "message", messageId: randomUUID(), role: "user", parts: [textPart(v, text)] }
    : { messageId: randomUUID(), role: "ROLE_USER", parts: [textPart(v, text)] };

const CLEAN_CHUNKS = ["The quarterly figures are", " all in order."];
/** 被拦词整个落在一个 artifact 事件里。 */
const RISKY_CHUNKS = ["Here is the finding:", ` case seven ${MARKER} confirmed.`];
/** 被拦词拆在两个相邻的 artifact 事件里，客户端拼起来才是完整的词。 */
const SPLIT_CHUNKS = [`The code word is ${MARKER.slice(0, 9)}`, `${MARKER.slice(9)} and nothing else.`];
/** agent 收尾时的状态消息：收到它，说明流走到了正常结束。 */
const CLOSING = "Report generated.";

interface SseEvent {
  at: number;
  data: Record<string, unknown>;
}

/**
 * 读一个 SSE 响应，直到流结束、`stop` 认定可以停、或超过 `deadlineMs`。
 * 记下每个 data 事件的到达时间。
 */
async function readSse(
  res: Response,
  stop: (e: SseEvent) => boolean = () => false,
  deadlineMs = 15_000,
): Promise<{ events: SseEvent[]; wire: string; timedOut: boolean }> {
  const reader = res.body!.getReader();
  const decoder = new TextDecoder();
  const events: SseEvent[] = [];
  let wire = "";
  let buf = "";
  const deadline = Date.now() + deadlineMs;
  for (;;) {
    const left = deadline - Date.now();
    if (left <= 0) {
      await reader.cancel();
      return { events, wire, timedOut: true };
    }
    const next = await Promise.race([
      reader.read(),
      new Promise<"timeout">((r) => setTimeout(() => r("timeout"), left)),
    ]);
    if (next === "timeout") {
      await reader.cancel();
      return { events, wire, timedOut: true };
    }
    if (next.done) return { events, wire, timedOut: false };
    const chunk = decoder.decode(next.value, { stream: true });
    wire += chunk;
    buf += chunk;
    let cut: number;
    while ((cut = buf.indexOf("\n\n")) >= 0) {
      const frame = buf.slice(0, cut);
      buf = buf.slice(cut + 2);
      const data = frame
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).trimStart())
        .join("\n");
      if (!data) continue;
      const event = { at: Date.now(), data: JSON.parse(data) as Record<string, unknown> };
      events.push(event);
      if (stop(event)) {
        await reader.cancel();
        return { events, wire, timedOut: false };
      }
    }
  }
}

/** 流里所有 artifact 事件的文本，按到达顺序拼起来（两个 wire 版本都认）。 */
function artifactText(events: SseEvent[]): string {
  return events
    .map((e) => e.data.result as Record<string, unknown> | undefined)
    .map((r) => (r?.artifactUpdate ?? r) as Record<string, unknown> | undefined)
    .filter((r) => r?.artifact !== undefined)
    .flatMap((r) => ((r!.artifact as { parts: Array<{ text?: string }> }).parts ?? []))
    .map((p) => p.text ?? "")
    .join("");
}

const isError = (e: SseEvent) => e.data.error !== undefined;

describe("A2A output guardrails hold streamed answers back and refuse push delivery", () => {
  let app: SpawnedApp | undefined;
  const upstreams = new Map<string, A2aUpstream>();
  let etcdReachable = false;

  const agentName = (name: string, v: Wire) => `stream-${name}-${v === "0.3" ? "v03" : "v10"}`;
  const posts = (v: Wire, name: string) =>
    upstreams.get(agentName(name, v))!.requests.filter((r) => r.httpMethod === "POST");

  const call = (v: Wire, name: string, method: string, params: unknown, id = randomUUID()) =>
    fetch(`${app!.proxyUrl}/a2a/${agentName(name, v)}`, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
    });

  const AGENTS: Record<string, { streamChunks: string[]; holdStreamOpen?: boolean }> = {
    clean: { streamChunks: CLEAN_CHUNKS },
    risky: { streamChunks: RISKY_CHUNKS },
    split: { streamChunks: SPLIT_CHUNKS },
    "open-clean": { streamChunks: CLEAN_CHUNKS, holdStreamOpen: true },
    "open-risky": { streamChunks: RISKY_CHUNKS, holdStreamOpen: true },
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    for (const v of WIRES) {
      for (const [name, opts] of Object.entries(AGENTS)) {
        upstreams.set(agentName(name, v), await startA2aUpstream({ wireShape: v, ...opts }));
      }
    }
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [key, up] of upstreams) {
      await seed.update("a2a_agents", randomUUID(), {
        name: key,
        url: up.url,
        protocol_version: key.endsWith("v03") ? "0.3" : "1.0",
        auth_type: "none",
        enabled: true,
      });
    }
    await seed.createGuardrail({
      name: "a2a-stream-keyword",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: MARKER }],
    });
    // key 最后写：它能鉴权时，按修订号顺序前面的 agent 与护栏都已生效。
    await seed.createApiKey({ key_hash: sha256(KEY), allowed_models: [], allowed_agents: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${KEY}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    for (const up of upstreams.values()) await up.close();
  });

  for (const v of WIRES) {
    describe(`wire ${v}`, () => {
      for (const [label, method] of [
        ["a streamed answer", STREAM[v]],
        ["a resubscribed task stream", RESUBSCRIBE[v]],
      ] as const) {
        const params = () =>
          method === STREAM[v] ? { message: message(v, "write the report") } : { id: "task-e2e-stream" };

        test(`${label} carrying a forbidden word never delivers it`, async (ctx) => {
          if (!etcdReachable || !app) return ctx.skip();
          const res = await call(v, "risky", method, params());
          const { events, wire } = await readSse(res);
          expect(wire, "the forbidden word reached the caller").not.toContain(MARKER);
          // 被拦的那段回答整段不出现，被拦之后也不再有「任务完成」送到调用方。
          expect(wire).not.toContain(RISKY_CHUNKS[1].trim());
          expect(wire).not.toContain(CLOSING);
          // 调用方知道回答被拦了，而不是看到一个无声结束的流。
          expect(res.status !== 200 || events.some(isError), wire.slice(0, 400)).toBe(true);
        });

        test(`${label} with a forbidden word split across two events delivers neither half`, async (ctx) => {
          if (!etcdReachable || !app) return ctx.skip();
          const res = await call(v, "split", method, params());
          const { events, wire } = await readSse(res);
          expect(wire).not.toContain(MARKER);
          expect(wire).not.toContain(SPLIT_CHUNKS[0]);
          expect(wire).not.toContain(SPLIT_CHUNKS[1]);
          expect(res.status !== 200 || events.some(isError), wire.slice(0, 400)).toBe(true);
        });

        test(`${label} that is clean arrives in full`, async (ctx) => {
          if (!etcdReachable || !app) return ctx.skip();
          const res = await call(v, "clean", method, params());
          expect(res.status).toBe(200);
          const { events, wire } = await readSse(res);
          expect(events.some(isError), wire.slice(0, 400)).toBe(false);
          expect(artifactText(events)).toBe(CLEAN_CHUNKS.join(""));
          expect(wire).toContain(CLOSING);
        });
      }

      test("a clean stream the agent leaves open still delivers its final event", async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        // agent 发完最后一个事件后不关连接；调用方读到最后那个事件就该停。
        // 要是网关等连接关闭才放行扣着的内容，调用方就一直等下去。
        const res = await call(v, "open-clean", STREAM[v], { message: message(v, "write the report") });
        expect(res.status).toBe(200);
        const { events, wire, timedOut } = await readSse(
          res,
          (e) => JSON.stringify(e.data).includes(CLOSING),
          8_000,
        );
        expect(timedOut, `the final event never arrived; got: ${wire.slice(0, 400)}`).toBe(false);
        expect(artifactText(events)).toBe(CLEAN_CHUNKS.join(""));
      });

      test("a flagged stream the agent leaves open is refused without waiting for it to close", async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const res = await call(v, "open-risky", STREAM[v], { message: message(v, "write the report") });
        const { events, wire, timedOut } = await readSse(res, isError, 8_000);
        expect(wire).not.toContain(MARKER);
        expect(timedOut, `no refusal arrived; got: ${wire.slice(0, 400)}`).toBe(false);
        expect(res.status !== 200 || events.some(isError)).toBe(true);
      });

      for (const [label, method] of [
        ["message/send", SEND[v]],
        ["message/stream", STREAM[v]],
      ] as const) {
        test(`a ${label} asking for push notifications is refused before the agent sees it`, async (ctx) => {
          if (!etcdReachable || !app) return ctx.skip();
          const before = posts(v, "clean").length;
          const id = randomUUID();
          const res = await call(
            v,
            "clean",
            method,
            {
              message: message(v, "write the report"),
              configuration: { [PUSH_FIELD[v]]: { url: WEBHOOK, token: "caller-token" } },
            },
            id,
          );
          const text = await res.text();
          expect(res.status, text.slice(0, 300)).not.toBe(200);
          const body = JSON.parse(text) as { id?: unknown; error?: { code?: number }; result?: unknown };
          expect(body.id).toBe(id);
          expect(body.result).toBeUndefined();
          expect(body.error?.code).toBe(PUSH_NOT_SUPPORTED);
          expect(posts(v, "clean").length).toBe(before);
        });
      }

      test("setting a push notification config on a task is refused before the agent sees it", async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const before = posts(v, "clean").length;
        const params =
          v === "0.3"
            ? { taskId: "task-e2e-stream", pushNotificationConfig: { url: WEBHOOK } }
            : { taskId: "task-e2e-stream", config: { url: WEBHOOK } };
        const res = await call(v, "clean", SET_PUSH[v], params);
        const text = await res.text();
        expect(res.status, text.slice(0, 300)).not.toBe(200);
        expect((JSON.parse(text) as { error?: { code?: number } }).error?.code).toBe(PUSH_NOT_SUPPORTED);
        expect(posts(v, "clean").length).toBe(before);
      });

      test("push setup sent as a batch or with differently cased keys is refused too", async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        // 有的 agent 按 JSON-RPC 批量请求处理数组，有的解码器不分键的大小写：
        // 这两种写法到了 agent 那里都是在设推送。
        const before = posts(v, "clean").length;
        for (const body of [
          [{ jsonrpc: "2.0", id: 1, method: SET_PUSH[v], params: { taskId: "task-e2e-stream" } }],
          {
            jsonrpc: "2.0",
            id: 2,
            Method: SET_PUSH[v],
            params: { taskId: "task-e2e-stream" },
          },
          {
            jsonrpc: "2.0",
            id: 3,
            method: SEND[v],
            Params: {
              message: message(v, "write the report"),
              Configuration: { [PUSH_FIELD[v]]: { url: WEBHOOK } },
            },
          },
        ]) {
          const res = await fetch(`${app.proxyUrl}/a2a/${agentName("clean", v)}`, {
            method: "POST",
            headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
            body: JSON.stringify(body),
          });
          const text = await res.text();
          expect(res.status, `${JSON.stringify(body).slice(0, 80)}: ${text.slice(0, 200)}`).not.toBe(200);
          expect((JSON.parse(text) as { error?: { code?: number } }).error?.code).toBe(PUSH_NOT_SUPPORTED);
        }
        expect(posts(v, "clean").length).toBe(before);
      });

      test("reading a push notification config is still forwarded", async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        // 拒的是「设推送」，不是所有和推送相关的方法：读回配置不会把回答送出去。
        const before = posts(v, "clean").length;
        const res = await call(v, "clean", GET_PUSH[v], { id: "task-e2e-stream" });
        await res.text();
        expect(res.status).toBe(200);
        expect(posts(v, "clean").length).toBe(before + 1);
      });
    });
  }
});

describe("A2A push notification settings pass through when no output guardrail applies", () => {
  let app: SpawnedApp | undefined;
  let agent: A2aUpstream | undefined;
  let etcdReachable = false;
  const KEY_IN = "sk-a2a-guardrail-stream-input-only";

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    agent = await startA2aUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.update("a2a_agents", randomUUID(), {
      name: "push-ok",
      url: agent.url,
      protocol_version: "0.3",
      auth_type: "none",
      enabled: true,
    });
    // 只审请求的护栏：回答不经审查，推送通知没有可绕过的东西。
    await seed.createGuardrail({
      name: "a2a-stream-input-only",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: MARKER }],
    });
    await seed.createApiKey({ key_hash: sha256(KEY_IN), allowed_models: [], allowed_agents: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${KEY_IN}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await agent?.close();
  });

  test("a message/send asking for push notifications reaches the agent with its config intact", async (ctx) => {
    if (!etcdReachable || !app || !agent) return ctx.skip();
    const configuration = { pushNotificationConfig: { url: WEBHOOK, token: "caller-token" } };
    const res = await fetch(`${app.proxyUrl}/a2a/push-ok`, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY_IN}`, "content-type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        method: "message/send",
        params: { message: message("0.3", "write the report"), configuration },
      }),
    });
    expect(res.status, await res.text()).toBe(200);
    const sent = agent.requests.filter((r) => r.httpMethod === "POST").at(-1)!.body as {
      params: { configuration: unknown };
    };
    expect(sent.params.configuration).toEqual(configuration);
  });
});

/** 阿里云文本审核的替身：内容含被拦词时判高风险。 */
async function startModeration(): Promise<{ baseUrl: string; close(): Promise<void> }> {
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      let content = "";
      try {
        const sp = JSON.parse(new URLSearchParams(raw).get("ServiceParameters") ?? "{}") as {
          content?: unknown;
        };
        content = typeof sp.content === "string" ? sp.content : "";
      } catch {
        content = "";
      }
      const risky = content.includes(MARKER);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          Code: 200,
          Message: "OK",
          RequestId: "A2A-STREAM-REQ",
          Data: {
            RiskLevel: risky ? "high" : "none",
            Result: risky ? [{ Label: "inappropriate_oral", Confidence: 100.0 }] : [{ Label: "nonLabel" }],
          },
        }),
      );
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((r) => server.listen(port, "127.0.0.1", r));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

describe("A2A streams under a window-moderated output guardrail", () => {
  let app: SpawnedApp | undefined;
  let moderation: Awaited<ReturnType<typeof startModeration>> | undefined;
  const upstreams = new Map<string, A2aUpstream>();
  let etcdReachable = false;
  const KEY_WIN = "sk-a2a-guardrail-stream-window";
  const agentName = (name: string, v: Wire) => `win-${name}-${v === "0.3" ? "v03" : "v10"}`;

  const stream = (v: Wire, name: string) =>
    fetch(`${app!.proxyUrl}/a2a/${agentName(name, v)}`, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY_WIN}`, "content-type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: randomUUID(),
        method: STREAM[v],
        params: { message: message(v, "write the report") },
      }),
    });

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    moderation = await startModeration();
    for (const v of WIRES) {
      upstreams.set(agentName("clean", v), await startA2aUpstream({ wireShape: v, streamChunks: CLEAN_CHUNKS }));
      upstreams.set(agentName("risky", v), await startA2aUpstream({ wireShape: v, streamChunks: RISKY_CHUNKS }));
    }
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [key, up] of upstreams) {
      await seed.update("a2a_agents", randomUUID(), {
        name: key,
        url: up.url,
        protocol_version: key.endsWith("v03") ? "0.3" : "1.0",
        auth_type: "none",
        enabled: true,
      });
    }
    await seed.createGuardrail({
      name: "a2a-stream-aliyun-window",
      enabled: true,
      hook_point: "output",
      fail_open: false,
      kind: "aliyun_text_moderation",
      region: "cn-shanghai",
      endpoint: moderation.baseUrl,
      access_key_id: "LTAI_E2E",
      access_key_secret: "e2e-secret",
      risk_level_threshold: "high",
      stream_processing_mode: "window",
      window_size: 16,
      window_overlap_size: 4,
    });
    await seed.createApiKey({ key_hash: sha256(KEY_WIN), allowed_models: [], allowed_agents: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${KEY_WIN}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    for (const up of upstreams.values()) await up.close();
    await moderation?.close();
  });

  for (const v of WIRES) {
    test(`wire ${v}: the flagged event never reaches the caller`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const res = await stream(v, "risky");
      const { events, wire } = await readSse(res);
      expect(wire).not.toContain(MARKER);
      expect(wire).not.toContain(CLOSING);
      expect(res.status !== 200 || events.some(isError), wire.slice(0, 400)).toBe(true);
    });

    test(`wire ${v}: a clean answer still streams — early events arrive before the task ends`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const res = await stream(v, "clean");
      expect(res.status).toBe(200);
      const { events, wire } = await readSse(res);
      expect(events.some(isError), wire.slice(0, 400)).toBe(false);
      expect(artifactText(events)).toBe(CLEAN_CHUNKS.join(""));
      // 窗口模式是「审过一段放一段」：agent 隔了几百毫秒才收尾，最早的事件
      // 应当早于最后一个事件好一阵到达，而不是攒到最后一起放。
      expect(events.length).toBeGreaterThan(2);
      expect(events.at(-1)!.at - events[0].at).toBeGreaterThan(150);
    });
  }
});
