import { createHash } from "node:crypto";
import OpenAI from "openai";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: STREAMED tool use from a Claude upstream reaches the caller.
//
// User journey: an agent framework streams a tool-calling turn through the
// gateway against a Model backed by Claude — either the Anthropic Messages
// API directly or Claude on Vertex AI (`:streamRawPredict`, which streams
// the same Messages SSE). Claude streams each `tool_use` block as a
// `content_block_start` carrying the call `id` + `name`, followed by
// `input_json_delta` fragments carrying the JSON arguments
// (<https://docs.anthropic.com/en/docs/build-with-claude/streaming>,
// "Streaming request with tool use"). The caller must get those calls back
// in the protocol it speaks:
//
//   - /v1/chat/completions → `delta.tool_calls[]` fragments correlated by
//     `index`, the position of the call in the assembled `tool_calls` array
//     (<https://platform.openai.com/docs/api-reference/chat-streaming>), then
//     `finish_reason: "tool_calls"`.
//   - /v1/responses → `function_call` output items with `call_id`, `name`
//     and the full `arguments` string
//     (<https://platform.openai.com/docs/api-reference/responses-streaming>).
//   - /v1/messages → `tool_use` content blocks whose `input_json_delta`
//     fragments rebuild the original input.
//
// The non-streaming translation is pinned by tools-cross-provider-e2e;
// this file pins the streaming one, where a dropped call is silent: the
// stream still ends 200 with `finish_reason: "tool_calls"` and no calls.

const CALLER = "sk-anth-stream-tools-caller";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");

const SF_INPUT = { location: "San Francisco, CA", unit: "celsius" };
const TOKYO_INPUT = { location: "Tokyo", unit: "celsius" };
const LEAD_TEXT = "Let me check both cities.";

// A text block followed by three parallel tool_use blocks, in the event
// order the Messages API streams them (`ping`s, an empty first
// `input_json_delta`, argument fragments split mid-token). The last call
// takes no arguments, so its only fragment is the empty one. Between the
// calls Claude runs its own web search: a `server_tool_use` block whose
// query streams as `input_json_delta` too, then the result block
// (<https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/web-search-tool>,
// "Streaming"). The upstream executes that search itself, so it is not a
// call the client is asked to make.
const CLAUDE_TOOL_STREAM = [
  JSON.stringify({
    type: "message_start",
    message: {
      id: "msg_stream_tools_01",
      type: "message",
      role: "assistant",
      model: "claude-sonnet-4-5",
      content: [],
      stop_reason: null,
      stop_sequence: null,
      usage: { input_tokens: 472, output_tokens: 2 },
    },
  }),
  JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
  JSON.stringify({ type: "ping" }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "Let me check " } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "both cities." } }),
  JSON.stringify({ type: "content_block_stop", index: 0 }),
  JSON.stringify({
    type: "content_block_start",
    index: 1,
    content_block: { type: "tool_use", id: "toolu_stream_sf", name: "get_weather", input: {} },
  }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: '{"location": "San Fra' } }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: 'ncisco, CA", "unit": "cel' } }),
  JSON.stringify({ type: "content_block_delta", index: 1, delta: { type: "input_json_delta", partial_json: 'sius"}' } }),
  JSON.stringify({ type: "content_block_stop", index: 1 }),
  JSON.stringify({
    type: "content_block_start",
    index: 2,
    content_block: { type: "tool_use", id: "toolu_stream_tokyo", name: "get_weather", input: {} },
  }),
  JSON.stringify({ type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: '{"location": "Tokyo", ' } }),
  JSON.stringify({ type: "content_block_delta", index: 2, delta: { type: "input_json_delta", partial_json: '"unit": "celsius"}' } }),
  JSON.stringify({ type: "content_block_stop", index: 2 }),
  JSON.stringify({
    type: "content_block_start",
    index: 3,
    content_block: { type: "server_tool_use", id: "srvtoolu_stream_search", name: "web_search" },
  }),
  JSON.stringify({ type: "content_block_delta", index: 3, delta: { type: "input_json_delta", partial_json: '{"query": "tokyo weather"}' } }),
  JSON.stringify({ type: "content_block_stop", index: 3 }),
  JSON.stringify({
    type: "content_block_start",
    index: 4,
    content_block: {
      type: "web_search_tool_result",
      tool_use_id: "srvtoolu_stream_search",
      content: [{ type: "web_search_result", title: "Tokyo weather", url: "https://example.com/tokyo" }],
    },
  }),
  JSON.stringify({ type: "content_block_stop", index: 4 }),
  JSON.stringify({
    type: "content_block_start",
    index: 5,
    content_block: { type: "tool_use", id: "toolu_stream_time", name: "get_local_time", input: {} },
  }),
  JSON.stringify({ type: "content_block_delta", index: 5, delta: { type: "input_json_delta", partial_json: "" } }),
  JSON.stringify({ type: "content_block_stop", index: 5 }),
  JSON.stringify({
    type: "message_delta",
    delta: { stop_reason: "tool_use", stop_sequence: null },
    usage: { output_tokens: 89 },
  }),
  JSON.stringify({ type: "message_stop" }),
];

