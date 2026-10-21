import { createHash } from "node:crypto";
import OpenAI from "openai";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：上游是 Anthropic 的模型，经 OpenAI 形状的流式接口
// （/v1/chat/completions 与 /v1/responses）返回工具调用时，调用方要拿到完整的
// 工具调用：id、函数名、参数。
//
// Anthropic 流式里工具调用是独立的内容块：`content_block_start` 带
// `tool_use{id,name}`，参数随后以 `input_json_delta` 分片到达
// （https://docs.anthropic.com/en/api/messages-streaming）。OpenAI SDK、经 Claude
// 模型跑的 agent 循环拿不到这些，就只看到 `finish_reason: "tool_calls"` 而没有
// 任何工具调用，原地空转。
//
// 这里让上游先回一段文字、再回三个工具调用（两个的参数被切成几片，一个不带
// 参数），断言用官方 OpenAI SDK 的流式累加器拼出来的结果，以及 Responses 流的
// 终态输出。

const CALLER = "sk-anth-stream-tools-cross-format";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const MODEL = "anth-stream-tools";

const STREAM_EVENTS = [
  JSON.stringify({
    type: "message_start",
    message: {
      id: "msg_tools_1",
      type: "message",
      role: "assistant",
      content: [],
      model: "claude-sonnet-4-5",
      stop_reason: null,
      usage: { input_tokens: 30, output_tokens: 1 },
    },
  }),
  JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Let me check." } }),
  JSON.stringify({ type: "content_block_stop", index: 0 }),
  JSON.stringify({
    type: "content_block_start",
    index: 1,
    content_block: { type: "tool_use", id: "toolu_weather", name: "get_weather", input: {} },
  }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: '{"city": "Pa' } }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: 'ris"}' } }),
  JSON.stringify({ type: "content_block_stop", index: 1 }),
  JSON.stringify({
    type: "content_block_start",
    index: 2,
    content_block: { type: "tool_use", id: "toolu_time", name: "get_time", input: {} },
  }),
  JSON.stringify({ type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: '{"tz": "Europe/' } }),
  JSON.stringify({ type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: 'Paris"}' } }),
  JSON.stringify({ type: "content_block_stop", index: 2 }),
  // 不带参数的工具：只有一个空的参数分片。调用方拿到的参数仍须是合法 JSON。
  JSON.stringify({
    type: "content_block_start",
    index: 3,
    content_block: { type: "tool_use", id: "toolu_clock", name: "list_timezones", input: {} },
  }),
  JSON.stringify({ type: "content_block_delta", index: 3, delta: { type: "input_json_delta", partial_json: "" } }),
  JSON.stringify({ type: "content_block_stop", index: 3 }),
  JSON.stringify({ type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 40 } }),
  JSON.stringify({ type: "message_stop" }),
];

const CHAT_TOOLS = [
  {
    type: "function" as const,
    function: {
      name: "get_weather",
      parameters: { type: "object", properties: { city: { type: "string" } } },
    },
  },
  {
    type: "function" as const,
    function: {
      name: "get_time",
      parameters: { type: "object", properties: { tz: { type: "string" } } },
    },
  },
  {
    type: "function" as const,
    function: { name: "list_timezones", parameters: { type: "object", properties: {} } },
  },
];

describe("streamed tool calls from an Anthropic upstream reach OpenAI-shape callers", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({ streamEvents: STREAM_EVENTS, eventDelayMs: 2 });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "anth-stream-tools-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-anth-mock",
      api_base: upstream.baseUrl,
    });
    await seed.createModel({
      display_name: MODEL,
      provider: "anthropic",
      model_name: "claude-sonnet-4-5",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: [MODEL] });
    await waitConfigPropagation(async () => {
      try {
        const r = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
          method: "POST",
          headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
          body: JSON.stringify({ model: MODEL, stream: true, messages: [{ role: "user", content: "probe" }] }),
        });
        await r.text();
        return r.ok;
      } catch {
        return false;
      }
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("/v1/chat/completions stream: the OpenAI SDK assembles every tool call", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const client = new OpenAI({ apiKey: CALLER, baseURL: `${app.proxyUrl}/v1`, maxRetries: 0 });
    const stream = client.beta.chat.completions.stream({
      model: MODEL,
      messages: [{ role: "user", content: "Weather and time in Paris?" }],
      tools: CHAT_TOOLS,
    });
    const done = await stream.finalChatCompletion();
    const choice = done.choices[0];
    expect(choice?.finish_reason).toBe("tool_calls");
    expect(choice?.message.content).toBe("Let me check.");
    const calls = choice?.message.tool_calls ?? [];
    expect(
      calls.map((c) => ({ id: c.id, type: c.type, name: c.function.name, args: JSON.parse(c.function.arguments) })),
    ).toEqual([
      { id: "toolu_weather", type: "function", name: "get_weather", args: { city: "Paris" } },
      { id: "toolu_time", type: "function", name: "get_time", args: { tz: "Europe/Paris" } },
      { id: "toolu_clock", type: "function", name: "list_timezones", args: {} },
    ]);
  });

  test("/v1/responses stream: the completed response carries every function call", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await fetch(`${app.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model: MODEL,
        stream: true,
        input: "Weather and time in Paris?",
        tools: CHAT_TOOLS.map((t) => ({ type: "function", ...t.function })),
      }),
    });
    expect(res.status).toBe(200);
    const events = (await res.text())
      .split("\n")
      .filter((l) => l.startsWith("data:"))
      .map((l) => l.slice(5).trim())
      .filter((d) => d.startsWith("{"))
      .map((d) => JSON.parse(d) as { type?: string; response?: { output?: Array<Record<string, unknown>> } });
    const completed = events.find((e) => e.type === "response.completed");
    expect(completed, "the stream must end with response.completed").toBeDefined();
    const calls = (completed?.response?.output ?? []).filter((o) => o.type === "function_call");
    expect(
      calls.map((c) => ({ call_id: c.call_id, name: c.name, args: JSON.parse(String(c.arguments)) })),
    ).toEqual([
      { call_id: "toolu_weather", name: "get_weather", args: { city: "Paris" } },
      { call_id: "toolu_time", name: "get_time", args: { tz: "Europe/Paris" } },
      { call_id: "toolu_clock", name: "list_timezones", args: {} },
    ]);
  });
});
