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

// E2E: in platform mode a tenant cannot tell which upstream channel served
// it from the ids it is handed, and multi-turn tool use still works.
//
// Every upstream below answers the way a model resold through a cloud
// platform does: its message id and tool-call ids carry a channel marker
// (`bdrk`), a Chat Completions answer names its own id and a
// `system_fingerprint`, and a relayed answer carries the platform's
// invocation metrics. None of it may reach the caller, on any surface,
// buffered or streamed, translated or relayed as the upstream wrote it.
//
// The tool-call round trip is what a real agent does next: it puts the id
// it was handed into the next turn's history.
//
// - Chat Completions and Messages carry the whole conversation on every
//   request, so the upstream only needs the ids in that one request to
//   agree: the id the caller was handed is the id the upstream receives.
// - Responses can continue a conversation the upstream stored
//   (`previous_response_id`), where only the ids it issued itself pair, so
//   the upstream must receive its OWN ids back.
//
// And the same upstream id must come back as the same gateway id every
// time, or an agent matching calls to results across turns breaks.
//
// The response-level id of a Responses answer is the second suite's subject:
// it belongs to the tenant it was handed to.

const CALLER_PLAINTEXT = "sk-platform-identity-e2e-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

/** The variable holding the platform secret — handed to the gateway process only. */
const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_SECRET";
const PLATFORM_SECRET = "platform-identity-e2e-secret-0123456789abcdef";

const UPSTREAM_MESSAGE_ID = "msg_bdrk_01X";
const UPSTREAM_TOOL_USE_ID = "toolu_bdrk_01Y";
const UPSTREAM_CHAT_ID = "chatcmpl-up";
const UPSTREAM_FC_ITEM_ID = "fc_bdrk_01Z";
const UPSTREAM_CALL_ID = "call_bdrk_01Y";
/** A tool-call id in the shape a Converse channel hands out. */
const UPSTREAM_CONVERSE_TOOL_ID = "tooluse_bdrk_01Q";
/** A server tool's use id in the shape a Vertex channel marks its ids. */
const UPSTREAM_SERVER_TOOL_ID = "srvtoolu_vrtx_01S";
const CHANNEL_METRICS = { inputTokenCount: 9, outputTokenCount: 4, invocationLatency: 120 };

const HEADERS = {
  authorization: `Bearer ${CALLER_PLAINTEXT}`,
  "content-type": "application/json",
};

const ALIASES = {
  chatOpenAi: "pi-chat-openai",
  chatAnthropic: "pi-chat-anthropic",
  messagesNative: "pi-messages-native",
  messagesTranslated: "pi-messages-translated",
  responsesNative: "pi-responses-native",
  responsesTranslated: "pi-responses-translated",
  responsesChat: "pi-responses-chat",
  messagesServerTool: "pi-messages-server-tool",
};

/** What must never appear anywhere in a response the caller receives. */
function expectNoUpstreamIdentity(text: string): void {
  for (const marker of [
    "_bdrk_",
    UPSTREAM_MESSAGE_ID,
    UPSTREAM_TOOL_USE_ID,
    UPSTREAM_FC_ITEM_ID,
    UPSTREAM_CALL_ID,
    "fp_bdrk_7",
    UPSTREAM_CHAT_ID,
    "system_fingerprint",
    "amazon-bedrock-invocationMetrics",
  ]) {
    expect(text, `response carries ${marker}`).not.toContain(marker);
  }
}

/** Every JSON `data:` payload of an SSE body, in order. */
function sseEvents(text: string): Array<Record<string, any>> {
  return text.split(/\r?\n\r?\n/).flatMap((frame) => {
    const data = frame
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).replace(/^ /, ""))
      .join("\n");
    if (!data || data === "[DONE]") return [];
    return [JSON.parse(data) as Record<string, any>];
  });
}

function upstreamBody(upstream: OpenAiUpstream, index: number): Record<string, any> {
  const req = upstream.receivedRequests[index];
  expect(req, `upstream request #${index}`).toBeDefined();
  return JSON.parse(req!.body) as Record<string, any>;
}

// ── Upstream answers ────────────────────────────────────────────────────

const WEATHER_ARGS = '{"city":"Paris"}';

/** A Chat Completions upstream answering with a tool call. */
function openAiChatAnswer(toolCallId = UPSTREAM_TOOL_USE_ID): Record<string, unknown> {
  return {
    id: UPSTREAM_CHAT_ID,
    object: "chat.completion",
    created: 1_760_000_000,
    model: "upstream-model",
    system_fingerprint: "fp_bdrk_7",
    choices: [
      {
        index: 0,
        message: {
          role: "assistant",
          content: null,
          tool_calls: [
            {
              id: toolCallId,
              type: "function",
              function: { name: "get_weather", arguments: WEATHER_ARGS },
            },
          ],
        },
        finish_reason: "tool_calls",
      },
    ],
    usage: { prompt_tokens: 9, completion_tokens: 4, total_tokens: 13 },
  };
}

function openAiChatStream(toolCallId = UPSTREAM_TOOL_USE_ID): string[] {
  const chunk = (choices: unknown[], extra: Record<string, unknown> = {}) =>
    JSON.stringify({
      id: UPSTREAM_CHAT_ID,
      object: "chat.completion.chunk",
      created: 1_760_000_000,
      model: "upstream-model",
      system_fingerprint: "fp_bdrk_7",
      choices,
      ...extra,
    });
  return [
    chunk([{ index: 0, delta: { role: "assistant", content: null }, finish_reason: null }]),
    chunk([
      {
        index: 0,
        delta: {
          tool_calls: [
            {
              index: 0,
              id: toolCallId,
              type: "function",
              function: { name: "get_weather", arguments: "" },
            },
          ],
        },
        finish_reason: null,
      },
    ]),
    chunk([
      {
        index: 0,
        delta: { tool_calls: [{ index: 0, function: { arguments: WEATHER_ARGS } }] },
        finish_reason: null,
      },
    ]),
    chunk([{ index: 0, delta: {}, finish_reason: "tool_calls" }]),
    chunk([], { usage: { prompt_tokens: 9, completion_tokens: 4, total_tokens: 13 } }),
    "[DONE]",
  ];
}

/** An Anthropic Messages upstream answering with a tool_use block. */
function anthropicAnswer(): Record<string, unknown> {
  return {
    id: UPSTREAM_MESSAGE_ID,
    type: "message",
    role: "assistant",
    model: "upstream-model",
    content: [
      { type: "text", text: "Checking." },
      { type: "tool_use", id: UPSTREAM_TOOL_USE_ID, name: "get_weather", input: { city: "Paris" } },
    ],
    stop_reason: "tool_use",
    stop_sequence: null,
    usage: { input_tokens: 9, output_tokens: 4, cache_creation_input_tokens: 2 },
    "amazon-bedrock-invocationMetrics": CHANNEL_METRICS,
  };
}

