import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startA2aUpstream,
  waitConfigPropagation,
  type A2aUpstream,
  type SpawnedApp,
} from "../harness/index.js";
import { UNARY_TASK_ID } from "../harness/upstream-a2a.js";

// E2E：A2A 的内容护栏要读到 agent 收发的每一处内容，而不只是文本 part。
//
// 一个 Task 的内容不止状态消息里的那句话：`history[]` 里是往来的每条消息，
// artifact 与消息里可以是结构化的 data part（`{"kind":"data","data":{…}}`，
// 1.0 写作 `{"data":{…}}`）。这些都原样到达调用方 / agent，护栏只读文本
// part，就等于在这些位置上不设防：
//
// - 回答里只在 history 或 data part 中出现的被拦词，直接回到调用方；
// - `tasks/get` 读回同一个 Task 时根本不检查，`message/send` 拒掉的回答换个
//   方法就能读出来；
// - 请求里 data part 的字符串（以及对象键）不经检查就送到 agent；mask 规则
//   改写不了 data part，就只能原样放行。
//
// 两个 wire 版本各跑一遍：0.3 的对象带 `kind` 标签，1.0 是 protobuf JSON（
// `SendMessage` 的结果包在 `task` 里，`GetTask` 直接返回 Task）。
// 每个拒绝都有对照：同样形状、不含被拦内容的调用照常 200，内容完整送达，
// 说明拦下的是那段内容，而不是这种形状本身。

const KEY = "sk-a2a-guardrail-task-content";
const MARKER = "a2ataskcontentforbidden";
/** 一个按数字写的被拦值：数字型的 JSON 值也必须被读到。 */
const NUMBER_MARKER = 7777777777777;
const EMAIL = "carol.task-content@example.com";

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

type Wire = "0.3" | "1.0";
const WIRES: Wire[] = ["0.3", "1.0"];

const SEND: Record<Wire, string> = { "0.3": "message/send", "1.0": "SendMessage" };
const GET: Record<Wire, string> = { "0.3": "tasks/get", "1.0": "GetTask" };
const CANCEL: Record<Wire, string> = { "0.3": "tasks/cancel", "1.0": "CancelTask" };
const STREAM: Record<Wire, string> = { "0.3": "message/stream", "1.0": "SendStreamingMessage" };

// 各版本自己的写法：0.3 的 part / message 带 `kind`，1.0 的 part 是 oneof
// （设了哪个字段就是哪种 part），role 是枚举名。
const textPart = (v: Wire, text: string) => (v === "0.3" ? { kind: "text", text } : { text });
const dataPart = (v: Wire, data: unknown) => (v === "0.3" ? { kind: "data", data } : { data });
/** 一个纯文本文件部件：内容按协议以 base64 承载，agent 与客户端都会解出来读。 */
const textFilePart = (v: Wire, text: string) => filePart(v, Buffer.from(text, "utf8"), "text/plain");
/** 一个文件部件，内容按协议以 base64 承载；`wrap` 时按 MIME 习惯每 8 个字符断一行。 */
const filePart = (v: Wire, bytes: Buffer, mediaType?: string, wrap = false) => {
  let b64 = bytes.toString("base64");
  if (wrap) b64 = b64.replace(/(.{8})/g, "$1\n");
  const media = mediaType ? (v === "0.3" ? { mimeType: mediaType } : { mediaType }) : {};
  return v === "0.3"
    ? { kind: "file", file: { name: "note.txt", ...media, bytes: b64 } }
    : { raw: b64, ...media, filename: "note.txt" };
};
const message = (v: Wire, role: "user" | "agent", parts: unknown[]) =>
  v === "0.3"
    ? { kind: "message", messageId: randomUUID(), role, parts }
    : { messageId: randomUUID(), role: role === "user" ? "ROLE_USER" : "ROLE_AGENT", parts };
const completed = (v: Wire, parts: unknown[]) => ({
  state: v === "0.3" ? "completed" : "TASK_STATE_COMPLETED",
  message: message(v, "agent", parts),
});

