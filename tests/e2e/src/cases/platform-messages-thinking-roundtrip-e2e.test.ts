import { createHash } from "node:crypto";
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

// E2E: an agent on /v1/messages with extended thinking and tool use, through
// a platform-mode gateway, over two turns.
//
// The upstream answers the first turn with a signed thinking block, a
// redacted thinking block and a tool call. The agent sends that assistant
// turn back as it received it, with the tool's result. The upstream can only
// verify its thinking signature and pair the result with its call if it gets
// back exactly what it issued, so the second turn must reach it with the
// first turn's tool-use id, thinking text, signature and redacted data
// byte-identical to what it sent. That holds whether the agent read the
// first turn buffered or assembled it from the stream.

const CALLER_PLAINTEXT = "sk-platform-thinking-e2e-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_THINKING_KEY";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "5be1c0d27a9f4e3b8c6d1a0f2e4b7c9d3a5f8e1b0c2d4f6a8e9b1c3d5f7a9e2b";

const HEADERS = {
  authorization: `Bearer ${CALLER_PLAINTEXT}`,
  "content-type": "application/json",
  "anthropic-version": "2023-06-01",
};

/** What the upstream issued: none of it may change on its way back. */
const TOOL_USE_ID = "toolu_01A09q90qw90lq917835lq9";
const THINKING = "The user wants the weather in Paris.\nI should call get_weather — Paris, 巴黎.";
const SIGNATURE = "ErUBCkYIBRgCIkD0y3+Qw/8xZ1Lm9aT2vNq7kP4s+R6uE1oYb5cW8dHgJ0iFfKzLxMnBpVeXjTyUq3wSaIlOr==";
const REDACTED = "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5L8rLVyIwxtE3rAFBa8cr3qpPkNRj2YfWXGmKDxH4mPnZ5sQ7vB8=";
const ARGS = { city: "Paris" };

const TOOLS = [
  {
    name: "get_weather",
    description: "Current weather for a city",
    input_schema: { type: "object", properties: { city: { type: "string" } }, required: ["city"] },
  },
];
const USER_TURN = { role: "user", content: "What is the weather in Paris?" };
const THINKING_ON = { type: "enabled", budget_tokens: 1024 };

/** The first turn, buffered. */
function firstAnswer(): Record<string, unknown> {
  return {
    id: "msg_01XFDUDYJgAACzvnptvVoYEL",
    type: "message",
    role: "assistant",
    model: "upstream-model",
    content: [
      { type: "thinking", thinking: THINKING, signature: SIGNATURE },
      { type: "redacted_thinking", data: REDACTED },
      { type: "text", text: "Let me check." },
      { type: "tool_use", id: TOOL_USE_ID, name: "get_weather", input: ARGS },
    ],
    stop_reason: "tool_use",
    stop_sequence: null,
    usage: { input_tokens: 40, output_tokens: 30 },
  };
}

/** The same first turn, streamed. */
function firstStream(): string[] {
  const frame = (event: string, data: unknown) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  return [
    frame("message_start", {
      type: "message_start",
      message: {
        id: "msg_01XFDUDYJgAACzvnptvVoYEL",
        type: "message",
        role: "assistant",
        model: "upstream-model",
        content: [],
        stop_reason: null,
        stop_sequence: null,
        usage: { input_tokens: 40, output_tokens: 1 },
      },
    }),
    frame("content_block_start", {
      type: "content_block_start",
      index: 0,
      content_block: { type: "thinking", thinking: "", signature: "" },
    }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "thinking_delta", thinking: THINKING.slice(0, 20) },
    }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "thinking_delta", thinking: THINKING.slice(20) },
    }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "signature_delta", signature: SIGNATURE },
    }),
    frame("content_block_stop", { type: "content_block_stop", index: 0 }),
    frame("content_block_start", {
      type: "content_block_start",
      index: 1,
      content_block: { type: "redacted_thinking", data: REDACTED },
    }),
    frame("content_block_stop", { type: "content_block_stop", index: 1 }),
    frame("content_block_start", {
      type: "content_block_start",
      index: 2,
      content_block: { type: "tool_use", id: TOOL_USE_ID, name: "get_weather", input: {} },
    }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 2,
      delta: { type: "input_json_delta", partial_json: JSON.stringify(ARGS) },
    }),
    frame("content_block_stop", { type: "content_block_stop", index: 2 }),
    frame("message_delta", {
      type: "message_delta",
      delta: { stop_reason: "tool_use", stop_sequence: null },
      usage: { output_tokens: 30 },
    }),
    frame("message_stop", { type: "message_stop" }),
  ];
}

/** The second turn: the answer after the tool ran. */
function secondAnswer(): Record<string, unknown> {
  return {
    id: "msg_01Aq9w938a90dw8q",
    type: "message",
    role: "assistant",
    model: "upstream-model",
    content: [{ type: "text", text: "It is sunny in Paris." }],
    stop_reason: "end_turn",
    stop_sequence: null,
    usage: { input_tokens: 80, output_tokens: 8 },
  };
}

