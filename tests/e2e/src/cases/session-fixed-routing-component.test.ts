import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  pickFreePort,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// 组件协议测试：真实本地网关和受控 HTTP peers，不代表外部模型推理验收。
// 预期来自合同：同一固定账号池内按 session 分配，运行时不可用不能换账号。
const SESSION_SOURCES = [
  { type: "header", name: "x-claude-code-session-id" },
  { type: "header", name: "session-id" },
  { type: "header", name: "thread-id" },
];
const SURFACES = [
  ["/v1/chat/completions", false],
  ["/v1/chat/completions", true],
  ["/v1/messages", false],
  ["/v1/messages", true],
  ["/v1/messages/count_tokens", false],
  ["/v1/responses", false],
  ["/v1/responses", true],
] as const;

interface Peer {
  name: string;
  baseUrl: string;
  paths: string[];
  failing: boolean;
  close(): Promise<void>;
}

function chatAnswer(name: string, id: string) {
  return {
    id, object: "chat.completion", created: 1, model: "component-model",
    choices: [{ index: 0, message: { role: "assistant", content: name }, finish_reason: "stop" }],
    usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
  };
}

async function startPeer(name: string): Promise<Peer> {
  const peer: Peer = { name, baseUrl: "", paths: [], failing: false, close: async () => {} };
  let sequence = 0;
  const server = createServer((request, response) => {
    let raw = "";
    request.on("data", (part: Buffer) => { raw += part.toString("utf8"); });
    request.on("end", () => {
      const path = request.url ?? "/";
      const body = JSON.parse(raw) as { stream?: boolean };
      peer.paths.push(path);
      if (peer.failing) {
        response.writeHead(503, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { type: "server_error", message: "temporarily unavailable" } }));
        return;
      }
      const id = `${name}-${++sequence}`;
      let answer: unknown;
      let frames: string[];
      if (path.endsWith("/messages/count_tokens")) {
        answer = { input_tokens: 3 };
        frames = [];
      } else if (path.endsWith("/messages")) {
        answer = { id: `msg_${id}`, type: "message", role: "assistant", model: "component-model",
          content: [{ type: "text", text: name }], stop_reason: "end_turn",
          usage: { input_tokens: 3, output_tokens: 1 } };
        frames = [
          `event: message_start\ndata: ${JSON.stringify({ type: "message_start", message: { ...answer as object, content: [], stop_reason: null } })}\n\n`,
          `event: content_block_start\ndata: ${JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } })}\n\n`,
          `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: name } })}\n\n`,
          'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
          'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}\n\n',
          'event: message_stop\ndata: {"type":"message_stop"}\n\n',
        ];
      } else if (path.endsWith("/responses")) {
        answer = { id: `resp_${id}`, object: "response", created_at: 1, status: "completed",
          model: "component-model", output: [{ id: `msg_${id}`, type: "message", role: "assistant", status: "completed",
            content: [{ type: "output_text", text: name, annotations: [] }] }],
          usage: { input_tokens: 3, output_tokens: 1, total_tokens: 4 } };
        frames = [
          `event: response.created\ndata: ${JSON.stringify({ type: "response.created", sequence_number: 0, response: { ...answer as object, status: "in_progress", output: [] } })}\n\n`,
          `event: response.completed\ndata: ${JSON.stringify({ type: "response.completed", sequence_number: 1, response: answer })}\n\n`,
        ];
      } else {
        answer = chatAnswer(name, `chatcmpl-${id}`);
        frames = [
          `data: ${JSON.stringify({ id: `chatcmpl-${id}`, object: "chat.completion.chunk", created: 1,
            model: "component-model", choices: [{ index: 0, delta: { role: "assistant", content: name }, finish_reason: null }] })}\n\n`,
          `data: ${JSON.stringify({ id: `chatcmpl-${id}`, object: "chat.completion.chunk", created: 1,
            model: "component-model", choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
            usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 } })}\n\n`,
          "data: [DONE]\n\n",
        ];
      }
      response.writeHead(200, { "content-type": body.stream ? "text/event-stream" : "application/json" });
      response.end(body.stream ? frames.join("") : JSON.stringify(answer));
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  peer.baseUrl = `http://127.0.0.1:${port}`;
  peer.close = () => new Promise<void>((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
    server.closeAllConnections();
  });
  return peer;
}

interface Fixture {
  model: string;
  caller: string;
  modelIds: [string, string];
  accountIds: [string, string];
}

describe("fixed session hashing component protocol", () => {
  let app: SpawnedApp;
  let seed: SeedClient;
  let peers: [Peer, Peer];
  let fixtureSequence = 0;
  let barrierSequence = 0;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    await etcd.ping();
    peers = await Promise.all([startPeer("component-account-a"), startPeer("component-account-b")]);
    app = await spawnApp({ threadPerCore: false });
    seed = new SeedClient(etcd, app.etcdPrefix);
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(peers?.map((peer) => peer.close()) ?? []);
  });

  const counts = () => peers.map((peer) => peer.paths.length);

  async function ready(caller: string): Promise<void> {
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${caller}` }, signal: AbortSignal.timeout(8_000),
      });
      await response.arrayBuffer();
      if (response.status === 401) return false;
      if (response.status !== 200) throw new Error(`configuration barrier returned ${response.status}`);
      return true;
    });
  }

  async function barrier(model: string): Promise<void> {
    // 新调用key最后写入；它能认证就证明此前目标变更已被同一watch快照应用。
    const caller = `sk-fixed-session-barrier-${++barrierSequence}`;
    await seed.createApiKey({ key_hash: createHash("sha256").update(caller).digest("hex"), allowed_models: [model] });
    await ready(caller);
  }

  async function fixture(fixed = true): Promise<Fixture> {
    const model = `fixed-session-${++fixtureSequence}`;
    const direct = await Promise.all(peers.map(async (peer, index) => {
      const account = await seed.createProviderKey({ display_name: `${model}-account-${index}`,
        api_key: `component-credential-${index}`, api_base: `${peer.baseUrl}/v1`,
        apis: { messages: { base: peer.baseUrl }, responses: { base: `${peer.baseUrl}/v1` } } });
      const target = await seed.createModel({ display_name: `${model}-target-${index}`,
        provider: "openai", model_name: "component-model", provider_key_id: account.id,
        retries: 0, cooldown: { enabled: true, default_seconds: 60 } });
      return { accountId: account.id, modelId: target.id };
    }));
    await seed.createModel({ display_name: model, routing: {
      strategy: "consistent_hash", ...(fixed ? { hash_failover: false } : {}), hash_on: SESSION_SOURCES,
      retries: 0, targets: direct.map((target) => ({ model_id: target.modelId, weight: 1, priority: 0 })),
    } });
    const caller = `sk-fixed-session-caller-${fixtureSequence}`;
    await seed.createApiKey({ key_hash: createHash("sha256").update(caller).digest("hex"), allowed_models: [model] });
    await ready(caller);
    return { model, caller, modelIds: [direct[0].modelId, direct[1].modelId],
      accountIds: [direct[0].accountId, direct[1].accountId] };
  }

  async function send(
    fixture: Fixture, path: string, stream: boolean, sessionHeaders: Record<string, string>,
  ): Promise<{ status: number; text: string }> {
    const body = path === "/v1/responses"
      ? { model: fixture.model, input: "component request", stream, store: false }
      : { model: fixture.model, messages: [{ role: "user", content: "component request" }],
        ...(path === "/v1/messages/count_tokens" ? {} : { stream, max_tokens: 8 }) };
    const response = await fetch(`${app.proxyUrl}${path}`, { method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${fixture.caller}`,
        "anthropic-version": "2023-06-01", ...sessionHeaders },
      body: JSON.stringify(body), signal: AbortSignal.timeout(8_000) });
    return { status: response.status, text: await response.text() };
  }

  async function accountFor(fixture: Fixture, session: string): Promise<number> {
    const before = counts();
    const response = await send(fixture, "/v1/chat/completions", false, { "session-id": session });
    expect(response.status, response.text).toBe(200);
    const delta = counts().map((value, index) => value - before[index]);
    expect(delta.reduce((sum, value) => sum + value, 0)).toBe(1);
    return delta.indexOf(1);
  }

  test("one session keeps its account across HTTP families and streaming", async () => {
    // 若某个handler没有线程会话头，或流式分支重新选账号，peer计数就会不一致。
    const config = await fixture();
    const session = "cross-family-session";
    const expected = await accountFor(config, session);
    for (const [path, stream] of SURFACES) {
      const before = counts();
      const response = await send(config, path, stream, { "x-claude-code-session-id": session });
      expect(response.status, `${path} ${stream}: ${response.text}`).toBe(200);
      expect(counts().map((value, index) => value - before[index])).toEqual(expected === 0 ? [1, 0] : [0, 1]);
    }
  });

  test("missing session headers reject before either account is called", async () => {
    // 删除会话必填判断、回退APIkey或hash空串都会使status/零请求断言变红。
    const config = await fixture();
    const before = counts();
    for (const [path, stream] of SURFACES) {
      const response = await send(config, path, stream, {});
      expect(response.status, `${path} ${stream}: ${response.text}`).toBe(400);
    }
    expect(counts()).toEqual(before);
  });

  test("concurrent turns stick, distinct sessions spread, and restart recomputes the same account", async () => {
    const config = await fixture();
    const session = "restart-session";
    const expected = await accountFor(config, session);
    const before = counts();
    const turns = await Promise.all(Array.from({ length: 8 }, () =>
      send(config, "/v1/chat/completions", false, { "thread-id": session })));
    expect(turns.map((turn) => turn.status)).toEqual(Array(8).fill(200));
    expect(counts().map((value, index) => value - before[index])).toEqual(expected === 0 ? [8, 0] : [0, 8]);
    const sessions = await Promise.all(Array.from({ length: 32 }, (_, index) =>
      send(config, "/v1/chat/completions", false, { "session-id": `distinct-session-${index}` })));
    for (const response of sessions) expect(response.status, response.text).toBe(200);
    const contents = sessions.map((response) =>
      (JSON.parse(response.text) as { choices: Array<{ message: { content: string } }> }).choices[0].message.content);
    expect(new Set(contents)).toEqual(new Set(peers.map((peer) => peer.name)));
    // 不改变etcd池或HTTP peers，只替换网关进程；随机seed/进程会话表会在此失效。
    const prefix = app.etcdPrefix;
    await app.stop();
    app = await spawnApp({ threadPerCore: false, etcdPrefix: prefix });
    await ready(config.caller);
    expect(await accountFor(config, session)).toBe(expected);
  });

  test("503 and cooldown fail the session without calling the other account", async () => {
    const config = await fixture();
    const session = "unavailable-session";
    const chosen = await accountFor(config, session);
    const before = counts();
    peers[chosen].failing = true;
    try {
      const first = await send(config, "/v1/chat/completions", false, { "session-id": session });
      expect(first.status, first.text).toBe(503);
      expect(counts().map((value, index) => value - before[index])).toEqual(chosen === 0 ? [1, 0] : [0, 1]);
      const cooling = counts();
      for (const [path, stream] of SURFACES) {
        const response = await send(config, path, stream, { "session-id": session });
        expect(response.status, `${path} ${stream}: ${response.text}`).toBe(503);
      }
      // 任何handler先过滤cooldown再建小环都会让另一个peer计数增加。
      expect(counts()).toEqual(cooling);
    } finally {
      peers[chosen].failing = false;
    }
  });

  for (const removed of ["models", "provider_keys"] as const) {
    test(`a missing configured ${removed} cannot silently shrink the pool`, async () => {
      const config = await fixture();
      const session = `missing-${removed}-session`;
      const chosen = await accountFor(config, session);
      await seed.delete(removed, removed === "models" ? config.modelIds[chosen] : config.accountIds[chosen]);
      await barrier(config.model);
      const before = counts();
      for (const [path, stream] of SURFACES) {
        const response = await send(config, path, stream, { "session-id": session });
        expect(response.status, `${path} ${stream}: ${response.text}`).toBe(503);
      }
      expect(counts()).toEqual(before);
    });
  }

  test("ordinary consistent_hash still fails over when hash_failover is omitted", async () => {
    // false若错误地改变所有哈希配置，这条旧合同就会失败。
    const config = await fixture(false);
    const session = "ordinary-hash-session";
    const chosen = await accountFor(config, session);
    const before = counts();
    peers[chosen].failing = true;
    try {
      const response = await send(config, "/v1/chat/completions", false, { "session-id": session });
      expect(response.status, response.text).toBe(200);
      expect((JSON.parse(response.text) as { choices: Array<{ message: { content: string } }> }).choices[0].message.content)
        .toBe(peers[1 - chosen].name);
      expect(counts().map((value, index) => value - before[index])).toEqual([1, 1]);
    } finally {
      peers[chosen].failing = false;
    }
  });
});
