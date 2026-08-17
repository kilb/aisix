import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  metricDelta,
  scrapeMetrics,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-native-stream-timeout";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const TIMEOUT_MS = 300;
const STALL_MS = 3_000;

const MODELS = {
  messagesFirst: "native-messages-first-timeout",
  messagesMid: "native-messages-mid-timeout",
  responsesFirst: "native-responses-first-timeout",
  responsesMid: "native-responses-mid-timeout",
  messagesTerminal: "native-messages-terminal-before-stall",
  responsesTerminal: "native-responses-terminal-before-stall",
  chatBridgeMid: "chat-bridge-mid-timeout",
  messagesBridgeMid: "messages-bridge-mid-timeout",
  responsesBridgeMid: "responses-bridge-mid-timeout",
  chatBridgeEof: "chat-bridge-premature-eof",
  messagesBridgeEof: "messages-bridge-premature-eof",
  responsesBridgeEof: "responses-bridge-premature-eof",
} as const;

const REQUEST_MARKERS = {
  messagesFirst: "messages-first-request-7c21",
  messagesMid: "messages-mid-request-8d32",
  responsesFirst: "responses-first-request-9e43",
  responsesMid: "responses-mid-request-af54",
  messagesTerminal: "messages-terminal-request-16cb",
  responsesTerminal: "responses-terminal-request-27dc",
  chatBridgeMid: "chat-bridge-mid-request-b065",
  messagesBridgeMid: "messages-bridge-mid-request-c176",
  responsesBridgeMid: "responses-bridge-mid-request-d287",
  chatBridgeEof: "chat-bridge-eof-request-e398",
  messagesBridgeEof: "messages-bridge-eof-request-f409",
  responsesBridgeEof: "responses-bridge-eof-request-05ba",
} as const;

const OUTPUT_MARKERS = {
  messagesFirst: "messages-first-late-output-b165",
  messagesMidFirst: "messages-mid-first-output-c276",
  messagesMidLate: "messages-mid-late-output-d387",
  responsesFirst: "responses-first-late-output-e498",
  responsesMidFirst: "responses-mid-first-output-f5a9",
  responsesMidLate: "responses-mid-late-output-06ba",
  messagesTerminal: "messages-terminal-output-071c",
  messagesAfterTerminal: "messages-after-terminal-must-not-arrive-182d",
  responsesTerminal: "responses-terminal-output-293e",
  responsesAfterTerminal: "responses-after-terminal-must-not-arrive-3a4f",
  chatBridgeFirst: "chat-bridge-first-output-17cb",
  chatBridgeLate: "chat-bridge-late-output-28dc",
  messagesBridgeFirst: "messages-bridge-first-output-39ed",
  messagesBridgeLate: "messages-bridge-late-output-4afe",
  responsesBridgeFirst: "responses-bridge-first-output-5b0f",
  responsesBridgeLate: "responses-bridge-late-output-6c10",
  chatBridgeEof: "chat-bridge-eof-output-7d21",
  messagesBridgeEof: "messages-bridge-eof-output-8e32",
  responsesBridgeEof: "responses-bridge-eof-output-9f43",
} as const;

function chatEvents(first: string, late: string): string[] {
  const chunk = (content: string, finishReason: string | null = null) =>
    JSON.stringify({
      id: "chatcmpl-timeout",
      object: "chat.completion.chunk",
      created: 1,
      model: "deepseek-chat",
      choices: [{ index: 0, delta: { content }, finish_reason: finishReason }],
    });
  return [chunk(first), chunk(late, "stop"), "[DONE]"];
}

function messagesFrames(first: string, late?: string): string[] {
  const prefix =
    `event: message_start\ndata: {"type":"message_start","message":{"id":"msg-timeout","type":"message","role":"assistant","content":[],"model":"claude","stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}\n\n` +
    `event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n` +
    `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: first } })}\n\n`;
  const suffix =
    (late
      ? `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: late } })}\n\n`
      : "") +
    `event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n` +
    `event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}\n\n` +
    `event: message_stop\ndata: {"type":"message_stop"}\n\n`;
  return late ? [prefix, suffix] : [prefix + suffix];
}