function anthropicStream(): string[] {
  const frame = (event: string, data: unknown) =>
    `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  return [
    frame("message_start", {
      type: "message_start",
      message: {
        id: UPSTREAM_MESSAGE_ID,
        type: "message",
        role: "assistant",
        model: "upstream-model",
        content: [],
        stop_reason: null,
        usage: { input_tokens: 9, output_tokens: 1, cache_creation_input_tokens: 2 },
      },
    }),
    frame("content_block_start", {
      type: "content_block_start",
      index: 0,
      content_block: { type: "tool_use", id: UPSTREAM_TOOL_USE_ID, name: "get_weather", input: {} },
    }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "input_json_delta", partial_json: WEATHER_ARGS },
    }),
    frame("content_block_stop", { type: "content_block_stop", index: 0 }),
    frame("message_delta", {
      type: "message_delta",
      delta: { stop_reason: "tool_use", stop_sequence: null },
      usage: { output_tokens: 4 },
    }),
    frame("message_stop", {
      type: "message_stop",
      "amazon-bedrock-invocationMetrics": CHANNEL_METRICS,
    }),
  ];
}

/** A web search the upstream ran itself, and the answer it built from it. */
const SERVER_TOOL_BLOCKS = [
  {
    type: "server_tool_use",
    id: UPSTREAM_SERVER_TOOL_ID,
    name: "web_search",
    input: { query: "weather in Paris" },
  },
  {
    type: "web_search_tool_result",
    tool_use_id: UPSTREAM_SERVER_TOOL_ID,
    content: [
      {
        type: "web_search_result",
        url: "https://weather.example/paris",
        title: "Paris weather",
        encrypted_content: "EqgfCioIARgBIiQ",
        page_age: null,
      },
    ],
  },
  { type: "text", text: "Sunny in Paris." },
];

function serverToolAnswer(): Record<string, unknown> {
  return {
    id: "msg_vrtx_01M",
    type: "message",
    role: "assistant",
    model: "upstream-model",
    content: SERVER_TOOL_BLOCKS,
    stop_reason: "end_turn",
    stop_sequence: null,
    usage: { input_tokens: 9, output_tokens: 4, server_tool_use: { web_search_requests: 1 } },
  };
}

function serverToolStream(): string[] {
  const frame = (event: string, data: unknown) =>
    `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  const [use, result, text] = SERVER_TOOL_BLOCKS;
  return [
    frame("message_start", {
      type: "message_start",
      message: {
        id: "msg_vrtx_01M",
        type: "message",
        role: "assistant",
        model: "upstream-model",
        content: [],
        stop_reason: null,
        usage: { input_tokens: 9, output_tokens: 1 },
      },
    }),
    frame("content_block_start", { type: "content_block_start", index: 0, content_block: { ...use, input: {} } }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "input_json_delta", partial_json: JSON.stringify(use!.input) },
    }),
    frame("content_block_stop", { type: "content_block_stop", index: 0 }),
    frame("content_block_start", { type: "content_block_start", index: 1, content_block: result }),
    frame("content_block_stop", { type: "content_block_stop", index: 1 }),
    frame("content_block_start", { type: "content_block_start", index: 2, content_block: { type: "text", text: "" } }),
    frame("content_block_delta", {
      type: "content_block_delta",
      index: 2,
      delta: { type: "text_delta", text: (text as { text: string }).text },
    }),
    frame("content_block_stop", { type: "content_block_stop", index: 2 }),
    frame("message_delta", {
      type: "message_delta",
      delta: { stop_reason: "end_turn", stop_sequence: null },
      usage: { output_tokens: 4 },
    }),
    frame("message_stop", { type: "message_stop" }),
  ];
}

const FUNCTION_CALL_ITEM = {
  type: "function_call",
  id: UPSTREAM_FC_ITEM_ID,
  call_id: UPSTREAM_CALL_ID,
  name: "get_weather",
  arguments: WEATHER_ARGS,
  status: "completed",
};

/** A usage object carrying a counter of the upstream's own beside OpenAI's. */
const RESPONSES_USAGE = {
  input_tokens: 9,
  input_tokens_details: { cached_tokens: 0, cache_creation_tokens: 2 },
  output_tokens: 4,
  total_tokens: 13,
};

/** A Responses upstream answering with a function call. */
function responsesAnswer(): Record<string, unknown> {
  return {
    id: "resp_up01",
    object: "response",
    created_at: 1_760_000_000,
    status: "completed",
    model: "upstream-model",
    output: [FUNCTION_CALL_ITEM],
    usage: RESPONSES_USAGE,
    "amazon-bedrock-invocationMetrics": CHANNEL_METRICS,
  };
}

function responsesStream(): string[] {
  const frame = (event: string, data: Record<string, unknown>) =>
    `event: ${event}\ndata: ${JSON.stringify({ type: event, ...data })}\n\n`;
  const snapshot = (status: string, output: unknown[]) => ({
    id: "resp_up01",
    object: "response",
    created_at: 1_760_000_000,
    status,
    model: "upstream-model",
    output,
  });
  return [
    frame("response.created", { response: snapshot("in_progress", []) }),
    frame("response.output_item.added", {
      output_index: 0,
      item: { ...FUNCTION_CALL_ITEM, arguments: "", status: "in_progress" },
    }),
    frame("response.function_call_arguments.delta", {
      item_id: UPSTREAM_FC_ITEM_ID,
      output_index: 0,
      delta: WEATHER_ARGS,
    }),
    frame("response.function_call_arguments.done", {
      item_id: UPSTREAM_FC_ITEM_ID,
      output_index: 0,
      arguments: WEATHER_ARGS,
    }),
    frame("response.output_item.done", { output_index: 0, item: FUNCTION_CALL_ITEM }),
    frame("response.completed", {
      response: {
        ...snapshot("completed", [FUNCTION_CALL_ITEM]),
        usage: RESPONSES_USAGE,
      },
      "amazon-bedrock-invocationMetrics": CHANNEL_METRICS,
    }),
  ];
}

// ── Caller requests ─────────────────────────────────────────────────────

const OPENAI_TOOLS = [
  {
    type: "function",
    function: {
      name: "get_weather",
      parameters: { type: "object", properties: { city: { type: "string" } } },
    },
  },
];
const ANTHROPIC_TOOLS = [
  {
    name: "get_weather",
    input_schema: { type: "object", properties: { city: { type: "string" } } },
  },
];
const RESPONSES_TOOLS = [
  {
    type: "function",
    name: "get_weather",
    parameters: { type: "object", properties: { city: { type: "string" } } },
  },
];

function chatTurn(model: string, toolCallId?: string, stream = false): Record<string, unknown> {
  const messages: unknown[] = [{ role: "user", content: "weather in Paris?" }];
  if (toolCallId) {
    messages.push(
      {
        role: "assistant",
        content: null,
        tool_calls: [
          {
            id: toolCallId,
            type: "function",
            function: { name: "get_weather", arguments: WEATHER_ARGS },
          },
        ],
      },
      { role: "tool", tool_call_id: toolCallId, content: "sunny" },
    );
  }
  return { model, messages, tools: OPENAI_TOOLS, ...(stream ? { stream: true } : {}) };
}

function messagesTurn(model: string, toolUseId?: string, stream = false): Record<string, unknown> {
  const messages: unknown[] = [{ role: "user", content: "weather in Paris?" }];
  if (toolUseId) {
    messages.push(
      {
        role: "assistant",
        content: [{ type: "tool_use", id: toolUseId, name: "get_weather", input: { city: "Paris" } }],
      },
      { role: "user", content: [{ type: "tool_result", tool_use_id: toolUseId, content: "sunny" }] },
    );
  }
  return {
    model,
    max_tokens: 64,
    messages,
    tools: ANTHROPIC_TOOLS,
    ...(stream ? { stream: true } : {}),
  };
}

async function post(path: string, body: unknown): Promise<{ status: number; text: string }> {
  const res = await fetch(`${app!.proxyUrl}${path}`, {
    method: "POST",
    headers: HEADERS,
    body: JSON.stringify(body),
  });
  return { status: res.status, text: await res.text() };
}

let app: SpawnedApp | undefined;

/** The tool-call id a Chat Completions answer hands out. */
function chatToolCallId(text: string): string {
  const body = JSON.parse(text);
  return body.choices[0].message.tool_calls[0].id as string;
}

/** The tool_use id a Messages answer hands out. */
function messagesToolUseId(text: string): string {
  const body = JSON.parse(text);
  const block = (body.content as Array<Record<string, any>>).find((b) => b.type === "tool_use");
  return block!.id as string;
}

/** The function_call item a Responses answer hands out. */
function responsesFunctionCall(output: Array<Record<string, any>>): Record<string, any> {
  const item = output.find((i) => i.type === "function_call");
  expect(item, "function_call item").toBeDefined();
  return item!;
}

/** A Chat Completions stream: one id on every frame, and the tool-call id it hands out. */
function chatStreamIds(text: string): { frameIds: string[]; toolCallId: string } {
  const events = sseEvents(text);
  const frameIds = events.map((e) => e.id as string);
  const toolCallId = events
    .flatMap((e) => (e.choices as Array<Record<string, any>>) ?? [])
    .flatMap((c) => (c.delta?.tool_calls as Array<Record<string, any>>) ?? [])
    .find((tc) => tc.id)?.id as string;
  return { frameIds, toolCallId };
}

/** A Messages stream: the message id of `message_start` and the tool_use id it hands out. */
function messagesStreamIds(text: string): { messageId: string; toolUseId: string } {
  const events = sseEvents(text);
  const start = events.find((e) => e.type === "message_start");
  const block = events.find(
    (e) => e.type === "content_block_start" && e.content_block?.type === "tool_use",
  );
  return { messageId: start?.message?.id, toolUseId: block?.content_block?.id };
}

