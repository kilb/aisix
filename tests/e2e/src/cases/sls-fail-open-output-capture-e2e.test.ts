import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  decodedTextFor,
  EtcdClient,
  pickFreePort,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForToken,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-sls-fail-open-output-capture";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const CREDENTIAL_REF = "failopen";
const LOGSTORE = "full-events-fail-open";
const REQUEST_IDS = {
  chat: "fail-open-chat-request-8f17",
  native: "fail-open-native-responses-request-3a29",
  bridge: "fail-open-bridge-responses-request-6c41",
  messages: "fail-open-messages-request-5d73",
  messagesBridge: "fail-open-bridge-messages-request-4e62",
  completions: "fail-open-completions-request-7e85",
};
const OUTPUTS = {
  chat: "raw-chat-output-must-not-be-exported-91ab",
  native: "raw-native-output-must-not-be-exported-27cd",
  bridge: "raw-bridge-output-must-not-be-exported-43ef",
  messages: "raw-messages-output-must-not-be-exported-59a1",
  messagesBridge: "raw-bridge-messages-output-must-not-be-exported-68b2",
  completions: "raw-completions-output-must-not-be-exported-75b3",
};
const INPUT_REQUEST_IDS = {
  chat: "fail-open-input-chat-request-a196",
  native: "fail-open-input-native-responses-request-b2a7",
  bridge: "fail-open-input-bridge-responses-request-c3b8",
  messages: "fail-open-input-messages-request-d4c9",
  messagesBridge: "fail-open-input-bridge-messages-request-e5da",
  completions: "fail-open-input-completions-request-f6eb",
};
const INPUTS = {
  chat: "raw-chat-input-must-not-be-exported-17ac",
  native: "raw-native-input-must-not-be-exported-28bd",
  bridge: "raw-bridge-input-must-not-be-exported-39ce",
  messages: "raw-messages-input-must-not-be-exported-4adf",
  messagesBridge: "raw-bridge-messages-input-must-not-be-exported-5be0",
  completions: "raw-completions-input-must-not-be-exported-6cf1",
};
const OVERFLOW_REQUEST_IDS = {
  nativeResponses: "buffer-fail-open-native-responses-18d1",
  bridgeResponses: "buffer-fail-open-bridge-responses-29e2",
  nativeMessages: "buffer-fail-open-native-messages-3af3",
  bridgeMessages: "buffer-fail-open-bridge-messages-4b04",
  completions: "buffer-fail-open-completions-5c15",
};
const OVERFLOW_MARKERS = {
  nativeResponses: "native-responses-overflow-visible-18d1",
  bridgeResponses: "bridge-responses-overflow-visible-29e2",
  nativeMessages: "native-messages-overflow-visible-3af3",
  bridgeMessages: "bridge-messages-overflow-visible-4b04",
  completions: "completions-overflow-visible-5c15",
};
const overflowOutput = (marker: string) => `${marker}:${"x".repeat(2_048)}`;

interface FailingBedrock {
  url: string;
  calls: number;
  requestBodies: string[];
  close(): Promise<void>;
}

async function startFailingBedrock(): Promise<FailingBedrock> {
  const mock = { calls: 0, requestBodies: [] as string[] } as FailingBedrock;
  const server: Server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      mock.calls += 1;
      mock.requestBodies.push(Buffer.concat(chunks).toString("utf8"));
      res.statusCode = 500;
      res.end("mock Bedrock outage");
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  mock.url = `http://127.0.0.1:${port}`;
  mock.close = async () => {
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  };
  return mock;
}

function chatEvents(output: string): string[] {
  return [
    JSON.stringify({
      id: "chat-fail-open",
      object: "chat.completion.chunk",
      model: "gpt-4o-mini",
      choices: [{ index: 0, delta: { role: "assistant" } }],
    }),
    JSON.stringify({
      id: "chat-fail-open",
      object: "chat.completion.chunk",
      model: "gpt-4o-mini",
      choices: [{ index: 0, delta: { content: output }, finish_reason: "stop" }],
      usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 },
    }),
    "[DONE]",
  ];
}

