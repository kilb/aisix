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
};

/** What must never appear anywhere in a response the caller receives. */
function expectNoUpstreamIdentity(text: string): void {
  for (const marker of [
    "bdrk",
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
function openAiChatAnswer(): Record<string, unknown> {
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
              id: UPSTREAM_TOOL_USE_ID,
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

function openAiChatStream(): string[] {
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
              id: UPSTREAM_TOOL_USE_ID,
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
    const routes: Array<[keyof typeof ALIASES, { provider: string; adapter: string }]> = [
      ["chatOpenAi", openai],
      ["chatAnthropic", anthropic],
      ["messagesNative", anthropic],
      ["messagesTranslated", openai],
      ["responsesNative", openai],
      ["responsesTranslated", anthropic],
    ];
    for (const [route, kind] of routes) {
      const pk = await seed.createProviderKey({
        display_name: `pk-${ALIASES[route]}`,
        secret: "sk-mock",
        api_base: `${up[route]!.baseUrl}/v1`,
        provider: kind.provider,
        adapter: kind.adapter,
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
    expect(call.call_id).toMatch(/^toolu_/);
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
};

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
  };
  const up: Record<keyof typeof TENANT_ALIASES, OpenAiUpstream | undefined> = {
    target: undefined,
    streamed: undefined,
    grouped: undefined,
    otherGroup: undefined,
    probe: undefined,
  };

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
        if (!Object.values(TENANT_ALIASES).every((alias) => data.some((m) => m.id === alias))) {
          return false;
        }
      }
      return true;
    });
  });

  afterAll(async () => {
    await tenantApp?.exit();
    await Promise.all(Object.values(up).map((u) => u?.close()));
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
});