/** 每种 agent 的回答（Task 的其余字段），按版本写出。 */
const AGENTS: Record<string, (v: Wire) => Record<string, unknown>> = {
  // 什么都不多带：给请求方向的用例用。
  plain: (v) => ({ status: completed(v, [textPart(v, "Done.")]) }),
  // 被拦词就在状态消息里 —— `message/send` 本来就会拦，用来单独验 `tasks/get`。
  "status-risky": (v) => ({ status: completed(v, [textPart(v, `case 7: ${MARKER}`)]) }),
  "history-risky": (v) => ({
    status: completed(v, [textPart(v, "Done.")]),
    history: [
      message(v, "user", [textPart(v, "look up case 7")]),
      message(v, "agent", [textPart(v, `case 7: ${MARKER}`)]),
    ],
  }),
  "history-clean": (v) => ({
    status: completed(v, [textPart(v, "Done.")]),
    history: [
      message(v, "user", [textPart(v, "look up case 7")]),
      message(v, "agent", [textPart(v, "case 7: all clear")]),
    ],
  }),
  // 被拦词藏在 artifact 里 data part 的嵌套字符串值中。
  "data-risky": (v) => ({
    status: completed(v, [textPart(v, "Done.")]),
    artifacts: [
      {
        artifactId: "verdict",
        parts: [dataPart(v, { case: 7, findings: [{ note: `flagged ${MARKER}` }] })],
      },
    ],
  }),
  // 被拦词是状态消息里 data part 的一个对象键。
  "data-key-risky": (v) => ({
    status: completed(v, [textPart(v, "Done."), dataPart(v, { [MARKER]: true })]),
  }),
  // 被拦词在 Task 的 metadata 与 artifact 的名字里：客户端同样拿得到。
  "metadata-risky": (v) => ({
    status: completed(v, [textPart(v, "Done.")]),
    artifacts: [{ artifactId: "verdict", name: `report on ${MARKER}`, parts: [textPart(v, "ok")] }],
    metadata: { summary: `flagged ${MARKER}` },
  }),
  // 被拦词在 artifact 里一个纯文本文件的内容中（base64）。
  "file-risky": (v) => ({
    status: completed(v, [textPart(v, "Done.")]),
    artifacts: [{ artifactId: "verdict", parts: [textFilePart(v, `case 7: ${MARKER}`)] }],
  }),
  "data-clean": (v) => ({
    status: completed(v, [textPart(v, "Done.")]),
    artifacts: [
      {
        artifactId: "verdict",
        parts: [dataPart(v, { case: 7, findings: [{ note: "all clear" }] })],
      },
    ],
  }),
};

const agentName = (name: string, v: Wire) => `${name}-${v === "0.3" ? "v03" : "v10"}`;