/** Every JSON `data:` payload of an SSE body, in order. */
function sseEvents(text: string): Array<Record<string, any>> {
  return text.split(/\r?\n\r?\n/).flatMap((frame) => {
    const data = frame
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).replace(/^ /, ""))
      .join("\n");
    return data ? [JSON.parse(data) as Record<string, any>] : [];
  });
}

/** The assistant content an SDK assembles from a Messages stream. */
function assembleContent(text: string): Array<Record<string, any>> {
  const blocks: Array<Record<string, any>> = [];
  const json: string[] = [];
  for (const event of sseEvents(text)) {
    if (event.type === "content_block_start") {
      blocks[event.index] = { ...event.content_block };
      json[event.index] = "";
    } else if (event.type === "content_block_delta") {
      const block = blocks[event.index]!;
      const delta = event.delta;
      if (delta.type === "thinking_delta") block.thinking += delta.thinking;
      else if (delta.type === "signature_delta") block.signature = (block.signature ?? "") + delta.signature;
      else if (delta.type === "text_delta") block.text += delta.text;
      else if (delta.type === "input_json_delta") json[event.index] += delta.partial_json;
    } else if (event.type === "content_block_stop") {
      const block = blocks[event.index]!;
      if (block.type === "tool_use" && json[event.index]) block.input = JSON.parse(json[event.index]!);
    }
  }
  return blocks;
}

describe("platform mode e2e: a thinking agent's second turn reaches the upstream as it issued it", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  const up: { buffered?: OpenAiUpstream; streamed?: OpenAiUpstream } = {};

  const post = async (body: unknown) => {
    const res = await fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: HEADERS,
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    up.buffered = await startOpenAiUpstream({
      scriptedResponses: [{ nonStreamBody: firstAnswer() }, { nonStreamBody: secondAnswer() }],
    });
    up.streamed = await startOpenAiUpstream({
      scriptedResponses: [{ rawStreamFrames: firstStream() }, { nonStreamBody: secondAnswer() }],
    });
    app = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [alias, upstream] of [
      ["pt-thinking-buffered", up.buffered],
      ["pt-thinking-streamed", up.streamed],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `pk-${alias}`,
        provider: "anthropic",
        adapter: "anthropic",
        secret: "sk-ant-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: alias,
        provider: "anthropic",
        model_name: "upstream-model",
        provider_key_id: pk.id,
      });
    }
    // The caller key last: once it authenticates, both models are loaded.
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    const probe = new ProxyClient(app.proxyUrl, CALLER_PLAINTEXT);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await up.buffered?.close();
    await up.streamed?.close();
  });

  for (const mode of ["buffered", "streamed"] as const) {
    test(`first turn ${mode}: the tool-use id, thinking signature and redacted data come back unchanged`, async (ctx) => {
      if (!etcdReachable || !app) return void ctx.skip();
      const upstream = up[mode]!;
      const model = `pt-thinking-${mode}`;

      const first = await post({
        model,
        max_tokens: 2048,
        thinking: THINKING_ON,
        tools: TOOLS,
        messages: [USER_TURN],
        ...(mode === "streamed" ? { stream: true } : {}),
      });
      expect(first.status, first.text).toBe(200);
      const content =
        mode === "streamed" ? assembleContent(first.text) : (JSON.parse(first.text).content as Array<Record<string, any>>);
      const toolUse = content.find((b) => b.type === "tool_use");
      expect(toolUse, first.text).toBeDefined();

      // The agent's next turn: the assistant turn as it was handed, then the
      // tool's result for the call it named.
      const second = await post({
        model,
        max_tokens: 2048,
        thinking: THINKING_ON,
        tools: TOOLS,
        messages: [
          USER_TURN,
          { role: "assistant", content },
          { role: "user", content: [{ type: "tool_result", tool_use_id: toolUse!.id, content: "sunny, 21°C" }] },
        ],
      });
      expect(second.status, second.text).toBe(200);
      expect(JSON.parse(second.text).content[0].text).toBe("It is sunny in Paris.");

      expect(upstream.receivedRequests.length).toBe(2);
      const forwarded = JSON.parse(upstream.receivedRequests[1]!.body) as Record<string, any>;
      const assistant = forwarded.messages[1].content as Array<Record<string, any>>;
      const thinking = assistant.find((b) => b.type === "thinking");
      expect(thinking?.thinking).toBe(THINKING);
      expect(thinking?.signature).toBe(SIGNATURE);
      expect(assistant.find((b) => b.type === "redacted_thinking")?.data).toBe(REDACTED);
      const call = assistant.find((b) => b.type === "tool_use");
      expect(call?.id).toBe(TOOL_USE_ID);
      expect(call?.input).toEqual(ARGS);
      expect(forwarded.messages[2].content[0].tool_use_id).toBe(TOOL_USE_ID);
      // Byte for byte, as the upstream wrote them.
      const raw = upstream.receivedRequests[1]!.body;
      for (const issued of [TOOL_USE_ID, SIGNATURE, REDACTED]) {
        expect(raw, `second turn carries ${issued}`).toContain(issued);
      }
    });
  }
});