function responsesFrames(first: string, late?: string): string[] {
  const prefix = `data: ${JSON.stringify({ type: "response.output_text.delta", delta: first })}\n\n`;
  const suffix =
    (late
      ? `data: ${JSON.stringify({ type: "response.output_text.delta", delta: late })}\n\n`
      : "") +
    `data: ${JSON.stringify({
      type: "response.completed",
      response: {
        id: "resp-timeout",
        object: "response",
        status: "completed",
        model: "gpt-4o-mini",
        output: [],
        usage: { input_tokens: 3, output_tokens: 2, total_tokens: 5 },
      },
    })}\n\n` +
    "data: [DONE]\n\n";
  return late ? [prefix, suffix] : [prefix + suffix];
}

async function readBounded(response: Response): Promise<string> {
  const reader = response.body!.getReader();
  const decoder = new TextDecoder();
  let body = "";
  try {
    for (;;) {
      const next = await reader.read();
      if (next.done) break;
      body += decoder.decode(next.value, { stream: true });
    }
  } catch {
    // A typed error terminates an already-committed HTTP body. Depending on
    // the fetch transport, truncation is surfaced as either EOF or rejection.
  }
  return body + decoder.decode();
}

async function postMessages(app: SpawnedApp, model: string, marker: string): Promise<Response> {
  return fetch(`${app.proxyUrl}/v1/messages`, {
    method: "POST",
    headers: {
      "x-api-key": CALLER,
      "anthropic-version": "2023-06-01",
      "content-type": "application/json",
    },
    body: JSON.stringify({
      model,
      max_tokens: 16,
      stream: true,
      messages: [{ role: "user", content: marker }],
    }),
  });
}

async function postResponses(app: SpawnedApp, model: string, marker: string): Promise<Response> {
  return fetch(`${app.proxyUrl}/v1/responses`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CALLER}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ model, input: marker, stream: true }),
  });
}

async function postChat(app: SpawnedApp, model: string, marker: string): Promise<Response> {
  return fetch(`${app.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CALLER}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      model,
      stream: true,
      messages: [{ role: "user", content: marker }],
    }),
  });
}

async function usage5xxCounter(app: SpawnedApp, handler: string): Promise<number> {
  const response = await fetch(`${app.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  const line = (await response.text())
    .split("\n")
    .find(
      (candidate) =>
        candidate.startsWith("aisix_usage_events_emitted_total{") &&
        candidate.includes(`handler="${handler}"`) &&
        candidate.includes('status_code="5xx"'),
    );
  return line ? Number(line.trim().split(/\s+/).at(-1)) : 0;
}

async function usage2xxCounter(app: SpawnedApp, handler: string): Promise<number> {
  const response = await fetch(`${app.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  const line = (await response.text())
    .split("\n")
    .find(
      (candidate) =>
        candidate.startsWith("aisix_usage_events_emitted_total{") &&
        candidate.includes(`handler="${handler}"`) &&
        candidate.includes('status_code="2xx"'),
    );
  return line ? Number(line.trim().split(/\s+/).at(-1)) : 0;
}

async function waitForTimeoutObservability(
  app: SpawnedApp,
  modelId: string,
  model: string,
  endpoint: "/v1/chat/completions" | "/v1/messages" | "/v1/responses",
  usageBaseline: number,
  reason = "request_timeout",
): Promise<void> {
  let observedStatus: unknown;
  try {
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app.metricsUrl}/status/models`);
      if (response.status !== 200) {
        throw new Error(`model status probe returned ${response.status}`);
      }
      const rows = (await response.json()) as Array<{
        id?: string;
        status?: string;
        status_reason?: string;
      }>;
      observedStatus = rows.find((row) => row.id === modelId);
      return rows.some(
        (row) =>
          row.id === modelId &&
          row.status === "cooldown" &&
          row.status_reason === reason,
      );
    });
  } catch (error) {
    throw new Error(
      `${error instanceof Error ? error.message : String(error)}; model status=${JSON.stringify(observedStatus)}`,
    );
  }

  await waitConfigPropagation(async () => {
    const response = await fetch(`${app.metricsUrl}/metrics`);
    if (response.status !== 200) {
      throw new Error(`metrics probe returned ${response.status}`);
    }
    const metrics = await response.text();
    return metrics.split("\n").some(
      (line) =>
        line.startsWith("aisix_request_e2e_latency_seconds_count{") &&
        line.includes(`endpoint="${endpoint}"`) &&
        line.includes(`model="${model}"`) &&
        line.includes('status_class="5xx"') &&
        line.includes('streaming="true"'),
    );
  });

  const handler = endpoint === "/v1/chat/completions" ? "chat" : endpoint.slice(4);
  await waitConfigPropagation(async () => {
    return (await usage5xxCounter(app, handler)) === usageBaseline + 1;
  });
}