const TIME_TOOL = {
  type: "function" as const,
  function: {
    name: "get_local_time",
    description: "Get the caller's local time",
    parameters: { type: "object", properties: {} },
  },
};

const WEATHER_TOOL = {
  type: "function" as const,
  function: {
    name: "get_weather",
    description: "Get the current weather in a given location",
    parameters: {
      type: "object",
      properties: {
        location: { type: "string" },
        unit: { type: "string", enum: ["celsius", "fahrenheit"] },
      },
      required: ["location"],
    },
  },
};

interface SseFrame {
  event: string | undefined;
  data: string;
}

/** Split a complete `text/event-stream` body into frames. */
function parseSse(body: string): SseFrame[] {
  const frames: SseFrame[] = [];
  for (const block of body.replace(/\r\n/g, "\n").split("\n\n")) {
    let event: string | undefined;
    const data: string[] = [];
    for (const line of block.split("\n")) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
    }
    if (data.length > 0) frames.push({ event, data: data.join("\n") });
  }
  return frames;
}

interface AssembledCall {
  id: string | undefined;
  type: string | undefined;
  name: string;
  arguments: string;
}

/**
 * Drive one streamed chat completion through the OpenAI SDK and assemble
 * it the way every OpenAI-shaped agent loop does: `delta.content`
 * concatenated, `delta.tool_calls[]` fragments merged by `index`, every
 * string but `type` concatenated across fragments as the SDK's stream
 * accumulator does — so a call `id` repeated on a later fragment shows.
 */
async function streamChat(client: OpenAI, model: string) {
  const stream = await client.chat.completions.create({
    model,
    stream: true,
    tools: [WEATHER_TOOL, TIME_TOOL],
    messages: [{ role: "user", content: "What's the weather in San Francisco and Tokyo, and what time is it here?" }],
  });
  let content = "";
  let finishReason: string | null = null;
  const calls = new Map<number, AssembledCall>();
  for await (const chunk of stream) {
    const choice = chunk.choices[0];
    if (!choice) continue;
    content += choice.delta.content ?? "";
    for (const tc of choice.delta.tool_calls ?? []) {
      const call = calls.get(tc.index) ?? { id: undefined, type: undefined, name: "", arguments: "" };
      if (tc.id) call.id = (call.id ?? "") + tc.id;
      if (tc.type) call.type = tc.type;
      if (tc.function?.name) call.name += tc.function.name;
      if (tc.function?.arguments) call.arguments += tc.function.arguments;
      calls.set(tc.index, call);
    }
    if (choice.finish_reason) finishReason = choice.finish_reason;
  }
  return { content, finishReason, calls };
}

