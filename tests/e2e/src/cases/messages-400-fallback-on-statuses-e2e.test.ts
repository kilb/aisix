import { createHash, randomBytes } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type ReceivedRequest,
  type SpawnedApp,
} from "../harness/index.js";

// 2026-11-11 的真实 Messages 失败：旧渠道只接受两种 thinking.display，
// 另一个渠道找不到 WebSearch 引用；fallback_on_statuses 为空时均只调用一次。
// 官方 beta 允许 updates，工具引用必须保留对应定义；切换不能靠删字段降级。
// https://platform.claude.com/docs/en/build-with-claude/thinking
// https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool
const CALLER = "sk-messages-400-fallback-caller";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const BETA = "thinking-display-updates-2026-08-18";
const BACKUP_TEXT = "The backup accepted the original request.";
const THINKING = { type: "adaptive", display: "updates" };
const TOOLS = [
  {
    name: "ToolSearch",
    description: "Find tools by name",
    input_schema: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
    },
  },
  {
    name: "WebSearch",
    description: "Search official documentation",
    input_schema: {
      type: "object",
      properties: { query: { type: "string" } },
      required: ["query"],
    },
    defer_loading: true,
  },
];
const MESSAGES = [
  { role: "user", content: "Find the official API error documentation." },
  {
    role: "assistant",
    content: [{ type: "tool_use", id: "toolu_find_web", name: "ToolSearch", input: { query: "WebSearch" } }],
  },
  {
    role: "user",
    content: [{
      type: "tool_result",
      tool_use_id: "toolu_find_web",
      content: [{ type: "tool_reference", tool_name: "WebSearch" }],
    }],
  },
];
const FAILURES = [
  { name: "thinking", message: "thinking.adaptive.display: Input should be 'summarized', 'omitted'" },
  { name: "tool-reference", message: "Tool reference 'WebSearch' not found in available tools" },
];

function messageBody(): Record<string, unknown> {
  return {
    id: "msg_messages_400_backup",
    type: "message",
    role: "assistant",
    model: "claude-opus-5-5",
    content: [{ type: "text", text: BACKUP_TEXT }],
    stop_reason: "end_turn",
    stop_sequence: null,
    usage: { input_tokens: 8, output_tokens: 4 },
  };
}

function messageFrames(): string[] {
  const frame = (type: string, rest: Record<string, unknown> = {}) =>
    `event: ${type}\ndata: ${JSON.stringify({ type, ...rest })}\n\n`;
  return [
    frame("message_start", { message: { ...messageBody(), content: [], stop_reason: null } }),
    frame("content_block_start", { index: 0, content_block: { type: "text", text: "" } }),
    frame("content_block_delta", { index: 0, delta: { type: "text_delta", text: BACKUP_TEXT } }),
    frame("content_block_stop", { index: 0 }),
    frame("message_delta", { delta: { stop_reason: "end_turn" }, usage: { output_tokens: 4 } }),
    frame("message_stop"),
  ];
}

function expectOriginalRequest(request: ReceivedRequest, stream: boolean): void {
  expect(request.method).toBe("POST");
  expect(request.path).toBe("/v1/messages");
  expect(request.headers["anthropic-version"]).toBe("2023-06-01");
  expect(request.headers["anthropic-beta"]?.split(",").map((v) => v.trim())).toContain(BETA);
  const body = JSON.parse(request.body) as Record<string, unknown>;
  expect(body.thinking).toEqual(THINKING);
  expect(body.tools).toEqual(TOOLS);
  expect(body.messages).toEqual(MESSAGES);
  expect(body.stream).toBe(stream);
}

function expectBackupAnswer(response: Response, text: string, stream: boolean): void {
  expect(response.status).toBe(200);
  if (stream) {
    expect(response.headers.get("content-type")).toContain("text/event-stream");
    const events = text.split(/\r?\n/)
      .filter((line) => line.startsWith("data: "))
      .map((line) => JSON.parse(line.slice(6)) as Record<string, unknown>);
    expect(events[0]?.type).toBe("message_start");
    expect(events.at(-1)?.type).toBe("message_stop");
    expect(events.filter((event) => event.type === "content_block_delta").map((event) => event.delta))
      .toEqual([{ type: "text_delta", text: BACKUP_TEXT }]);
  } else {
    expect(response.headers.get("content-type")).toContain("application/json");
    const body = JSON.parse(text) as Record<string, unknown>;
    expect(body.type).toBe("message");
    expect(body.content).toEqual([{ type: "text", text: BACKUP_TEXT }]);
    expect(body.stop_reason).toBe("end_turn");
  }
}

