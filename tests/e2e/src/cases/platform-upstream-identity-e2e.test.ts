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
// The response-level id of a Responses answer is outside this spec.

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