describe("A2A content guardrails read task history, data parts and tasks/get", () => {
  let app: SpawnedApp | undefined;
  const upstreams = new Map<string, A2aUpstream>();
  let etcdReachable = false;

  const rpc = async (agent: string, method: string, params: unknown) => {
    const id = randomUUID();
    const res = await fetch(`${app!.proxyUrl}/a2a/${agent}`, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params }),
    });
    return { id, status: res.status, text: await res.text() };
  };
  const send = (v: Wire, agent: string, parts: unknown[]) =>
    rpc(agentName(agent, v), SEND[v], { message: message(v, "user", parts) });
  const getTask = (v: Wire, agent: string) =>
    rpc(agentName(agent, v), GET[v], { id: UNARY_TASK_ID });

  /** 这个 agent 至今收到的 JSON-RPC 调用数。 */
  const posts = (v: Wire, agent: string) =>
    upstreams.get(agentName(agent, v))!.requests.filter((r) => r.httpMethod === "POST");

  /** 拒绝是一个 JSON-RPC 错误：回显请求 id，没有 result，也不带被拦内容。 */
  const expectRefused = (res: { id: string; status: number; text: string }, banned: string) => {
    expect(res.status, res.text.slice(0, 300)).toBe(422);
    expect(res.text).not.toContain(banned);
    const body = JSON.parse(res.text) as Record<string, unknown>;
    expect(body.id).toBe(res.id);
    expect(body.error).toBeDefined();
    expect(body.result).toBeUndefined();
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    for (const v of WIRES) {
      for (const [name, task] of Object.entries(AGENTS)) {
        upstreams.set(agentName(name, v), await startA2aUpstream({ wireShape: v, unaryTask: task(v) }));
      }
      // 一个用 JSON-RPC 错误回答的 agent：错误里的内容同样交到调用方手里。
      upstreams.set(
        agentName("error-risky", v),
        await startA2aUpstream({
          wireShape: v,
          unaryError: { code: -32000, message: `failed on ${MARKER}`, data: { detail: "x" } },
        }),
      );
    }
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const v of WIRES) {
      for (const name of [...Object.keys(AGENTS), "error-risky"]) {
        await seed.update("a2a_agents", randomUUID(), {
          name: agentName(name, v),
          url: upstreams.get(agentName(name, v))!.url,
          protocol_version: v,
          auth_type: "none",
          enabled: true,
        });
      }
    }
    await seed.createGuardrail({
      name: "a2a-task-content-keyword",
      enabled: true,
      hook_point: "both",
      kind: "keyword",
      patterns: [
        { kind: "literal", value: MARKER },
        { kind: "literal", value: String(NUMBER_MARKER) },
      ],
    });
    await seed.createGuardrail({
      name: "a2a-task-content-mask-email",
      enabled: true,
      hook_point: "input",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
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

  describe.each(WIRES)("A2A %s", (v) => {
    // ── agent 的回答 ──────────────────────────────────────────────────────

    test("a forbidden phrase only in the task history never reaches the caller", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectRefused(await send(v, "history-risky", [textPart(v, "look up case 7")]), MARKER);
    });

    test("a clean task history is returned intact", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const res = await send(v, "history-clean", [textPart(v, "look up case 7")]);
      expect(res.status, res.text.slice(0, 300)).toBe(200);
      expect(res.text).toContain("case 7: all clear");
    });

    test("a forbidden phrase inside a data part's value never reaches the caller", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectRefused(await send(v, "data-risky", [textPart(v, "look up case 7")]), MARKER);
    });

    test("a forbidden phrase as a data part's key never reaches the caller", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectRefused(await send(v, "data-key-risky", [textPart(v, "look up case 7")]), MARKER);
    });

    test("a clean data part is returned intact", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const res = await send(v, "data-clean", [textPart(v, "look up case 7")]);
      expect(res.status, res.text.slice(0, 300)).toBe(200);
      expect(res.text).toContain("all clear");
    });

    // ── 用 tasks/get 读回同一个 Task ───────────────────────────────────────

    test.for(["status-risky", "history-risky", "data-risky", "data-key-risky"])(
      "reading the %s task back with tasks/get is refused too",
      async (agent, ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        // 先按正常流程发一次：这个回答本身就被拒了。
        expectRefused(await send(v, agent, [textPart(v, "look up case 7")]), MARKER);
        // 换成读操作拿同一个 Task，也不能把它带出来。
        expectRefused(await getTask(v, agent), MARKER);
      },
    );

    test.for(["plain", "history-clean", "data-clean"])(
      "reading the clean %s task back with tasks/get returns it",
      async (agent, ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const res = await getTask(v, agent);
        expect(res.status, res.text.slice(0, 300)).toBe(200);
        expect(res.text).toContain(UNARY_TASK_ID);
        if (agent !== "plain") expect(res.text).toContain("all clear");
      },
    );

    test("cancelling the task does not hand its content back either", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      // `tasks/cancel` 也返回整个 Task。
      expectRefused(await rpc(agentName("history-risky", v), CANCEL[v], { id: UNARY_TASK_ID }), MARKER);
      const clean = await rpc(agentName("history-clean", v), CANCEL[v], { id: UNARY_TASK_ID });
      expect(clean.status, clean.text.slice(0, 300)).toBe(200);
      expect(clean.text).toContain("case 7: all clear");
    });

    test("a forbidden phrase in the task's metadata or an artifact's name never reaches the caller", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectRefused(await send(v, "metadata-risky", [textPart(v, "look up case 7")]), MARKER);
    });

    test("a forbidden phrase in the agent's error envelope never reaches the caller", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectRefused(await send(v, "error-risky", [textPart(v, "look up case 7")]), MARKER);
    });

    test("a forbidden phrase inside a text file the answer carries never reaches the caller", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectRefused(await send(v, "file-risky", [textPart(v, "look up case 7")]), MARKER);
    });

    // ── 调用方的请求 ──────────────────────────────────────────────────────

    test("a text file wrapped the MIME way is still read", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const file = filePart(v, Buffer.from(`please ${MARKER} now`, "utf8"), "text/plain", true);
      expectRefused(await send(v, "plain", [textPart(v, "read the note"), file]), MARKER);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("content the scan cannot read is refused rather than forwarded unread", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const blob = Buffer.from([0x00, 0x9c, 0xff, 0x13, 0x02, 0x07, 0x81, 0x00, 0xfe, 0x01]);
      const res = await send(v, "plain", [textPart(v, "here is a file"), filePart(v, blob, "application/octet-stream")]);
      expect(res.status, res.text.slice(0, 300)).toBe(422);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a file of a binary media type passes unread", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const png = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00]);
      const res = await send(v, "plain", [textPart(v, "here is a picture"), filePart(v, png, "image/png")]);
      expect(res.status, res.text.slice(0, 300)).toBe(200);
    });

    test("text labelled as an image is read like any text", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const disguised = filePart(v, Buffer.from(`please ${MARKER} now`, "utf8"), "image/png");
      expectRefused(await send(v, "plain", [textPart(v, "here is a picture"), disguised]), MARKER);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("bytes labelled as an image without an image's signature are refused", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const noise = Buffer.from([0x13, 0x9c, 0xff, 0x00, 0x02, 0x07, 0x81, 0x00, 0xfe, 0x01]);
      const res = await send(v, "plain", [textPart(v, "here is a picture"), filePart(v, noise, "image/png")]);
      expect(res.status, res.text.slice(0, 300)).toBe(422);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a forbidden value written as a JSON number is read", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const res = await send(v, "plain", [textPart(v, "file this form"), dataPart(v, { ref: NUMBER_MARKER })]);
      expectRefused(res, String(NUMBER_MARKER));
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a forbidden phrase inside a text file the request carries is refused before the agent sees it", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const res = await send(v, "plain", [textPart(v, "read the note"), textFilePart(v, `please ${MARKER}`)]);
      expectRefused(res, MARKER);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a forbidden phrase in the request message's metadata is refused before the agent sees it", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const res = await rpc(agentName("plain", v), SEND[v], {
        message: { ...message(v, "user", [textPart(v, "file this form")]), metadata: { note: `please ${MARKER}` } },
      });
      expectRefused(res, MARKER);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a forbidden phrase inside a request data part's value is refused before the agent sees it", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const res = await send(v, "plain", [
        textPart(v, "file this form"),
        dataPart(v, { form: { reason: `please ${MARKER}` } }),
      ]);
      expectRefused(res, MARKER);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a forbidden phrase as a request data part's key is refused before the agent sees it", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const res = await send(v, "plain", [textPart(v, "file this form"), dataPart(v, { [MARKER]: 1 })]);
      expectRefused(res, MARKER);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("a clean request data part reaches the agent unchanged", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const parts = [textPart(v, "file this form"), dataPart(v, { form: { reason: "all clear", count: 3 } })];
      const res = await send(v, "plain", parts);
      expect(res.status, res.text.slice(0, 300)).toBe(200);
      const sent = posts(v, "plain").at(-1)!.body as { params: { message: { parts: unknown[] } } };
      expect(sent.params.message.parts).toEqual(parts);
    });

    test("a streamed call's request data part is read the same way", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      // 流式调用的请求在派发前照样要检查。
      const before = posts(v, "plain").length;
      const streamParams = (data: unknown) => ({
        message: message(v, "user", [textPart(v, "file this form"), dataPart(v, data)]),
      });
      const risky = await rpc(agentName("plain", v), STREAM[v], streamParams({ reason: `please ${MARKER}` }));
      expectRefused(risky, MARKER);
      expect(posts(v, "plain").length).toBe(before);

      const clean = await rpc(agentName("plain", v), STREAM[v], streamParams({ reason: "all clear" }));
      expect(clean.status, clean.text.slice(0, 300)).toBe(200);
      expect(posts(v, "plain").length).toBe(before + 1);
    });

    test("a request data part a mask rule would rewrite is refused, not forwarded unmasked", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const before = posts(v, "plain").length;
      const res = await send(v, "plain", [textPart(v, "file this form"), dataPart(v, { contact: EMAIL })]);
      expectRefused(res, EMAIL);
      expect(posts(v, "plain").length).toBe(before);
    });

    test("the same address in a text part is masked and forwarded", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      // 对照：mask 规则确实在生效，文本 part 照常改写后送达 —— 上一条被拒，
      // 是因为 data part 改写不了，而不是这个地址一律拒绝。
      const res = await send(v, "plain", [textPart(v, `file this form for ${EMAIL}`)]);
      expect(res.status, res.text.slice(0, 300)).toBe(200);
      const sent = JSON.stringify(posts(v, "plain").at(-1)!.body);
      expect(sent).toContain("file this form for");
      expect(sent).not.toContain(EMAIL);
    });
  });
});