function responsesEvents(output: string): string[] {
  return [
    JSON.stringify({ type: "response.output_text.delta", delta: output }),
    JSON.stringify({
      type: "response.completed",
      response: {
        id: "resp-fail-open",
        object: "response",
        status: "completed",
        model: "gpt-4o-mini",
        output: [
          {
            type: "message",
            role: "assistant",
            content: [{ type: "output_text", text: output }],
          },
        ],
        usage: { input_tokens: 5, output_tokens: 3, total_tokens: 8 },
      },
    }),
    "[DONE]",
  ];
}

function messagesSse(output: string): string {
  return (
    `event: message_start\ndata: {"type":"message_start","message":{"id":"msg-fail-open","type":"message","role":"assistant","content":[],"model":"claude","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0}}}\n\n` +
    `event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}\n\n` +
    `event: content_block_delta\ndata: ${JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: output } })}\n\n` +
    `event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n` +
    `event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}\n\n` +
    `event: message_stop\ndata: {"type":"message_stop"}\n\n`
  );
}

function completionsSse(output: string): string {
  return (
    `data: ${JSON.stringify({ id: "cmpl-fail-open", object: "text_completion", choices: [{ index: 0, text: output, finish_reason: "stop" }], usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 } })}\n\n` +
    "data: [DONE]\n\n"
  );
}