describe("stream timeout terminal state across handler families", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let messagesFirst: OpenAiUpstream | undefined;
  let messagesMid: OpenAiUpstream | undefined;
  let responsesFirst: OpenAiUpstream | undefined;
  let responsesMid: OpenAiUpstream | undefined;
  let messagesTerminal: OpenAiUpstream | undefined;
  let responsesTerminal: OpenAiUpstream | undefined;
  let chatBridgeMid: OpenAiUpstream | undefined;
  let messagesBridgeMid: OpenAiUpstream | undefined;
  let responsesBridgeMid: OpenAiUpstream | undefined;
  let chatBridgeEof: OpenAiUpstream | undefined;
  let messagesBridgeEof: OpenAiUpstream | undefined;
  let responsesBridgeEof: OpenAiUpstream | undefined;
  const modelIds: Partial<Record<keyof typeof MODELS, string>> = {};

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    messagesFirst = await startOpenAiUpstream({
      firstEventDelayMs: STALL_MS,
      rawSseChunks: messagesFrames(OUTPUT_MARKERS.messagesFirst),
    });
    messagesMid = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      rawSseChunks: messagesFrames(
        OUTPUT_MARKERS.messagesMidFirst,
        OUTPUT_MARKERS.messagesMidLate,
      ),
    });
    responsesFirst = await startOpenAiUpstream({
      firstEventDelayMs: STALL_MS,
      rawSseChunks: responsesFrames(OUTPUT_MARKERS.responsesFirst),
    });
    responsesMid = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      rawSseChunks: responsesFrames(
        OUTPUT_MARKERS.responsesMidFirst,
        OUTPUT_MARKERS.responsesMidLate,
      ),
    });
    messagesTerminal = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      rawSseChunks: [
        messagesFrames(OUTPUT_MARKERS.messagesTerminal)[0]! +
          `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: OUTPUT_MARKERS.messagesAfterTerminal } })}\n\n`,
        `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: OUTPUT_MARKERS.messagesAfterTerminal } })}\n\n`,
      ],
    });
    responsesTerminal = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      rawSseChunks: [
        responsesFrames(OUTPUT_MARKERS.responsesTerminal)[0]! +
          `data: ${JSON.stringify({ type: "response.output_text.delta", delta: OUTPUT_MARKERS.responsesAfterTerminal })}\n\n`,
        `data: ${JSON.stringify({ type: "response.output_text.delta", delta: OUTPUT_MARKERS.responsesAfterTerminal })}\n\n`,
      ],
    });
    chatBridgeMid = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      streamEvents: chatEvents(
        OUTPUT_MARKERS.chatBridgeFirst,
        OUTPUT_MARKERS.chatBridgeLate,
      ),
    });
    messagesBridgeMid = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      streamEvents: chatEvents(
        OUTPUT_MARKERS.messagesBridgeFirst,
        OUTPUT_MARKERS.messagesBridgeLate,
      ),
    });
    responsesBridgeMid = await startOpenAiUpstream({
      eventDelayMs: STALL_MS,
      streamEvents: chatEvents(
        OUTPUT_MARKERS.responsesBridgeFirst,
        OUTPUT_MARKERS.responsesBridgeLate,
      ),
    });
    const openAiPartial = (marker: string) => [
      JSON.stringify({
        id: "chatcmpl-eof",
        object: "chat.completion.chunk",
        created: 1,
        model: "deepseek-chat",
        choices: [{ index: 0, delta: { content: marker }, finish_reason: null }],
      }),
    ];
    chatBridgeEof = await startOpenAiUpstream({
      streamEvents: openAiPartial(OUTPUT_MARKERS.chatBridgeEof),
    });
    messagesBridgeEof = await startOpenAiUpstream({
      streamEvents: openAiPartial(OUTPUT_MARKERS.messagesBridgeEof),
    });
    responsesBridgeEof = await startOpenAiUpstream({
      rawSseChunks: [
        'event: message_start\ndata: {"type":"message_start","message":{"id":"msg-eof","type":"message","role":"assistant","content":[],"model":"claude","stop_reason":null,"usage":{"input_tokens":3,"output_tokens":0}}}\n\n',
        `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: OUTPUT_MARKERS.responsesBridgeEof } })}\n\n`,
      ],
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const createModel = async (
      key: keyof typeof MODELS,
      provider: "anthropic" | "openai",
      upstream: OpenAiUpstream,
    ) => {
      const providerKey = await seed.createProviderKey({
        display_name: `${MODELS[key]}-pk`,
        provider,
        adapter: provider,
        secret: "sk-mock",
        api_base: provider === "anthropic" ? upstream.baseUrl : `${upstream.baseUrl}/v1`,
      });
      const model = await seed.createModel({
        display_name: MODELS[key],
        provider,
        model_name: provider === "anthropic" ? "claude" : "gpt-4o-mini",
        provider_key_id: providerKey.id,
        stream_timeout: TIMEOUT_MS,
        retries: 0,
        cooldown: { default_seconds: 60 },
      });
      modelIds[key] = model.id;
    };

    await createModel("messagesFirst", "anthropic", messagesFirst);
    await createModel("messagesMid", "anthropic", messagesMid);
    await createModel("responsesFirst", "openai", responsesFirst);
    await createModel("responsesMid", "openai", responsesMid);
    await createModel("messagesTerminal", "anthropic", messagesTerminal);
    await createModel("responsesTerminal", "openai", responsesTerminal);
    const createBridgeModel = async (
      key:
        | "chatBridgeMid"
        | "messagesBridgeMid"
        | "responsesBridgeMid"
        | "chatBridgeEof"
        | "messagesBridgeEof",
      upstream: OpenAiUpstream,
    ) => {
      const providerKey = await seed.createProviderKey({
        display_name: `${MODELS[key]}-pk`,
        provider: "deepseek",
        adapter: "openai",
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      const model = await seed.createModel({
        display_name: MODELS[key],
        provider: "deepseek",
        model_name: "deepseek-chat",
        provider_key_id: providerKey.id,
        stream_timeout: TIMEOUT_MS,
        retries: 0,
        cooldown: { default_seconds: 60 },
      });
      modelIds[key] = model.id;
    };
    await createBridgeModel("chatBridgeMid", chatBridgeMid);
    await createBridgeModel("messagesBridgeMid", messagesBridgeMid);
    await createBridgeModel("responsesBridgeMid", responsesBridgeMid);
    await createBridgeModel("chatBridgeEof", chatBridgeEof);
    await createBridgeModel("messagesBridgeEof", messagesBridgeEof);
    await createModel("responsesBridgeEof", "anthropic", responsesBridgeEof);
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: Object.values(MODELS),
    });

    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      if (response.status === 401) return false;
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((model) => model.id));
      return Object.values(MODELS).every((model) => ids.has(model));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([
      messagesFirst?.close(),
      messagesMid?.close(),
      responsesFirst?.close(),
      responsesMid?.close(),
      messagesTerminal?.close(),
      responsesTerminal?.close(),
      chatBridgeMid?.close(),
      messagesBridgeMid?.close(),
      responsesBridgeMid?.close(),
      chatBridgeEof?.close(),
      messagesBridgeEof?.close(),
      responsesBridgeEof?.close(),
    ]);
  });

  test("first-event stalls fail with 504 before committing a stream", async (ctx) => {
    if (!etcdReachable || !app || !messagesFirst || !responsesFirst) {
      ctx.skip();
      return;
    }

    const messagesBase = messagesFirst.receivedRequests.length;
    const messagesUsageBase = await usage5xxCounter(app, "messages");
    const messagesStarted = Date.now();
    const messagesResponse = await postMessages(
      app,
      MODELS.messagesFirst,
      REQUEST_MARKERS.messagesFirst,
    );
    expect(messagesResponse.status).toBe(504);
    expect(Date.now() - messagesStarted).toBeLessThan(STALL_MS / 2);
    expect(messagesFirst.receivedRequests.length - messagesBase).toBe(1);
    expect(messagesFirst.receivedRequests.at(-1)?.path).toBe("/v1/messages");
    expect(messagesFirst.receivedRequests.at(-1)?.body).toContain(
      REQUEST_MARKERS.messagesFirst,
    );

    const responsesBase = responsesFirst.receivedRequests.length;
    const responsesUsageBase = await usage5xxCounter(app, "responses");
    const responsesStarted = Date.now();
    const responsesResponse = await postResponses(
      app,
      MODELS.responsesFirst,
      REQUEST_MARKERS.responsesFirst,
    );
    expect(responsesResponse.status).toBe(504);
    expect(Date.now() - responsesStarted).toBeLessThan(STALL_MS / 2);
    expect(responsesFirst.receivedRequests.length - responsesBase).toBe(1);
    expect(responsesFirst.receivedRequests.at(-1)?.path).toBe("/v1/responses");
    expect(responsesFirst.receivedRequests.at(-1)?.body).toContain(
      REQUEST_MARKERS.responsesFirst,
    );

    await waitForTimeoutObservability(
      app,
      modelIds.messagesFirst!,
      MODELS.messagesFirst,
      "/v1/messages",
      messagesUsageBase,
    );
    await waitForTimeoutObservability(
      app,
      modelIds.responsesFirst!,
      MODELS.responsesFirst,
      "/v1/responses",
      responsesUsageBase,
    );
  });

  test("mid-stream stalls terminate promptly and record timeout instead of clean EOF", async (ctx) => {
    if (!etcdReachable || !app || !messagesMid || !responsesMid) {
      ctx.skip();
      return;
    }

    const messagesBase = messagesMid.receivedRequests.length;
    const messagesUsageBase = await usage5xxCounter(app, "messages");
    const messagesStarted = Date.now();
    const messagesResponse = await postMessages(
      app,
      MODELS.messagesMid,
      REQUEST_MARKERS.messagesMid,
    );
    expect(messagesResponse.status).toBe(200);
    const messagesBody = await readBounded(messagesResponse);
    expect(Date.now() - messagesStarted).toBeLessThan(STALL_MS / 2);
    expect(messagesBody).toContain(OUTPUT_MARKERS.messagesMidFirst);
    expect(messagesBody).not.toContain(OUTPUT_MARKERS.messagesMidLate);
    expect(messagesBody).not.toContain('"type":"message_stop"');
    expect(messagesMid.receivedRequests.length - messagesBase).toBe(1);
    expect(messagesMid.receivedRequests.at(-1)?.body).toContain(REQUEST_MARKERS.messagesMid);

    const responsesBase = responsesMid.receivedRequests.length;
    const responsesUsageBase = await usage5xxCounter(app, "responses");
    const responsesStarted = Date.now();
    const responsesResponse = await postResponses(
      app,
      MODELS.responsesMid,
      REQUEST_MARKERS.responsesMid,
    );
    expect(responsesResponse.status).toBe(200);
    const responsesBody = await readBounded(responsesResponse);
    expect(Date.now() - responsesStarted).toBeLessThan(STALL_MS / 2);
    expect(responsesBody).toContain(OUTPUT_MARKERS.responsesMidFirst);
    expect(responsesBody).not.toContain(OUTPUT_MARKERS.responsesMidLate);
    expect(responsesBody).not.toContain("response.completed");
    expect(responsesMid.receivedRequests.length - responsesBase).toBe(1);
    expect(responsesMid.receivedRequests.at(-1)?.body).toContain(REQUEST_MARKERS.responsesMid);

    await waitForTimeoutObservability(
      app,
      modelIds.messagesMid!,
      MODELS.messagesMid,
      "/v1/messages",
      messagesUsageBase,
    );
    await waitForTimeoutObservability(
      app,
      modelIds.responsesMid!,
      MODELS.responsesMid,
      "/v1/responses",
      responsesUsageBase,
    );
  });

  test("semantic terminals complete before a later transport stall", async (ctx) => {
    if (!etcdReachable || !app || !messagesTerminal || !responsesTerminal) {
      ctx.skip();
      return;
    }
    const cases = [
      {
        endpoint: "/v1/messages" as const,
        modelKey: "messagesTerminal" as const,
        upstream: messagesTerminal,
        marker: OUTPUT_MARKERS.messagesTerminal,
        after: OUTPUT_MARKERS.messagesAfterTerminal,
        post: postMessages,
      },
      {
        endpoint: "/v1/responses" as const,
        modelKey: "responsesTerminal" as const,
        upstream: responsesTerminal,
        marker: OUTPUT_MARKERS.responsesTerminal,
        after: OUTPUT_MARKERS.responsesAfterTerminal,
        post: postResponses,
      },
    ];

    for (const scenario of cases) {
      const handler = scenario.endpoint.slice(4);
      const usageBaseline = await usage2xxCounter(app, handler);
      const failureBaseline = await usage5xxCounter(app, handler);
      const upstreamBaseline = scenario.upstream.receivedRequests.length;
      const started = Date.now();
      const response = await scenario.post(
        app,
        MODELS[scenario.modelKey],
        REQUEST_MARKERS[scenario.modelKey],
      );
      expect(response.status).toBe(200);
      const body = await response.text();
      expect(Date.now() - started).toBeLessThan(STALL_MS / 2);
      expect(body).toContain(scenario.marker);
      expect(body).not.toContain(scenario.after);
      expect(body).not.toContain("upstream stream failed");
      expect(scenario.upstream.receivedRequests.length).toBe(
        upstreamBaseline + 1,
      );

      await waitConfigPropagation(
        async () => (await usage2xxCounter(app!, handler)) === usageBaseline + 1,
      );
      expect(await usage5xxCounter(app, handler)).toBe(failureBaseline);
      const statusResponse = await fetch(`${app.metricsUrl}/status/models`);
      expect(statusResponse.status).toBe(200);
      const statuses = (await statusResponse.json()) as Array<{
        id?: string;
        status?: string;
      }>;
      expect(
        statuses.find((row) => row.id === modelIds[scenario.modelKey])?.status,
      ).not.toBe("cooldown");
    }
  });

  test("translated streams retain timeout failure state after the first chunk", async (ctx) => {
    if (
      !etcdReachable ||
      !app ||
      !chatBridgeMid ||
      !messagesBridgeMid ||
      !responsesBridgeMid
    ) {
      ctx.skip();
      return;
    }

    const cases = [
      {
        endpoint: "/v1/chat/completions" as const,
        modelKey: "chatBridgeMid" as const,
        upstream: chatBridgeMid,
        first: OUTPUT_MARKERS.chatBridgeFirst,
        late: OUTPUT_MARKERS.chatBridgeLate,
        post: postChat,
      },
      {
        endpoint: "/v1/messages" as const,
        modelKey: "messagesBridgeMid" as const,
        upstream: messagesBridgeMid,
        first: OUTPUT_MARKERS.messagesBridgeFirst,
        late: OUTPUT_MARKERS.messagesBridgeLate,
        post: postMessages,
      },
      {
        endpoint: "/v1/responses" as const,
        modelKey: "responsesBridgeMid" as const,
        upstream: responsesBridgeMid,
        first: OUTPUT_MARKERS.responsesBridgeFirst,
        late: OUTPUT_MARKERS.responsesBridgeLate,
        post: postResponses,
      },
    ];

    for (const scenario of cases) {
      const baseline = scenario.upstream.receivedRequests.length;
      const handler =
        scenario.endpoint === "/v1/chat/completions"
          ? "chat"
          : scenario.endpoint.slice(4);
      const usageBaseline = await usage5xxCounter(app, handler);
      const deploymentBaseline = await scrapeMetrics(app.metricsUrl);
      const started = Date.now();
      const response = await scenario.post(
        app,
        MODELS[scenario.modelKey],
        REQUEST_MARKERS[scenario.modelKey],
      );
      const body = await readBounded(response);
      expect(response.status, `${scenario.modelKey}: ${body}`).toBe(200);
      expect(Date.now() - started).toBeLessThan(STALL_MS / 2);
      expect(body).toContain(scenario.first);
      expect(body).not.toContain(scenario.late);
      expect(body).toContain("error");
      expect(scenario.upstream.receivedRequests.length - baseline).toBe(1);
      expect(scenario.upstream.receivedRequests.at(-1)?.path).toBe(
        "/v1/chat/completions",
      );
      expect(scenario.upstream.receivedRequests.at(-1)?.body).toContain(
        REQUEST_MARKERS[scenario.modelKey],
      );
      await waitForTimeoutObservability(
        app,
        modelIds[scenario.modelKey]!,
        MODELS[scenario.modelKey],
        scenario.endpoint,
        usageBaseline,
      );
      const deploymentAfter = await scrapeMetrics(app.metricsUrl);
      expect(
        metricDelta(
          deploymentBaseline,
          deploymentAfter,
          "aisix_deployment_failure_responses_total",
          { model: MODELS[scenario.modelKey] },
        ),
      ).toBe(1);
      expect(
        metricDelta(
          deploymentBaseline,
          deploymentAfter,
          "aisix_deployment_success_responses_total",
          { model: MODELS[scenario.modelKey] },
        ),
      ).toBe(0);
    }
  }, 30_000);

  test("translated streams reject clean EOF before the provider terminal event", async (ctx) => {
    if (
      !etcdReachable ||
      !app ||
      !chatBridgeEof ||
      !messagesBridgeEof ||
      !responsesBridgeEof
    ) {
      ctx.skip();
      return;
    }

    const cases = [
      {
        endpoint: "/v1/chat/completions" as const,
        modelKey: "chatBridgeEof" as const,
        upstream: chatBridgeEof,
        output: OUTPUT_MARKERS.chatBridgeEof,
        forbiddenTerminal: "[DONE]",
        upstreamPath: "/v1/chat/completions",
        post: postChat,
      },
      {
        endpoint: "/v1/messages" as const,
        modelKey: "messagesBridgeEof" as const,
        upstream: messagesBridgeEof,
        output: OUTPUT_MARKERS.messagesBridgeEof,
        forbiddenTerminal: '"type":"message_stop"',
        upstreamPath: "/v1/chat/completions",
        post: postMessages,
      },
      {
        endpoint: "/v1/responses" as const,
        modelKey: "responsesBridgeEof" as const,
        upstream: responsesBridgeEof,
        output: OUTPUT_MARKERS.responsesBridgeEof,
        forbiddenTerminal: "response.completed",
        upstreamPath: "/v1/messages",
        post: postResponses,
      },
    ];

    for (const scenario of cases) {
      const baseline = scenario.upstream.receivedRequests.length;
      const handler =
        scenario.endpoint === "/v1/chat/completions"
          ? "chat"
          : scenario.endpoint.slice(4);
      const usageBaseline = await usage5xxCounter(app, handler);
      const response = await scenario.post(
        app,
        MODELS[scenario.modelKey],
        REQUEST_MARKERS[scenario.modelKey],
      );
      const body = await readBounded(response);
      expect(response.status, `${scenario.modelKey}: ${body}`).toBe(200);
      expect(body).toContain(scenario.output);
      expect(body).toContain("error");
      expect(body).not.toContain(scenario.forbiddenTerminal);
      expect(scenario.upstream.receivedRequests).toHaveLength(baseline + 1);
      expect(scenario.upstream.receivedRequests.at(-1)?.path).toBe(
        scenario.upstreamPath,
      );
      expect(scenario.upstream.receivedRequests.at(-1)?.body).toContain(
        REQUEST_MARKERS[scenario.modelKey],
      );
      await waitForTimeoutObservability(
        app,
        modelIds[scenario.modelKey]!,
        MODELS[scenario.modelKey],
        scenario.endpoint,
        usageBaseline,
        "transport_error",
      );
    }
  }, 30_000);
});