function expectAllThreeCalls(calls: Map<number, AssembledCall>): void {
  // `index` is the call's position in the assembled `tool_calls` array, so
  // three calls must land on 0, 1 and 2 — not on the Claude content-block
  // positions (1, 2 and 5) that the text and search blocks shifted — and
  // the search the upstream ran itself is not among them.
  expect([...calls.keys()].sort()).toEqual([0, 1, 2]);
  const [first, second, third] = [calls.get(0)!, calls.get(1)!, calls.get(2)!];
  expect(first.id).toBe("toolu_stream_sf");
  expect(first.type).toBe("function");
  expect(first.name).toBe("get_weather");
  expect(JSON.parse(first.arguments)).toEqual(SF_INPUT);
  expect(second.id).toBe("toolu_stream_tokyo");
  expect(second.type).toBe("function");
  expect(second.name).toBe("get_weather");
  expect(JSON.parse(second.arguments)).toEqual(TOKYO_INPUT);
  // A call without arguments still carries a JSON object, as `arguments`
  // does on a non-streamed response.
  expect(third.id).toBe("toolu_stream_time");
  expect(third.type).toBe("function");
  expect(third.name).toBe("get_local_time");
  expect(JSON.parse(third.arguments)).toEqual({});
}

describe("streamed tool use from a Claude upstream reaches the caller", () => {
  let app: SpawnedApp | undefined;
  let anthropicUpstream: OpenAiUpstream | undefined;
  let vertexUpstream: OpenAiUpstream | undefined;
  let client: OpenAI;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    anthropicUpstream = await startOpenAiUpstream({ streamEvents: CLAUDE_TOOL_STREAM, eventDelayMs: 2 });
    vertexUpstream = await startOpenAiUpstream({ streamEvents: CLAUDE_TOOL_STREAM, eventDelayMs: 2 });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const anthropicPk = await seed.createProviderKey({
      display_name: "stream-tools-anthropic-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: anthropicUpstream.baseUrl,
    });
    await seed.createModel({
      display_name: "stream-tools-anthropic",
      provider: "anthropic",
      model_name: "claude-sonnet-4-5",
      provider_key_id: anthropicPk.id,
    });

    const vertexPk = await seed.createProviderKey({
      display_name: "stream-tools-vertex-pk",
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "ya29.e2e-test", project: "proj-e2e", region: "us-east5" }),
      api_base: vertexUpstream.baseUrl,
    });
    await seed.createModel({
      display_name: "stream-tools-vertex-claude",
      provider: "google",
      model_name: "claude-sonnet-4-5",
      provider_key_id: vertexPk.id,
    });

    // Caller key last: once it authenticates, every resource above is in
    // the snapshot.
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: ["stream-tools-anthropic", "stream-tools-vertex-claude"],
    });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);

    client = new OpenAI({ apiKey: CALLER, baseURL: `${app.proxyUrl}/v1`, maxRetries: 0 });
  });

  afterAll(async () => {
    await app?.exit();
    await anthropicUpstream?.close();
    await vertexUpstream?.close();
  });

  test("/v1/chat/completions stream → Anthropic: tool_calls deltas reassemble every call", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const { content, finishReason, calls } = await streamChat(client, "stream-tools-anthropic");
    expect(content).toBe(LEAD_TEXT);
    expectAllThreeCalls(calls);
    expect(finishReason).toBe("tool_calls");
  });

  test("/v1/chat/completions stream → Claude on Vertex AI: tool_calls deltas reassemble every call", async (ctx) => {
    if (!etcdReachable || !app || !vertexUpstream) {
      ctx.skip();
      return;
    }
    const { content, finishReason, calls } = await streamChat(client, "stream-tools-vertex-claude");
    // Confirms the request rode the Claude-on-Vertex streaming rail.
    expect(vertexUpstream.receivedRequests.at(-1)?.path).toMatch(
      /\/publishers\/anthropic\/models\/claude-sonnet-4-5:streamRawPredict$/,
    );
    expect(content).toBe(LEAD_TEXT);
    expectAllThreeCalls(calls);
    expect(finishReason).toBe("tool_calls");
  });

  test("/v1/responses stream → Anthropic: every call surfaces as a function_call item", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await fetch(`${app.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model: "stream-tools-anthropic",
        stream: true,
        input: "What's the weather in San Francisco and Tokyo, and what time is it here?",
        tools: [WEATHER_TOOL, TIME_TOOL].map(({ function: f }) => ({
          type: "function",
          name: f.name,
          description: f.description,
          parameters: f.parameters,
        })),
      }),
    });
    expect(res.status).toBe(200);
    const frames = parseSse(await res.text());
    const events = frames
      .filter((f) => f.data !== "[DONE]")
      .map((f) => JSON.parse(f.data) as Record<string, unknown> & { type: string });

    // Live events: each call is announced, then its arguments stream in.
    const added = events
      .filter((e) => e.type === "response.output_item.added")
      .map((e) => e.item as Record<string, unknown>)
      .filter((item) => item.type === "function_call");
    expect(added.map((i) => [i.call_id, i.name])).toEqual([
      ["toolu_stream_sf", "get_weather"],
      ["toolu_stream_tokyo", "get_weather"],
      ["toolu_stream_time", "get_local_time"],
    ]);
    const expectedInput: Record<string, unknown> = {
      toolu_stream_sf: SF_INPUT,
      toolu_stream_tokyo: TOKYO_INPUT,
      toolu_stream_time: {},
    };
    for (const item of added) {
      const streamedArgs = events
        .filter((e) => e.type === "response.function_call_arguments.delta" && e.item_id === item.id)
        .map((e) => e.delta as string)
        .join("");
      expect(JSON.parse(streamedArgs)).toEqual(expectedInput[item.call_id as string]);
    }

    // Terminal snapshot: the completed response carries every call whole.
    const completed = events.find((e) => e.type === "response.completed");
    expect(completed, "stream must end with response.completed").toBeDefined();
    const output = (completed!.response as { output: Array<Record<string, unknown>> }).output;
    const fnCalls = output.filter((o) => o.type === "function_call");
    expect(fnCalls.map((c) => [c.call_id, c.name, JSON.parse(c.arguments as string)])).toEqual([
      ["toolu_stream_sf", "get_weather", SF_INPUT],
      ["toolu_stream_tokyo", "get_weather", TOKYO_INPUT],
      ["toolu_stream_time", "get_local_time", {}],
    ]);
  });

  test("/v1/messages stream → Claude on Vertex AI: tool_use blocks rebuild every input", async (ctx) => {
    if (!etcdReachable || !app || !vertexUpstream) {
      ctx.skip();
      return;
    }
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: "stream-tools-vertex-claude",
        max_tokens: 1024,
        stream: true,
        tools: [WEATHER_TOOL, TIME_TOOL].map(({ function: f }) => ({
          name: f.name,
          description: f.description,
          input_schema: f.parameters,
        })),
        messages: [{ role: "user", content: "What's the weather in San Francisco and Tokyo, and what time is it here?" }],
      }),
    });
    expect(res.status).toBe(200);
    const events = parseSse(await res.text()).map(
      (f) => JSON.parse(f.data) as Record<string, unknown> & { type: string },
    );

    // Rebuild content blocks the way an Anthropic SDK consumer does.
    const blocks = new Map<number, { type: string; id?: string; name?: string; text: string; json: string }>();
    for (const e of events) {
      const index = e.index as number;
      if (e.type === "content_block_start") {
        const cb = e.content_block as { type: string; id?: string; name?: string };
        blocks.set(index, { type: cb.type, id: cb.id, name: cb.name, text: "", json: "" });
      } else if (e.type === "content_block_delta") {
        const d = e.delta as { type: string; text?: string; partial_json?: string };
        const b = blocks.get(index)!;
        if (d.type === "text_delta") b.text += d.text ?? "";
        if (d.type === "input_json_delta") b.json += d.partial_json ?? "";
      }
    }
    // An Anthropic SDK reads a block with no fragments as an empty input.
    const toolUses = [...blocks.values()].filter((b) => b.type === "tool_use");
    expect(toolUses.map((b) => [b.id, b.name, JSON.parse(b.json || "{}")])).toEqual([
      ["toolu_stream_sf", "get_weather", SF_INPUT],
      ["toolu_stream_tokyo", "get_weather", TOKYO_INPUT],
      ["toolu_stream_time", "get_local_time", {}],
    ]);
    const text = [...blocks.values()]
      .filter((b) => b.type === "text")
      .map((b) => b.text)
      .join("");
    expect(text).toBe(LEAD_TEXT);
    const stop = events.find((e) => e.type === "message_delta")?.delta as { stop_reason?: string } | undefined;
    expect(stop?.stop_reason).toBe("tool_use");
  });
});