describe("Messages 400: explicitly configured failover preserves the original request", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  const failed = new Map<string, OpenAiUpstream>();
  const healthy = new Map<boolean, OpenAiUpstream>();

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    await Promise.all([
      ...FAILURES.map(async (failure) => {
        failed.set(failure.name, await startOpenAiUpstream({
          status: 400,
          errorBody: { type: "error", error: { type: "invalid_request_error", message: failure.message } },
        }));
      }),
      ...[false, true].map(async (stream) => {
        healthy.set(stream, await startOpenAiUpstream(stream
          ? { rawStreamFrames: messageFrames() }
          : { nonStreamBody: messageBody() }));
      }),
    ]);
    const secretEnv = "MESSAGES_400_FALLBACK_PLATFORM_KEY";
    app = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: secretEnv,
        [secretEnv]: randomBytes(32).toString("hex"),
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [name, upstream] of [
      ...failed.entries(),
      ...[...healthy.entries()].map(([stream, upstream]) => [`healthy-${stream}`, upstream] as const),
    ]) {
      const key = await seed.createProviderKey({
        display_name: `messages-400-${name}-pk`,
        provider: "anthropic",
        adapter: "anthropic",
        secret: "sk-ant-messages-400-peer",
        api_base: upstream.baseUrl,
      });
      await seed.createModel({
        display_name: `messages-400-${name}`,
        provider: "anthropic",
        model_name: "claude-opus-5-5",
        provider_key_id: key.id,
      });
    }
    for (const failure of FAILURES) {
      for (const stream of [false, true]) {
        for (const configured of [false, true]) {
          await seed.createModel({
            display_name: `messages-400-${failure.name}-${stream}-${configured}`,
            routing: {
              strategy: "failover",
              targets: [
                { model: `messages-400-${failure.name}` },
                { model: `messages-400-healthy-${stream}` },
              ],
              retries: 0,
              ...(configured ? { fallback_on_statuses: [400] } : {}),
            },
          });
        }
      }
    }
    await seed.createModel({
      display_name: "messages-400-all-failed",
      routing: {
        strategy: "failover",
        targets: FAILURES.map((failure) => ({ model: `messages-400-${failure.name}` })),
        fallback_on_statuses: [400],
        retries: 0,
      },
    });
    // 调用者最后写入；认证可用即证明之前所有模型已进入快照，不先触发受测行为。
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["*"] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([...failed.values(), ...healthy.values()].map((upstream) => upstream.close()));
  });

  async function post(model: string, stream: boolean): Promise<Response> {
    return fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
        "anthropic-beta": BETA,
      },
      body: JSON.stringify({ model, max_tokens: 128, stream, thinking: THINKING, tools: TOOLS, messages: MESSAGES }),
    });
  }

  for (const failure of FAILURES) {
    for (const stream of [false, true]) {
      for (const configured of [false, true]) {
        test(`${failure.name}, stream=${stream}, configured=${configured}: ${configured ? "tries the backup once" : "keeps the default terminal 400"}`, async (ctx) => {
          if (!etcdReachable || !app) {
            ctx.skip();
            return;
          }
          const first = failed.get(failure.name)!;
          const backup = healthy.get(stream)!;
          const firstBefore = first.receivedRequests.length;
          const backupBefore = backup.receivedRequests.length;
          const response = await post(`messages-400-${failure.name}-${stream}-${configured}`, stream);
          const text = await response.text();
          if (configured) {
            expectBackupAnswer(response, text, stream);
          } else {
            expect(response.status).toBe(400);
            expect(JSON.parse(text).error.message).toBe(failure.message);
          }
          expect(first.receivedRequests.length - firstBefore).toBe(1);
          expect(backup.receivedRequests.length - backupBefore).toBe(configured ? 1 : 0);
          expectOriginalRequest(first.receivedRequests[firstBefore]!, stream);
          if (configured) expectOriginalRequest(backup.receivedRequests[backupBefore]!, stream);
        });
      }
    }
  }

  for (const stream of [false, true]) {
    test(`stream=${stream}: all channels reject, each is tried once and the final 400 is preserved`, async (ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      const before = new Map([...failed.entries()].map(([name, upstream]) => [name, upstream.receivedRequests.length]));
      const response = await post("messages-400-all-failed", stream);
      const body = await response.json() as { error: { message: string } };
      expect(response.status).toBe(400);
      expect(body.error.message).toBe(FAILURES.at(-1)!.message);
      for (const [name, upstream] of failed) {
        expect(upstream.receivedRequests.length - before.get(name)!).toBe(1);
        expectOriginalRequest(upstream.receivedRequests[before.get(name)!]!, stream);
      }
    });
  }
});