describe("platform mode e2e: ids name no upstream channel, and tool use still round-trips", () => {
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  const up: Record<keyof typeof ALIASES, OpenAiUpstream | undefined> = {
    chatOpenAi: undefined,
    chatAnthropic: undefined,
    messagesNative: undefined,
    messagesTranslated: undefined,
    responsesNative: undefined,
    responsesTranslated: undefined,
    responsesChat: undefined,
    messagesServerTool: undefined,
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Each upstream answers its requests in order: the first turn, the
    // second turn (same tool call again), then a streamed turn.
    const chatScript = () => [
      { nonStreamBody: openAiChatAnswer() },
      { nonStreamBody: openAiChatAnswer() },
      { streamEvents: openAiChatStream() },
    ];
    const anthropicScript = () => [
      { nonStreamBody: anthropicAnswer() },
      { nonStreamBody: anthropicAnswer() },
      { rawStreamFrames: anthropicStream() },
    ];
    up.chatOpenAi = await startOpenAiUpstream({ scriptedResponses: chatScript() });
    up.chatAnthropic = await startOpenAiUpstream({ scriptedResponses: anthropicScript() });
    up.messagesNative = await startOpenAiUpstream({ scriptedResponses: anthropicScript() });
    up.messagesTranslated = await startOpenAiUpstream({ scriptedResponses: chatScript() });
    up.responsesNative = await startOpenAiUpstream({
      scriptedResponses: [
        { nonStreamBody: responsesAnswer() },
        { nonStreamBody: responsesAnswer() },
        { nonStreamBody: responsesAnswer() },
        { rawStreamFrames: responsesStream() },
      ],
    });
    up.responsesTranslated = await startOpenAiUpstream({ scriptedResponses: anthropicScript() });
    up.responsesChat = await startOpenAiUpstream({
      scriptedResponses: [
        { nonStreamBody: openAiChatAnswer(UPSTREAM_CONVERSE_TOOL_ID) },
        { nonStreamBody: openAiChatAnswer(UPSTREAM_CONVERSE_TOOL_ID) },
        { streamEvents: openAiChatStream(UPSTREAM_CONVERSE_TOOL_ID) },
      ],
    });
    up.messagesServerTool = await startOpenAiUpstream({
      scriptedResponses: [
        { nonStreamBody: serverToolAnswer() },
        { nonStreamBody: serverToolAnswer() },
        { rawStreamFrames: serverToolStream() },
      ],
    });

    app = await spawnApp({
      // The secret reaches the gateway process and nothing else.
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    seed = new SeedClient(etcd, app.etcdPrefix);

    const anthropic = { provider: "anthropic", adapter: "anthropic" };
    const openai = { provider: "openai", adapter: "openai" };
    // An OpenAI-compatible endpoint that declares no `/v1/responses`, so
    // Responses traffic is translated onto its Chat Completions API.
    const chatOnly = { ...openai, apis: {} };
    const routes: Array<
      [keyof typeof ALIASES, { provider: string; adapter: string; apis?: Record<string, never> }]
    > = [
      ["chatOpenAi", openai],
      ["chatAnthropic", anthropic],
      ["messagesNative", anthropic],
      ["messagesTranslated", openai],
      ["responsesNative", openai],
      ["responsesTranslated", anthropic],
      ["responsesChat", chatOnly],
      ["messagesServerTool", anthropic],
    ];
    for (const [route, kind] of routes) {
      const pk = await seed.createProviderKey({
        display_name: `pk-${ALIASES[route]}`,
        secret: "sk-mock",
        api_base: `${up[route]!.baseUrl}/v1`,
        provider: kind.provider,
        adapter: kind.adapter,
        ...(kind.apis ? { apis: kind.apis } : {}),
      });
      await seed.createModel({
        display_name: ALIASES[route],
        provider: kind.provider,
        model_name: "upstream-model",
        provider_key_id: pk.id,
      });
    }
    // The caller key last: its authenticating implies every model above
    // is in the gateway's snapshot.
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    const probe = new ProxyClient(app.proxyUrl, CALLER_PLAINTEXT);
    await waitConfigPropagation(async () => {
      const res = await probe.listModels();
      if (res.status !== 200) return false;
      const data = (res.body as { data?: Array<{ id?: string }> }).data ?? [];
      return Object.values(ALIASES).every((alias) => data.some((m) => m.id === alias));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(Object.values(up).map((u) => u?.close()));
  });

  for (const [route, label] of [
    ["chatOpenAi", "a Chat Completions upstream"],
    ["chatAnthropic", "an Anthropic Messages upstream"],
  ] as const) {
    test(`/v1/chat/completions from ${label}: no upstream id, one id per stream, tool calls round-trip`, async (ctx) => {
      if (!etcdReachable || !app) return void ctx.skip();
      const upstream = up[route]!;
      const alias = ALIASES[route];

      const first = await post("/v1/chat/completions", chatTurn(alias));
      expect(first.status, first.text).toBe(200);
      expectNoUpstreamIdentity(first.text);
      const toolCallId = chatToolCallId(first.text);
      expect(toolCallId).not.toBe(UPSTREAM_TOOL_USE_ID);
      expect(JSON.parse(first.text).id).not.toBe(UPSTREAM_MESSAGE_ID);

      // The agent's next turn carries the id it was handed.
      const second = await post("/v1/chat/completions", chatTurn(alias, toolCallId));
      expect(second.status, second.text).toBe(200);
      expectNoUpstreamIdentity(second.text);
      const forwarded = upstreamBody(upstream, 1);
      const forwardedText = JSON.stringify(forwarded);
      expect(forwardedText).toContain(toolCallId);
      expect(forwardedText).not.toContain(UPSTREAM_TOOL_USE_ID);
      if (route === "chatOpenAi") {
        expect(forwarded.messages[1].tool_calls[0].id).toBe(toolCallId);
        expect(forwarded.messages[2].tool_call_id).toBe(toolCallId);
      } else {
        const blocks = (forwarded.messages as Array<Record<string, any>>).flatMap((m) =>
          Array.isArray(m.content) ? m.content : [],
        );
        expect(blocks.find((b) => b.type === "tool_use")?.id).toBe(toolCallId);
        expect(blocks.find((b) => b.type === "tool_result")?.tool_use_id).toBe(toolCallId);
      }
      // The upstream named the same tool call again: so does the gateway.
      expect(chatToolCallId(second.text)).toBe(toolCallId);

      const streamed = await post("/v1/chat/completions", chatTurn(alias, undefined, true));
      expect(streamed.status, streamed.text).toBe(200);
      expectNoUpstreamIdentity(streamed.text);
      const { frameIds, toolCallId: streamedToolCallId } = chatStreamIds(streamed.text);
      expect(frameIds.length).toBeGreaterThan(1);
      expect(new Set(frameIds).size, `one id across ${frameIds.join(",")}`).toBe(1);
      expect(frameIds[0]).not.toBe(JSON.parse(first.text).id);
      expect(streamedToolCallId).toBe(toolCallId);
    });
  }

  for (const [route, label] of [
    ["messagesNative", "an Anthropic upstream relayed as written"],
    ["messagesTranslated", "a Chat Completions upstream"],
  ] as const) {
    test(`/v1/messages from ${label}: no upstream id, tool_use round-trips`, async (ctx) => {
      if (!etcdReachable || !app) return void ctx.skip();
      const upstream = up[route]!;
      const alias = ALIASES[route];

      const first = await post("/v1/messages", messagesTurn(alias));
      expect(first.status, first.text).toBe(200);
      expectNoUpstreamIdentity(first.text);
      const firstBody = JSON.parse(first.text);
      expect(firstBody.id).toMatch(/^msg_/);
      const toolUseId = messagesToolUseId(first.text);
      expect(toolUseId).toMatch(/^toolu_/);
      // Every other part of the answer survives.
      expect(firstBody.stop_reason).toBe("tool_use");
      expect(firstBody.content.find((b: any) => b.type === "tool_use").input).toEqual({
        city: "Paris",
      });

      const second = await post("/v1/messages", messagesTurn(alias, toolUseId));
      expect(second.status, second.text).toBe(200);
      expectNoUpstreamIdentity(second.text);
      const forwarded = upstreamBody(upstream, 1);
      const forwardedText = JSON.stringify(forwarded);
      expect(forwardedText).toContain(toolUseId);
      expect(forwardedText).not.toContain(UPSTREAM_TOOL_USE_ID);
      if (route === "messagesNative") {
        expect(forwarded.messages[1].content[0].id).toBe(toolUseId);
        expect(forwarded.messages[2].content[0].tool_use_id).toBe(toolUseId);
      } else {
        const assistant = (forwarded.messages as Array<Record<string, any>>).find(
          (m) => m.role === "assistant",
        );
        const tool = (forwarded.messages as Array<Record<string, any>>).find((m) => m.role === "tool");
        expect(assistant?.tool_calls?.[0]?.id).toBe(toolUseId);
        expect(tool?.tool_call_id).toBe(toolUseId);
      }
      expect(messagesToolUseId(second.text)).toBe(toolUseId);
      // Two answers, two message ids.
      expect(JSON.parse(second.text).id).not.toBe(firstBody.id);

      const streamed = await post("/v1/messages", messagesTurn(alias, undefined, true));
      expect(streamed.status, streamed.text).toBe(200);
      expectNoUpstreamIdentity(streamed.text);
      const ids = messagesStreamIds(streamed.text);
      expect(ids.messageId).toMatch(/^msg_/);
      expect(ids.toolUseId).toBe(toolUseId);
      expect(streamed.text).toContain("message_stop");
    });
  }

  test("/v1/responses relayed as written: item ids leave sealed and the upstream gets its own back", async (ctx) => {
    if (!etcdReachable || !app) return void ctx.skip();
    const upstream = up.responsesNative!;
    const alias = ALIASES.responsesNative;
    const turn = (input: unknown, extra: Record<string, unknown> = {}) => ({
      model: alias,
      input,
      tools: RESPONSES_TOOLS,
      ...extra,
    });

    const first = await post("/v1/responses", turn("weather in Paris?"));
    expect(first.status, first.text).toBe(200);
    expectNoUpstreamIdentity(first.text);
    const firstBody = JSON.parse(first.text);
    const call = responsesFunctionCall(firstBody.output);
    // Each id keeps the kind its prefix names, and nothing else.
    expect(call.id).toMatch(/^fc_/);
    expect(call.call_id).toMatch(/^call_/);
    expect(call.id).not.toBe(UPSTREAM_FC_ITEM_ID);
    expect(call.call_id).not.toBe(UPSTREAM_CALL_ID);
    expect(call.arguments).toBe(WEATHER_ARGS);
    // A relayed answer comes back in full: the upstream's own usage counters
    // included, buffered and streamed alike (asserted on the stream below).
    expect(firstBody.usage).toEqual(RESPONSES_USAGE);

    // A stateless continuation: the whole history, echoed.
    const stateless = await post(
      "/v1/responses",
      turn([
        { role: "user", content: "weather in Paris?" },
        { type: "function_call", id: call.id, call_id: call.call_id, name: "get_weather", arguments: WEATHER_ARGS },
        { type: "function_call_output", call_id: call.call_id, output: "sunny" },
      ]),
    );
    expect(stateless.status, stateless.text).toBe(200);
    expectNoUpstreamIdentity(stateless.text);
    const echoed = upstreamBody(upstream, 1);
    expect(echoed.input[1].id).toBe(UPSTREAM_FC_ITEM_ID);
    expect(echoed.input[1].call_id).toBe(UPSTREAM_CALL_ID);
    expect(echoed.input[2].call_id).toBe(UPSTREAM_CALL_ID);
    // The same upstream call comes back as the same sealed ids.
    const again = responsesFunctionCall(JSON.parse(stateless.text).output);
    expect(again.id).toBe(call.id);
    expect(again.call_id).toBe(call.call_id);

    // A stored continuation: only the call's output, against the response
    // the upstream kept.
    const stored = await post(
      "/v1/responses",
      turn([{ type: "function_call_output", call_id: call.call_id, output: "sunny" }], {
        previous_response_id: firstBody.id,
      }),
    );
    expect(stored.status, stored.text).toBe(200);
    expect(upstreamBody(upstream, 2).input[0].call_id).toBe(UPSTREAM_CALL_ID);

    const streamed = await post("/v1/responses", turn("weather in Paris?", { stream: true }));
    expect(streamed.status, streamed.text).toBe(200);
    expectNoUpstreamIdentity(streamed.text);
    const events = sseEvents(streamed.text);
    const added = events.find((e) => e.type === "response.output_item.added");
    expect(added?.item?.id).toBe(call.id);
    expect(added?.item?.call_id).toBe(call.call_id);
    const perItem = events.filter((e) => typeof e.item_id === "string");
    expect(perItem.length).toBeGreaterThan(0);
    for (const e of perItem) expect(e.item_id, e.type).toBe(call.id);
    const completed = events.find((e) => e.type === "response.completed");
    expect(responsesFunctionCall(completed?.response?.output ?? []).call_id).toBe(call.call_id);
    expect(completed?.response?.usage).toEqual(RESPONSES_USAGE);
  });

  test("/v1/responses from an Anthropic upstream: call ids leave sealed and the upstream gets its own back", async (ctx) => {
    if (!etcdReachable || !app) return void ctx.skip();
    const upstream = up.responsesTranslated!;
    const alias = ALIASES.responsesTranslated;
    const turn = (input: unknown, extra: Record<string, unknown> = {}) => ({
      model: alias,
      input,
      tools: RESPONSES_TOOLS,
      ...extra,
    });

    const first = await post("/v1/responses", turn("weather in Paris?"));
    expect(first.status, first.text).toBe(200);
    expectNoUpstreamIdentity(first.text);
    const firstBody = JSON.parse(first.text);
    const call = responsesFunctionCall(firstBody.output);
    // A call id names no vendor: not the upstream's `toolu_`.
    expect(call.call_id).toMatch(/^call_/);
    expect(call.call_id).not.toBe(UPSTREAM_TOOL_USE_ID);
    // The answer the gateway built keeps to the Responses usage fields; the
    // upstream's cache write is not named beside them.
    expect(firstBody.usage?.input_tokens_details).toBeDefined();
    expect(firstBody.usage.input_tokens_details).not.toHaveProperty("cache_creation_tokens");

    const second = await post(
      "/v1/responses",
      turn([
        { role: "user", content: "weather in Paris?" },
        { type: "function_call", id: call.id, call_id: call.call_id, name: "get_weather", arguments: WEATHER_ARGS },
        { type: "function_call_output", call_id: call.call_id, output: "sunny" },
      ]),
    );
    expect(second.status, second.text).toBe(200);
    expectNoUpstreamIdentity(second.text);
    const blocks = (upstreamBody(upstream, 1).messages as Array<Record<string, any>>).flatMap((m) =>
      Array.isArray(m.content) ? m.content : [],
    );
    expect(blocks.find((b) => b.type === "tool_use")?.id).toBe(UPSTREAM_TOOL_USE_ID);
    expect(blocks.find((b) => b.type === "tool_result")?.tool_use_id).toBe(UPSTREAM_TOOL_USE_ID);
    expect(responsesFunctionCall(JSON.parse(second.text).output).call_id).toBe(call.call_id);

    const streamed = await post("/v1/responses", turn("weather in Paris?", { stream: true }));
    expect(streamed.status, streamed.text).toBe(200);
    expectNoUpstreamIdentity(streamed.text);
    const events = sseEvents(streamed.text);
    const added = events.find(
      (e) => e.type === "response.output_item.added" && e.item?.type === "function_call",
    );
    expect(added?.item?.call_id).toBe(call.call_id);
    for (const e of events.filter((e) => e.item_id && e.type.startsWith("response.function_call"))) {
      expect(e.item_id, e.type).toBe(added?.item?.id);
    }
    const completed = events.find((e) => e.type === "response.completed");
    expect(completed?.response?.usage?.input_tokens_details).toBeDefined();
    expect(completed!.response.usage.input_tokens_details).not.toHaveProperty("cache_creation_tokens");
  });

  // A server tool's use id carries the channel's marker like every other id
  // the upstream hands out. Mapped, it keeps the `srvtoolu_` the Messages
  // API requires of it, the result that names it still pairs, and the
  // caller can send both back as history.
  test("/v1/messages with a server tool: its use id and the result naming it keep srvtoolu_ and lose the channel", async (ctx) => {
    if (!etcdReachable || !app) return void ctx.skip();
    const upstream = up.messagesServerTool!;
    const alias = ALIASES.messagesServerTool;
    const turn = (messages: unknown[], stream = false) => ({
      model: alias,
      max_tokens: 64,
      messages,
      tools: [{ type: "web_search_20250305", name: "web_search", max_uses: 1 }],
      ...(stream ? { stream: true } : {}),
    });
    const question = { role: "user", content: "weather in Paris?" };
    const SERVER_ID = /^srvtoolu_[a-zA-Z0-9_]+$/;

    const first = await post("/v1/messages", turn([question]));
    expect(first.status, first.text).toBe(200);
    expect(first.text).not.toContain("vrtx");
    const content = JSON.parse(first.text).content as Array<Record<string, any>>;
    const use = content.find((b) => b.type === "server_tool_use");
    const result = content.find((b) => b.type === "web_search_tool_result");
    expect(use?.id).toMatch(SERVER_ID);
    expect(use?.id).not.toBe(UPSTREAM_SERVER_TOOL_ID);
    expect(result?.tool_use_id).toBe(use?.id);
    // The rest of the answer survives.
    expect(result?.content?.[0]?.encrypted_content).toBe("EqgfCioIARgBIiQ");
    expect(content.find((b) => b.type === "text")?.text).toBe("Sunny in Paris.");

    // The next turn carries the answer as history; the upstream receives
    // the ids the caller was handed, still paired.
    const second = await post(
      "/v1/messages",
      turn([question, { role: "assistant", content }, { role: "user", content: "and tomorrow?" }]),
    );
    expect(second.status, second.text).toBe(200);
    expect(second.text).not.toContain("vrtx");
    const history = (upstreamBody(upstream, 1).messages as Array<Record<string, any>>)[1].content as Array<
      Record<string, any>
    >;
    expect(history.find((b) => b.type === "server_tool_use")?.id).toBe(use?.id);
    expect(history.find((b) => b.type === "web_search_tool_result")?.tool_use_id).toBe(use?.id);

    const streamed = await post("/v1/messages", turn([question], true));
    expect(streamed.status, streamed.text).toBe(200);
    expect(streamed.text).not.toContain("vrtx");
    const blocks = sseEvents(streamed.text)
      .filter((e) => e.type === "content_block_start")
      .map((e) => e.content_block as Record<string, any>);
    expect(blocks.find((b) => b.type === "server_tool_use")?.id).toBe(use?.id);
    expect(blocks.find((b) => b.type === "web_search_tool_result")?.tool_use_id).toBe(use?.id);
    expect(streamed.text).toContain("message_stop");
  });

  // A translated answer's call id is the tool-call id the Chat Completions
  // upstream handed out, and a Converse channel's start `tooluse_`: the
  // caller must see a call id in the Responses API's own shape, and the
  // upstream must get its own id back on the next turn.
  test("/v1/responses from a Chat Completions upstream: a Converse-shaped call id leaves as call_ and returns as it was", async (ctx) => {
    if (!etcdReachable || !app) return void ctx.skip();
    const upstream = up.responsesChat!;
    const alias = ALIASES.responsesChat;
    const turn = (input: unknown, extra: Record<string, unknown> = {}) => ({
      model: alias,
      input,
      tools: RESPONSES_TOOLS,
      ...extra,
    });
    const expectNoConverseId = (text: string) => {
      expectNoUpstreamIdentity(text);
      expect(text).not.toContain(UPSTREAM_CONVERSE_TOOL_ID);
      expect(text).not.toContain("tooluse");
    };

    const first = await post("/v1/responses", turn("weather in Paris?"));
    expect(first.status, first.text).toBe(200);
    expectNoConverseId(first.text);
    const call = responsesFunctionCall(JSON.parse(first.text).output);
    expect(call.call_id).toMatch(/^call_/);
    expect(call.arguments).toBe(WEATHER_ARGS);

    // The agent's next turn: the call it was handed, and the call's output.
    const second = await post(
      "/v1/responses",
      turn([
        { role: "user", content: "weather in Paris?" },
        { type: "function_call", id: call.id, call_id: call.call_id, name: "get_weather", arguments: WEATHER_ARGS },
        { type: "function_call_output", call_id: call.call_id, output: "sunny" },
      ]),
    );
    expect(second.status, second.text).toBe(200);
    expectNoConverseId(second.text);
    const forwarded = upstreamBody(upstream, 1).messages as Array<Record<string, any>>;
    const assistant = forwarded.find((m) => m.role === "assistant");
    const tool = forwarded.find((m) => m.role === "tool");
    expect(assistant?.tool_calls?.[0]?.id).toBe(UPSTREAM_CONVERSE_TOOL_ID);
    expect(tool?.tool_call_id).toBe(UPSTREAM_CONVERSE_TOOL_ID);
    // The same upstream call comes back as the same call id.
    expect(responsesFunctionCall(JSON.parse(second.text).output).call_id).toBe(call.call_id);

    const streamed = await post("/v1/responses", turn("weather in Paris?", { stream: true }));
    expect(streamed.status, streamed.text).toBe(200);
    expectNoConverseId(streamed.text);
    const events = sseEvents(streamed.text);
    const added = events.find(
      (e) => e.type === "response.output_item.added" && e.item?.type === "function_call",
    );
    expect(added?.item?.call_id).toBe(call.call_id);
    const completed = events.find((e) => e.type === "response.completed");
    expect(responsesFunctionCall(completed?.response?.output ?? []).call_id).toBe(call.call_id);
  });
});

// ── A response id belongs to the tenant it was handed to ────────────────
//
// A Responses answer's id is what a client continues from
// (`previous_response_id`). In platform mode one gateway, and one upstream
// account, serves many tenants, so the id must work as a continuation only
// for the tenant that received it. Anyone else holding it gets the same "no
// such response" answer an id nobody was ever handed gets, and nothing
// reaches an upstream on their behalf. The id names neither the upstream's
// own id nor the target that served it, read as is or decoded.
//
// A key restricted to provider groups continues only through a target its
// groups still contain: narrowing the groups turns the continuation into the
// same "no such response", rather than sending it to some other upstream.
//
// The ids of the items inside an answer belong to its tenant too. The
// upstream account is shared, so an item or call id echoed back is resolved
// against everything that account stored: echoed by another tenant, it must
// reach the upstream as the opaque string that tenant sent, which names
// nothing the upstream has.
//
// The same holds for every other object the shared account stores: a
// container, a file, a vector store, a conversation, a stored prompt. A
// tenant is never handed the ids of such objects through the gateway, so a
// request naming one names somebody else's, and it is refused as an invalid
// request naming the field, before any upstream sees it.

const TENANT_KEYS = {
  /** Tenant A. */
  a: "sk-platform-tenant-e2e-a",
  /** Tenant B: another team on the same gateway and upstream. */
  b: "sk-platform-tenant-e2e-b",
  /** Tenant A again, restricted to provider groups. */
  aGrouped: "sk-platform-tenant-e2e-a-grouped",
};

const TENANT_ALIASES = {
  target: "pi-tenant-target",
  streamed: "pi-tenant-streamed",
  grouped: "pi-tenant-grouped",
  otherGroup: "pi-tenant-other-group",
  probe: "pi-tenant-probe",
  items: "pi-tenant-items",
  refs: "pi-tenant-refs",
};

/** A Messages target for the stored-object refusals. */
const REFS_ANTHROPIC_ALIAS = "pi-tenant-refs-anthropic";

/** A Responses upstream's answer, under an id that carries a channel marker. */
function tenantAnswer(id: string, previous: string | null = null): Record<string, unknown> {
  return {
    id,
    object: "response",
    created_at: 1_760_000_000,
    status: "completed",
    model: "upstream-model",
    previous_response_id: previous,
    output: [
      {
        type: "message",
        id: `msg_bdrk_${id.slice(-3)}`,
        role: "assistant",
        status: "completed",
        content: [{ type: "output_text", text: "Sunny.", annotations: [] }],
      },
    ],
    usage: { input_tokens: 5, output_tokens: 2, total_tokens: 7 },
  };
}

function tenantStream(id: string): string[] {
  const frame = (event: string, data: Record<string, unknown>) =>
    `event: ${event}\ndata: ${JSON.stringify({ type: event, ...data })}\n\n`;
  const answer = tenantAnswer(id);
  const inProgress = { ...answer, status: "in_progress", output: [] };
  const message = (answer.output as Array<Record<string, unknown>>)[0]!;
  return [
    frame("response.created", { sequence_number: 0, response: inProgress }),
    frame("response.in_progress", { sequence_number: 1, response: inProgress }),
    frame("response.output_item.added", {
      sequence_number: 2,
      output_index: 0,
      item: { ...message, status: "in_progress", content: [] },
    }),
    frame("response.output_text.delta", {
      sequence_number: 3,
      item_id: message.id,
      output_index: 0,
      content_index: 0,
      delta: "Sunny.",
    }),
    frame("response.output_item.done", { sequence_number: 4, output_index: 0, item: message }),
    frame("response.completed", { sequence_number: 5, response: answer }),
  ];
}

/** An answer whose items name the upstream's own message, item and call ids. */
function itemsAnswer(): Record<string, unknown> {
  const answer = tenantAnswer("resp_bdrk_i01");
  return {
    ...answer,
    output: [
      ...(answer.output as unknown[]),
      {
        type: "function_call",
        id: "fc_bdrk_i01",
        call_id: "call_bdrk_i01",
        name: "get_weather",
        arguments: WEATHER_ARGS,
        status: "completed",
      },
    ],
  };
}

describe("platform mode e2e: a response id continues only for the tenant it was handed to", () => {
  let tenantApp: SpawnedApp | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  let groupedKeyId = "";
  const modelIds: Record<keyof typeof TENANT_ALIASES, string> = {
    target: "",
    streamed: "",
    grouped: "",
    otherGroup: "",
    probe: "",
    items: "",
    refs: "",
  };
  const up: Record<keyof typeof TENANT_ALIASES, OpenAiUpstream | undefined> = {
    target: undefined,
    streamed: undefined,
    grouped: undefined,
    otherGroup: undefined,
    probe: undefined,
    items: undefined,
    refs: undefined,
  };
  let refsAnthropic: OpenAiUpstream | undefined;

  async function postAs(
    key: string,
    path: string,
    body: unknown,
  ): Promise<{ status: number; text: string }> {
    const res = await fetch(`${tenantApp!.proxyUrl}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  }

  /**
   * The id names nothing the tenant may not learn, as sent or decoded. The
   * whole upstream id is what is looked for: a short marker could turn up in
   * a random-looking id by chance.
   */
  function expectSealed(id: string, upstreamId: string, target: keyof typeof TENANT_ALIASES): void {
    expect(id).toMatch(/^resp_/);
    const decoded = Buffer.from(id.slice("resp_".length), "base64url").toString("latin1");
    for (const view of [id, decoded]) {
      for (const secret of [upstreamId, TENANT_ALIASES[target], modelIds[target]]) {
        expect(view, `${id} names ${secret}`).not.toContain(secret);
      }
    }
  }

  /** No upstream id of an answer reaches the caller. */
  function expectNoUpstreamIds(text: string, responseId: string): void {
    for (const upstreamId of [responseId, `msg_bdrk_${responseId.slice(-3)}`]) {
      expect(text, `answer carries ${upstreamId}`).not.toContain(upstreamId);
    }
  }

  /** What a caller is told for a response id nobody was ever handed. */
  async function unknownResponseAnswer(key: string, model: string): Promise<{ status: number; text: string }> {
    return postAs(key, "/v1/responses", {
      model,
      input: "and tomorrow?",
      previous_response_id: "resp_0123456789abcdef0123456789abcdef",
    });
  }

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    up.target = await startOpenAiUpstream({
      scriptedResponses: [
        { nonStreamBody: tenantAnswer("resp_bdrk_t01") },
        { nonStreamBody: tenantAnswer("resp_bdrk_t02", "resp_bdrk_t01") },
      ],
    });
    up.streamed = await startOpenAiUpstream({
      scriptedResponses: [
        { rawStreamFrames: tenantStream("resp_bdrk_s01") },
        { nonStreamBody: tenantAnswer("resp_bdrk_s02", "resp_bdrk_s01") },
      ],
    });
    up.grouped = await startOpenAiUpstream({ nonStreamBody: tenantAnswer("resp_bdrk_g01") });
    up.otherGroup = await startOpenAiUpstream({ nonStreamBody: tenantAnswer("resp_bdrk_o01") });
    up.probe = await startOpenAiUpstream();
    up.items = await startOpenAiUpstream({ nonStreamBody: itemsAnswer() });
    up.refs = await startOpenAiUpstream({
      pathBodies: { "/responses": tenantAnswer("resp_bdrk_r01") },
    });
    refsAnthropic = await startOpenAiUpstream({
      pathBodies: {
        "/messages": {
          id: "msg_bdrk_r01",
          type: "message",
          role: "assistant",
          model: "upstream-model",
          content: [{ type: "text", text: "Sunny." }],
          stop_reason: "end_turn",
          stop_sequence: null,
          usage: { input_tokens: 5, output_tokens: 2 },
        },
        "/count_tokens": { input_tokens: 5 },
      },
    });

    tenantApp = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    seed = new SeedClient(etcd, tenantApp.etcdPrefix);

    const groups: Record<keyof typeof TENANT_ALIASES, string[]> = {
      target: ["ga"],
      streamed: ["ga"],
      grouped: ["ga"],
      otherGroup: ["gb"],
      probe: ["ga"],
      items: ["ga"],
      refs: ["ga"],
    };
    for (const route of Object.keys(TENANT_ALIASES) as Array<keyof typeof TENANT_ALIASES>) {
      const pk = await seed.createProviderKey({
        display_name: `pk-${TENANT_ALIASES[route]}`,
        secret: "sk-mock",
        api_base: `${up[route]!.baseUrl}/v1`,
        groups: groups[route],
      });
      const model = await seed.createModel({
        display_name: TENANT_ALIASES[route],
        provider: "openai",
        model_name: "upstream-model",
        provider_key_id: pk.id,
        retries: 0,
      });
      modelIds[route] = model.id;
    }
    const refsPk = await seed.createProviderKey({
      display_name: `pk-${REFS_ANTHROPIC_ALIAS}`,
      secret: "sk-mock",
      api_base: `${refsAnthropic.baseUrl}/v1`,
      provider: "anthropic",
      adapter: "anthropic",
      groups: ["ga"],
    });
    await seed.createModel({
      display_name: REFS_ANTHROPIC_ALIAS,
      provider: "anthropic",
      model_name: "upstream-model",
      provider_key_id: refsPk.id,
      retries: 0,
    });
    // The caller keys last: each one authenticating implies every model
    // above is in the gateway's snapshot.
    const sha = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
    await seed.createApiKey({ key_hash: sha(TENANT_KEYS.a), allowed_models: ["*"], team_id: "tenant-a" });
    await seed.createApiKey({ key_hash: sha(TENANT_KEYS.b), allowed_models: ["*"], team_id: "tenant-b" });
    const grouped = await seed.createApiKey({
      key_hash: sha(TENANT_KEYS.aGrouped),
      allowed_models: ["*"],
      team_id: "tenant-a",
      allowed_provider_groups: ["ga", "gb"],
    });
    groupedKeyId = grouped.id;
    await waitConfigPropagation(async () => {
      for (const key of Object.values(TENANT_KEYS)) {
        const res = await new ProxyClient(tenantApp!.proxyUrl, key).listModels();
        if (res.status !== 200) return false;
        const data = (res.body as { data?: Array<{ id?: string }> }).data ?? [];
        const aliases = [...Object.values(TENANT_ALIASES), REFS_ANTHROPIC_ALIAS];
        if (!aliases.every((alias) => data.some((m) => m.id === alias))) {
          return false;
        }
      }
      return true;
    });
  });

  afterAll(async () => {
    await tenantApp?.exit();
    await Promise.all([...Object.values(up), refsAnthropic].map((u) => u?.close()));
  });

  test("a buffered answer's id: another tenant gets no such response, the owner continues", async (ctx) => {
    if (!etcdReachable || !tenantApp) return void ctx.skip();
    const upstream = up.target!;
    const model = TENANT_ALIASES.target;

    const first = await postAs(TENANT_KEYS.a, "/v1/responses", { model, input: "weather in Paris?" });
    expect(first.status, first.text).toBe(200);
    const firstBody = JSON.parse(first.text);
    const id = firstBody.id as string;
    expectSealed(id, "resp_bdrk_t01", "target");
    expectNoUpstreamIds(first.text, "resp_bdrk_t01");
    expect(firstBody.output[0].content[0].text).toBe("Sunny.");
    expect(upstream.receivedRequests).toHaveLength(1);

    // Tenant B holds tenant A's id: it is answered as an id nobody was handed.
    const foreign = await postAs(TENANT_KEYS.b, "/v1/responses", {
      model,
      input: "and tomorrow?",
      previous_response_id: id,
    });
    const unknown = await unknownResponseAnswer(TENANT_KEYS.b, model);
    expect(foreign.status, foreign.text).toBe(404);
    expect(unknown.status, unknown.text).toBe(404);
    expect(foreign.text).toBe(unknown.text);
    expect(foreign.text).not.toContain(id);
    expect(upstream.receivedRequests, "nothing reached the upstream for tenant B").toHaveLength(1);

    // Tenant A continues from its own id: the upstream gets its own id back,
    // and the answer echoes the id the client sent.
    const next = await postAs(TENANT_KEYS.a, "/v1/responses", {
      model,
      input: "and tomorrow?",
      previous_response_id: id,
    });
    expect(next.status, next.text).toBe(200);
    expect(upstreamBody(upstream, 1).previous_response_id).toBe("resp_bdrk_t01");
    const nextBody = JSON.parse(next.text);
    expect(nextBody.previous_response_id).toBe(id);
    expect(nextBody.id).not.toBe(id);
    expectSealed(nextBody.id, "resp_bdrk_t02", "target");
    expectNoUpstreamIds(next.text, "resp_bdrk_t02");
    expect(next.text).not.toContain("resp_bdrk_t01");
  });

  test("a streamed answer's id: one id on every frame, and it continues only for its tenant", async (ctx) => {
    if (!etcdReachable || !tenantApp) return void ctx.skip();
    const upstream = up.streamed!;
    const model = TENANT_ALIASES.streamed;

    const streamed = await postAs(TENANT_KEYS.a, "/v1/responses", {
      model,
      input: "weather in Paris?",
      stream: true,
    });
    expect(streamed.status, streamed.text).toBe(200);
    expectNoUpstreamIds(streamed.text, "resp_bdrk_s01");
    const ids = sseEvents(streamed.text)
      .filter((e) => typeof e.response?.id === "string")
      .map((e) => e.response.id as string);
    expect(ids.length, streamed.text).toBeGreaterThanOrEqual(3);
    expect(new Set(ids).size, `one id across ${ids.join(",")}`).toBe(1);
    const id = ids[0]!;
    expectSealed(id, "resp_bdrk_s01", "streamed");

    const foreign = await postAs(TENANT_KEYS.b, "/v1/responses", {
      model,
      input: "and tomorrow?",
      previous_response_id: id,
    });
    expect(foreign.status, foreign.text).toBe(404);
    expect(foreign.text).toBe((await unknownResponseAnswer(TENANT_KEYS.b, model)).text);
    expect(upstream.receivedRequests, "nothing reached the upstream for tenant B").toHaveLength(1);

    const next = await postAs(TENANT_KEYS.a, "/v1/responses", {
      model,
      input: "and tomorrow?",
      previous_response_id: id,
    });
    expect(next.status, next.text).toBe(200);
    expect(upstreamBody(upstream, 1).previous_response_id).toBe("resp_bdrk_s01");
    expect(JSON.parse(next.text).previous_response_id).toBe(id);
  });

  test("a grouped key whose groups no longer hold the target that answered gets no such response", async (ctx) => {
    if (!etcdReachable || !tenantApp) return void ctx.skip();

    const first = await postAs(TENANT_KEYS.aGrouped, "/v1/responses", {
      model: TENANT_ALIASES.grouped,
      input: "weather in Paris?",
    });
    expect(first.status, first.text).toBe(200);
    const id = JSON.parse(first.text).id as string;
    expectSealed(id, "resp_bdrk_g01", "grouped");

    // The operator narrows the key to the other group. Propagation shows on
    // a model of the dropped group, which the key may no longer use.
    await seed!.update("api_keys", groupedKeyId, {
      key_hash: createHash("sha256").update(TENANT_KEYS.aGrouped).digest("hex"),
      allowed_models: ["*"],
      team_id: "tenant-a",
      allowed_provider_groups: ["gb"],
    });
    await waitConfigPropagation(async () => {
      const probe = await new ProxyClient(tenantApp!.proxyUrl, TENANT_KEYS.aGrouped).chat({
        model: TENANT_ALIASES.probe,
        messages: [{ role: "user", content: "hi" }],
      });
      return probe.status !== 200;
    });

    const groupedBefore = up.grouped!.receivedRequests.length;
    const otherBefore = up.otherGroup!.receivedRequests.length;
    // A model the key can still use, continuing from the answer of one it
    // can no longer reach.
    const next = await postAs(TENANT_KEYS.aGrouped, "/v1/responses", {
      model: TENANT_ALIASES.otherGroup,
      input: "and tomorrow?",
      previous_response_id: id,
    });
    expect(next.status, next.text).toBe(404);
    expect(next.text).toBe(
      (await unknownResponseAnswer(TENANT_KEYS.aGrouped, TENANT_ALIASES.otherGroup)).text,
    );
    expect(up.grouped!.receivedRequests).toHaveLength(groupedBefore);
    expect(up.otherGroup!.receivedRequests).toHaveLength(otherBefore);

    // The model it can still use answers it without a continuation.
    const fresh = await postAs(TENANT_KEYS.aGrouped, "/v1/responses", {
      model: TENANT_ALIASES.otherGroup,
      input: "weather in Paris?",
    });
    expect(fresh.status, fresh.text).toBe(200);
    expect(up.otherGroup!.receivedRequests).toHaveLength(otherBefore + 1);
  });

  test("an item id echoed by another tenant reaches the upstream as the opaque string it sent", async (ctx) => {
    if (!etcdReachable || !tenantApp) return void ctx.skip();
    const upstream = up.items!;
    const model = TENANT_ALIASES.items;

    const first = await postAs(TENANT_KEYS.a, "/v1/responses", { model, input: "weather in Paris?" });
    expect(first.status, first.text).toBe(200);
    const output = JSON.parse(first.text).output as Array<Record<string, any>>;
    const message = output.find((i) => i.type === "message")!;
    const call = output.find((i) => i.type === "function_call")!;
    for (const [sealed, original] of [
      [message.id, "msg_bdrk_i01"],
      [call.id, "fc_bdrk_i01"],
      [call.call_id, "call_bdrk_i01"],
    ] as const) {
      expect(typeof sealed, JSON.stringify(output)).toBe("string");
      expect(sealed).not.toBe(original);
    }
    // The ids a later turn echoes: a reference to the stored message, and
    // the answer to the stored call.
    const echo = (key: string) =>
      postAs(key, "/v1/responses", {
        model,
        input: [
          { type: "item_reference", id: message.id },
          { type: "item_reference", id: call.id },
          { type: "function_call_output", call_id: call.call_id, output: "sunny" },
        ],
      });

    const foreign = await echo(TENANT_KEYS.b);
    expect(foreign.status, foreign.text).toBe(200);
    expect(upstream.receivedRequests).toHaveLength(2);
    const sentForB = upstreamBody(upstream, 1);
    expect(sentForB.input[0].id).toBe(message.id);
    expect(sentForB.input[1].id).toBe(call.id);
    expect(sentForB.input[2].call_id).toBe(call.call_id);
    for (const original of ["msg_bdrk_i01", "fc_bdrk_i01", "call_bdrk_i01"]) {
      expect(upstream.receivedRequests[1]!.body, `tenant B's request names ${original}`).not.toContain(
        original,
      );
    }

    // The tenant the ids were handed to gets the upstream's own ids back.
    const own = await echo(TENANT_KEYS.a);
    expect(own.status, own.text).toBe(200);
    const sentForA = upstreamBody(upstream, 2);
    expect(sentForA.input[0].id).toBe("msg_bdrk_i01");
    expect(sentForA.input[1].id).toBe("fc_bdrk_i01");
    expect(sentForA.input[2].call_id).toBe("call_bdrk_i01");
  });

  test("a request naming an object stored upstream is refused on every family, before any upstream", async (ctx) => {
    if (!etcdReachable || !tenantApp) return void ctx.skip();
    const openai = TENANT_ALIASES.refs;
    const anthropic = REFS_ANTHROPIC_ALIAS;
    const hello = { type: "input_text", text: "what is in it?" };
    const rows: Array<{ family: string; path: string; body: Record<string, unknown>; field: string }> = [
      {
        family: "responses: a container by id",
        path: "/v1/responses",
        body: {
          model: openai,
          input: "run it",
          tools: [{ type: "code_interpreter", container: "cntr_tenant_a_0001" }],
        },
        field: "tools[0].container",
      },
      {
        family: "responses: a conversation",
        path: "/v1/responses",
        body: { model: openai, input: "go on", conversation: "conv_tenant_a_0001" },
        field: "conversation",
      },
      {
        family: "responses: a stored file",
        path: "/v1/responses",
        body: {
          model: openai,
          input: [
            {
              role: "user",
              content: [hello, { type: "input_file", file_id: "file-tenant-a-0001" }],
            },
          ],
        },
        field: "input[0].content[1].file_id",
      },
      {
        family: "responses: a vector store",
        path: "/v1/responses",
        body: {
          model: openai,
          input: "search it",
          tools: [{ type: "file_search", vector_store_ids: ["vs_tenant_a_0001"] }],
        },
        field: "tools[0].vector_store_ids",
      },
      {
        family: "responses: a container by id, in a tool definition inside input",
        path: "/v1/responses",
        body: {
          model: openai,
          input: [
            {
              type: "additional_tools",
              role: "developer",
              tools: [{ type: "code_interpreter", container: "cntr_tenant_a_0001" }],
            },
            { role: "user", content: "run it" },
          ],
        },
        field: "input[0].tools[0].container",
      },
      {
        family: "responses: a vector store, in a tool search's output",
        path: "/v1/responses",
        body: {
          model: openai,
          input: [
            { role: "user", content: "search it" },
            {
              type: "tool_search_output",
              call_id: "call_search_1",
              execution: "client",
              status: "completed",
              tools: [{ type: "file_search", vector_store_ids: ["vs_tenant_a_0001"] }],
            },
          ],
        },
        field: "input[1].tools[0].vector_store_ids",
      },
      {
        family: "responses: an MCP tunnel",
        path: "/v1/responses",
        body: {
          model: openai,
          input: "list the files",
          tools: [{ type: "mcp", server_label: "private", tunnel_id: "tunnel_tenant_a_0001" }],
        },
        field: "tools[0].tunnel_id",
      },
      {
        family: "responses: a stored response to compare the cache with",
        path: "/v1/responses",
        body: {
          model: openai,
          input: "hello",
          prompt_cache_options: { comparison_response_id: "resp_tenant_a_0001" },
        },
        field: "prompt_cache_options.comparison_response_id",
      },
      {
        family: "chat: a stored file",
        path: "/v1/chat/completions",
        body: {
          model: openai,
          messages: [
            {
              role: "user",
              content: [
                { type: "text", text: "what is in it?" },
                { type: "file", file: { file_id: "file-tenant-a-0001" } },
              ],
            },
          ],
        },
        field: "messages[0].content[1].file.file_id",
      },
      {
        family: "chat: a custom voice",
        path: "/v1/chat/completions",
        body: {
          model: openai,
          modalities: ["text", "audio"],
          audio: { format: "wav", voice: { id: "voice_tenant_a_0001" } },
          messages: [{ role: "user", content: "say hello" }],
        },
        field: "audio.voice.id",
      },
      {
        family: "messages: a stored request to diagnose against",
        path: "/v1/messages",
        body: {
          model: anthropic,
          max_tokens: 64,
          diagnostics: { previous_message_id: "msg_tenant_a_0001" },
          messages: [{ role: "user", content: "hello" }],
        },
        field: "diagnostics.previous_message_id",
      },
      {
        family: "messages: a server-held thread",
        path: "/v1/messages",
        body: {
          model: anthropic,
          max_tokens: 64,
          thread: { type: "continue", previous_message_id: "msg_tenant_a_0001" },
          messages: [{ role: "user", content: "go on" }],
        },
        field: "thread",
      },
      {
        family: "messages: a fallback credit token",
        path: "/v1/messages",
        body: {
          model: anthropic,
          max_tokens: 64,
          fallback_credit_token: "fct_tenant_a_0001",
          messages: [{ role: "user", content: "try again" }],
        },
        field: "fallback_credit_token",
      },
      {
        family: "messages: a container by id",
        path: "/v1/messages",
        body: {
          model: anthropic,
          max_tokens: 64,
          container: "container_tenant_a_0001",
          messages: [{ role: "user", content: "run it" }],
        },
        field: "container",
      },
      {
        family: "messages: a stored file",
        path: "/v1/messages",
        body: {
          model: anthropic,
          max_tokens: 64,
          messages: [
            {
              role: "user",
              content: [
                { type: "text", text: "what is in it?" },
                { type: "document", source: { type: "file", file_id: "file_tenant_a_0001" } },
              ],
            },
          ],
        },
        field: "messages[0].content[1].source.file_id",
      },
      {
        family: "count_tokens: a stored file",
        path: "/v1/messages/count_tokens",
        body: {
          model: anthropic,
          messages: [
            {
              role: "user",
              content: [{ type: "image", source: { type: "file", file_id: "file_tenant_a_0001" } }],
            },
          ],
        },
        field: "messages[0].content[0].source.file_id",
      },
    ];
    // Every row is sent before anything is asserted, so a failure shows
    // each family that let its row through.
    const outcomes: Array<{ family: string; status: number; param: unknown }> = [];
    for (const row of rows) {
      const res = await postAs(TENANT_KEYS.b, row.path, row.body);
      const param = res.text.startsWith("{") ? JSON.parse(res.text).error?.param : res.text;
      outcomes.push({ family: row.family, status: res.status, param });
    }
    expect(outcomes).toEqual(
      rows.map((row) => ({ family: row.family, status: 400, param: row.field })),
    );
    expect(up.refs!.receivedRequests, "no OpenAI-shape request reached the upstream").toHaveLength(0);
    expect(refsAnthropic!.receivedRequests, "no Messages request reached the upstream").toHaveLength(0);

    // Without such a reference, the same requests are served.
    const fresh = await postAs(TENANT_KEYS.b, "/v1/responses", {
      model: openai,
      input: "run it",
      tools: [{ type: "code_interpreter", container: { type: "auto" } }],
    });
    expect(fresh.status, fresh.text).toBe(200);
    expect(up.refs!.receivedRequests).toHaveLength(1);
    const plain = await postAs(TENANT_KEYS.b, "/v1/messages", {
      model: anthropic,
      max_tokens: 64,
      messages: [{ role: "user", content: "hello" }],
    });
    expect(plain.status, plain.text).toBe(200);
    expect(refsAnthropic!.receivedRequests).toHaveLength(1);
    // What the model wrote as a tool's input is its own, whatever it looks
    // like: a conversation carrying it goes on.
    const toolTurn = await postAs(TENANT_KEYS.b, "/v1/messages", {
      model: anthropic,
      max_tokens: 64,
      tools: [{ name: "load", input_schema: { type: "object" } }],
      messages: [
        { role: "user", content: "load the report" },
        {
          role: "assistant",
          content: [
            {
              type: "tool_use",
              id: "toolu_load_1",
              name: "load",
              input: { source: { type: "file", path: "/tmp/report.csv" } },
            },
          ],
        },
        {
          role: "user",
          content: [{ type: "tool_result", tool_use_id: "toolu_load_1", content: "loaded" }],
        },
      ],
    });
    expect(toolTurn.status, toolTurn.text).toBe(200);
    expect(refsAnthropic!.receivedRequests).toHaveLength(2);
  });
});