describe("SLS full-content capture after output guardrail fail-open", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  let bedrock: FailingBedrock | undefined;
  let chat: OpenAiUpstream | undefined;
  let native: OpenAiUpstream | undefined;
  let bridge: OpenAiUpstream | undefined;
  let messages: OpenAiUpstream | undefined;
  let messagesBridge: OpenAiUpstream | undefined;
  let completions: OpenAiUpstream | undefined;
  let overflowNativeResponses: OpenAiUpstream | undefined;
  let overflowBridgeResponses: OpenAiUpstream | undefined;
  let overflowNativeMessages: OpenAiUpstream | undefined;
  let overflowBridgeMessages: OpenAiUpstream | undefined;
  let overflowCompletions: OpenAiUpstream | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    sls = await startMockSls();
    bedrock = await startFailingBedrock();
    chat = await startOpenAiUpstream({ streamEvents: chatEvents(OUTPUTS.chat) });
    native = await startOpenAiUpstream({ streamEvents: responsesEvents(OUTPUTS.native) });
    bridge = await startOpenAiUpstream({ streamEvents: chatEvents(OUTPUTS.bridge) });
    messages = await startOpenAiUpstream({ rawSseChunks: [messagesSse(OUTPUTS.messages)] });
    messagesBridge = await startOpenAiUpstream({
      streamEvents: chatEvents(OUTPUTS.messagesBridge),
    });
    completions = await startOpenAiUpstream({
      rawSseChunks: [completionsSse(OUTPUTS.completions)],
    });
    overflowNativeResponses = await startOpenAiUpstream({
      streamEvents: responsesEvents(
        overflowOutput(OVERFLOW_MARKERS.nativeResponses),
      ),
    });
    overflowBridgeResponses = await startOpenAiUpstream({
      streamEvents: chatEvents(
        overflowOutput(OVERFLOW_MARKERS.bridgeResponses),
      ),
    });
    overflowNativeMessages = await startOpenAiUpstream({
      rawSseChunks: [
        messagesSse(overflowOutput(OVERFLOW_MARKERS.nativeMessages)),
      ],
    });
    overflowBridgeMessages = await startOpenAiUpstream({
      streamEvents: chatEvents(
        overflowOutput(OVERFLOW_MARKERS.bridgeMessages),
      ),
    });
    overflowCompletions = await startOpenAiUpstream({
      rawSseChunks: [
        completionsSse(overflowOutput(OVERFLOW_MARKERS.completions)),
      ],
    });
    app = await spawnApp({
      extra: { bedrock_endpoint_url: bedrock.url },
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-fail-open-full",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });
    const modelIds = new Map<string, string>();
    for (const [name, provider, apiBase] of [
      ["fail-open-chat", "openai", `${chat.baseUrl}/v1`],
      ["fail-open-native-responses", "openai", `${native.baseUrl}/v1`],
      ["fail-open-bridge-responses", "deepseek", `${bridge.baseUrl}/v1`],
      ["fail-open-messages", "anthropic", messages.baseUrl],
      ["fail-open-bridge-messages", "deepseek", `${messagesBridge.baseUrl}/v1`],
      ["fail-open-completions", "openai", `${completions.baseUrl}/v1`],
      [
        "buffer-fail-open-native-responses",
        "openai",
        `${overflowNativeResponses.baseUrl}/v1`,
      ],
      [
        "buffer-fail-open-bridge-responses",
        "deepseek",
        `${overflowBridgeResponses.baseUrl}/v1`,
      ],
      [
        "buffer-fail-open-native-messages",
        "anthropic",
        overflowNativeMessages.baseUrl,
      ],
      [
        "buffer-fail-open-bridge-messages",
        "deepseek",
        `${overflowBridgeMessages.baseUrl}/v1`,
      ],
      [
        "buffer-fail-open-completions",
        "openai",
        `${overflowCompletions.baseUrl}/v1`,
      ],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: apiBase,
        ...(provider === "deepseek" ? { provider, adapter: "openai" } : {}),
      });
      const created = await seed.createModel({
        display_name: name,
        provider,
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
      modelIds.set(name, created.id);
    }
    await seed.createGuardrail({
      name: "bedrock-output-fail-open",
      enabled: true,
      hook_point: "output",
      fail_open: false,
      output_fail_open: true,
      kind: "bedrock",
      guardrail_id: "failopengr0001",
      guardrail_version: "DRAFT",
      region: "us-east-1",
      aws_credentials: {
        kind: "static",
        access_key_id: "AKIDFAILOPEN0001",
        secret_access_key: "secret-fail-open",
      },
      latency_mode: { kind: "serial" },
      enforcement_mode: "monitor",
    });
    await seed.createGuardrail({
      name: "bedrock-input-fail-open",
      enabled: true,
      hook_point: "input",
      fail_open: true,
      kind: "bedrock",
      guardrail_id: "failopengr0002",
      guardrail_version: "DRAFT",
      region: "us-east-1",
      aws_credentials: {
        kind: "static",
        access_key_id: "AKIDFAILOPEN0002",
        secret_access_key: "secret-input-fail-open",
      },
      latency_mode: { kind: "serial" },
      enforcement_mode: "enforce",
    });
    const overflowGuardrail = await seed.createGuardrail({
      name: "pii-output-buffer-fail-open",
      enabled: true,
      hook_point: "output",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
      max_buffer_bytes: 1_024,
      on_buffer_exceeded: "fail_open",
    });
    for (const modelName of [
      "buffer-fail-open-native-responses",
      "buffer-fail-open-bridge-responses",
      "buffer-fail-open-native-messages",
      "buffer-fail-open-bridge-messages",
      "buffer-fail-open-completions",
    ]) {
      const modelId = modelIds.get(modelName);
      if (!modelId) throw new Error(`missing model id for ${modelName}`);
      await etcd.put(
        `${app.etcdPrefix}/guardrail_attachments/${randomUUID()}`,
        JSON.stringify({
          guardrail_id: overflowGuardrail.id,
          env_id: randomUUID(),
          scope_type: "model",
          scope_id: modelId,
          priority: 0,
          enabled: true,
        }),
      );
    }
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: [
        "fail-open-chat",
        "fail-open-native-responses",
        "fail-open-bridge-responses",
        "fail-open-messages",
        "fail-open-bridge-messages",
        "fail-open-completions",
        "buffer-fail-open-native-responses",
        "buffer-fail-open-bridge-responses",
        "buffer-fail-open-native-messages",
        "buffer-fail-open-bridge-messages",
        "buffer-fail-open-completions",
      ],
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
      return [
        "fail-open-chat",
        "fail-open-native-responses",
        "fail-open-bridge-responses",
        "fail-open-messages",
        "fail-open-bridge-messages",
        "fail-open-completions",
        "buffer-fail-open-native-responses",
        "buffer-fail-open-bridge-responses",
        "buffer-fail-open-native-messages",
        "buffer-fail-open-bridge-messages",
      ].every((model) => ids.has(model));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([
      chat?.close(),
      native?.close(),
      bridge?.close(),
      messages?.close(),
      messagesBridge?.close(),
      completions?.close(),
      overflowNativeResponses?.close(),
      overflowBridgeResponses?.close(),
      overflowNativeMessages?.close(),
      overflowBridgeMessages?.close(),
      overflowCompletions?.close(),
    ]);
    await bedrock?.close();
    await sls?.close();
  });

  test(
    "releases fail-open responses but omits uninspected output from full-content logs",
    async (ctx) => {
      if (!etcdReachable || !app || !sls || !bedrock) {
        ctx.skip();
        return;
      }
      const before = sls.requests.length;
      const post = (path: string, requestId: string, body: unknown) =>
        fetch(`${app!.proxyUrl}${path}`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${CALLER}`,
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify(body),
        });

      const chatResponse = await post("/v1/chat/completions", REQUEST_IDS.chat, {
        model: "fail-open-chat",
        stream: true,
        messages: [{ role: "user", content: "ordinary chat prompt" }],
      });
      expect(chatResponse.status).toBe(200);
      expect(await chatResponse.text()).toContain(OUTPUTS.chat);

      const nativeResponse = await post("/v1/responses", REQUEST_IDS.native, {
        model: "fail-open-native-responses",
        input: "ordinary native responses prompt",
        stream: true,
      });
      expect(nativeResponse.status).toBe(200);
      expect(await nativeResponse.text()).toContain(OUTPUTS.native);

      const bridgeResponse = await post("/v1/responses", REQUEST_IDS.bridge, {
        model: "fail-open-bridge-responses",
        input: "ordinary bridge responses prompt",
        stream: true,
      });
      expect(bridgeResponse.status).toBe(200);
      expect(await bridgeResponse.text()).toContain(OUTPUTS.bridge);

      const messagesResponse = await fetch(`${app.proxyUrl}/v1/messages`, {
        method: "POST",
        headers: {
          "x-api-key": CALLER,
          "anthropic-version": "2023-06-01",
          "content-type": "application/json",
          "x-aisix-request-id": REQUEST_IDS.messages,
        },
        body: JSON.stringify({
          model: "fail-open-messages",
          messages: [{ role: "user", content: "ordinary messages prompt" }],
          max_tokens: 16,
          stream: true,
        }),
      });
      expect(messagesResponse.status).toBe(200);
      expect(await messagesResponse.text()).toContain(OUTPUTS.messages);

      const messagesBridgeResponse = await fetch(`${app.proxyUrl}/v1/messages`, {
        method: "POST",
        headers: {
          "x-api-key": CALLER,
          "anthropic-version": "2023-06-01",
          "content-type": "application/json",
          "x-aisix-request-id": REQUEST_IDS.messagesBridge,
        },
        body: JSON.stringify({
          model: "fail-open-bridge-messages",
          messages: [{ role: "user", content: "ordinary bridge messages prompt" }],
          max_tokens: 16,
          stream: true,
        }),
      });
      expect(messagesBridgeResponse.status).toBe(200);
      expect(await messagesBridgeResponse.text()).toContain(OUTPUTS.messagesBridge);

      const completionsResponse = await post(
        "/v1/completions",
        REQUEST_IDS.completions,
        {
          model: "fail-open-completions",
          prompt: "ordinary completions prompt",
          max_tokens: 16,
          stream: true,
        },
      );
      expect(completionsResponse.status).toBe(200);
      expect(await completionsResponse.text()).toContain(OUTPUTS.completions);
      expect(bedrock.calls).toBeGreaterThanOrEqual(6);
      const guardrailRequests = bedrock.requestBodies.join("\n");
      for (const output of Object.values(OUTPUTS)) {
        expect(guardrailRequests).toContain(output);
      }

      for (const requestId of Object.values(REQUEST_IDS)) {
        await waitForToken(sls, LOGSTORE, requestId, 15_000, before);
      }
      const exported = decodedTextFor(sls, LOGSTORE, before);
      for (const requestId of Object.values(REQUEST_IDS)) {
        expect(exported).toContain(requestId);
      }
      for (const output of Object.values(OUTPUTS)) {
        expect(exported).not.toContain(output);
      }
    },
    90_000,
  );

  test(
    "buffer fail-open releases all streaming families without capturing output",
    async (ctx) => {
      if (
        !etcdReachable ||
        !app ||
        !sls ||
        !overflowNativeResponses ||
        !overflowBridgeResponses ||
        !overflowNativeMessages ||
        !overflowBridgeMessages ||
        !overflowCompletions
      ) {
        ctx.skip();
        return;
      }
      const beforeLogs = sls.requests.length;
      const baselines = {
        nativeResponses: overflowNativeResponses.receivedRequests.length,
        bridgeResponses: overflowBridgeResponses.receivedRequests.length,
        nativeMessages: overflowNativeMessages.receivedRequests.length,
        bridgeMessages: overflowBridgeMessages.receivedRequests.length,
        completions: overflowCompletions.receivedRequests.length,
      };
      const post = (path: string, requestId: string, body: unknown) =>
        fetch(`${app!.proxyUrl}${path}`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${CALLER}`,
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify(body),
        });

      const nativeResponses = await post(
        "/v1/responses",
        OVERFLOW_REQUEST_IDS.nativeResponses,
        {
          model: "buffer-fail-open-native-responses",
          input: "ordinary native responses prompt",
          stream: true,
        },
      );
      expect(nativeResponses.status).toBe(200);
      const nativeResponsesWire = await nativeResponses.text();
      expect(nativeResponsesWire).toContain(OVERFLOW_MARKERS.nativeResponses);
      expect(nativeResponsesWire).not.toContain("content_filter");

      const bridgeResponses = await post(
        "/v1/responses",
        OVERFLOW_REQUEST_IDS.bridgeResponses,
        {
          model: "buffer-fail-open-bridge-responses",
          input: "ordinary bridge responses prompt",
          stream: true,
        },
      );
      expect(bridgeResponses.status).toBe(200);
      const bridgeResponsesWire = await bridgeResponses.text();
      expect(bridgeResponsesWire).toContain(OVERFLOW_MARKERS.bridgeResponses);
      expect(bridgeResponsesWire).not.toContain("content_filter");

      const postMessages = (requestId: string, model: string) =>
        fetch(`${app!.proxyUrl}/v1/messages`, {
          method: "POST",
          headers: {
            "x-api-key": CALLER,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify({
            model,
            messages: [{ role: "user", content: "ordinary messages prompt" }],
            max_tokens: 16,
            stream: true,
          }),
        });
      const nativeMessages = await postMessages(
        OVERFLOW_REQUEST_IDS.nativeMessages,
        "buffer-fail-open-native-messages",
      );
      expect(nativeMessages.status).toBe(200);
      const nativeMessagesWire = await nativeMessages.text();
      expect(nativeMessagesWire).toContain(OVERFLOW_MARKERS.nativeMessages);
      expect(nativeMessagesWire).not.toContain("content_filter");

      const bridgeMessages = await postMessages(
        OVERFLOW_REQUEST_IDS.bridgeMessages,
        "buffer-fail-open-bridge-messages",
      );
      expect(bridgeMessages.status).toBe(200);
      const bridgeMessagesWire = await bridgeMessages.text();
      expect(bridgeMessagesWire).toContain(OVERFLOW_MARKERS.bridgeMessages);
      expect(bridgeMessagesWire).not.toContain("content_filter");

      const completionsResponse = await post(
        "/v1/completions",
        OVERFLOW_REQUEST_IDS.completions,
        {
          model: "buffer-fail-open-completions",
          prompt: "ordinary completions prompt",
          max_tokens: 16,
          stream: true,
        },
      );
      expect(completionsResponse.status).toBe(200);
      const completionsWire = await completionsResponse.text();
      expect(completionsWire).toContain(OVERFLOW_MARKERS.completions);
      expect(completionsWire).not.toContain("content_filter");

      expect(overflowNativeResponses.receivedRequests.length).toBe(
        baselines.nativeResponses + 1,
      );
      expect(overflowBridgeResponses.receivedRequests.length).toBe(
        baselines.bridgeResponses + 1,
      );
      expect(overflowNativeMessages.receivedRequests.length).toBe(
        baselines.nativeMessages + 1,
      );
      expect(overflowBridgeMessages.receivedRequests.length).toBe(
        baselines.bridgeMessages + 1,
      );
      expect(overflowCompletions.receivedRequests.length).toBe(
        baselines.completions + 1,
      );

      for (const requestId of Object.values(OVERFLOW_REQUEST_IDS)) {
        await waitForToken(sls, LOGSTORE, requestId, 15_000, beforeLogs);
      }
      const exported = decodedTextFor(sls, LOGSTORE, beforeLogs);
      for (const requestId of Object.values(OVERFLOW_REQUEST_IDS)) {
        expect(exported).toContain(requestId);
      }
      for (const marker of Object.values(OVERFLOW_MARKERS)) {
        expect(exported).not.toContain(marker);
      }
    },
    90_000,
  );

  test(
    "releases fail-open requests but omits uninspected input from full-content logs",
    async (ctx) => {
      if (!etcdReachable || !app || !sls || !bedrock) {
        ctx.skip();
        return;
      }
      const before = sls.requests.length;
      const post = (path: string, requestId: string, body: unknown) =>
        fetch(`${app!.proxyUrl}${path}`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${CALLER}`,
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify(body),
        });

      const chatResponse = await post(
        "/v1/chat/completions",
        INPUT_REQUEST_IDS.chat,
        {
          model: "fail-open-chat",
          stream: true,
          messages: [{ role: "user", content: INPUTS.chat }],
        },
      );
      expect(chatResponse.status).toBe(200);
      expect(await chatResponse.text()).toContain(OUTPUTS.chat);

      const nativeResponse = await post(
        "/v1/responses",
        INPUT_REQUEST_IDS.native,
        {
          model: "fail-open-native-responses",
          input: INPUTS.native,
          stream: true,
        },
      );
      expect(nativeResponse.status).toBe(200);
      expect(await nativeResponse.text()).toContain(OUTPUTS.native);

      const bridgeResponse = await post(
        "/v1/responses",
        INPUT_REQUEST_IDS.bridge,
        {
          model: "fail-open-bridge-responses",
          input: INPUTS.bridge,
          stream: true,
        },
      );
      expect(bridgeResponse.status).toBe(200);
      expect(await bridgeResponse.text()).toContain(OUTPUTS.bridge);

      for (const [requestId, model, input, output] of [
        [INPUT_REQUEST_IDS.messages, "fail-open-messages", INPUTS.messages, OUTPUTS.messages],
        [
          INPUT_REQUEST_IDS.messagesBridge,
          "fail-open-bridge-messages",
          INPUTS.messagesBridge,
          OUTPUTS.messagesBridge,
        ],
      ] as const) {
        const response = await fetch(`${app.proxyUrl}/v1/messages`, {
          method: "POST",
          headers: {
            "x-api-key": CALLER,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify({
            model,
            messages: [{ role: "user", content: input }],
            max_tokens: 16,
            stream: true,
          }),
        });
        expect(response.status).toBe(200);
        expect(await response.text()).toContain(output);
      }

      const completionsResponse = await post(
        "/v1/completions",
        INPUT_REQUEST_IDS.completions,
        {
          model: "fail-open-completions",
          prompt: INPUTS.completions,
          max_tokens: 16,
          stream: true,
        },
      );
      expect(completionsResponse.status).toBe(200);
      expect(await completionsResponse.text()).toContain(OUTPUTS.completions);

      const guardrailRequests = bedrock.requestBodies.join("\n");
      for (const input of Object.values(INPUTS)) {
        expect(guardrailRequests).toContain(input);
      }
      for (const requestId of Object.values(INPUT_REQUEST_IDS)) {
        await waitForToken(sls, LOGSTORE, requestId, 15_000, before);
      }
      const exported = decodedTextFor(sls, LOGSTORE, before);
      for (const requestId of Object.values(INPUT_REQUEST_IDS)) {
        expect(exported).toContain(requestId);
      }
      for (const input of Object.values(INPUTS)) {
        expect(exported).not.toContain(input);
      }
    },
    90_000,
  );
});
