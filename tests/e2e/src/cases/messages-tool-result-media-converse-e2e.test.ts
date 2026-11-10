import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  eventStream,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an Anthropic Messages caller's `tool_result` that holds an image,
// sent to a model the gateway answers through another protocol.
//
// A Bedrock Converse `toolResult` carries images beside its text
// (https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ToolResultContentBlock.html,
// and Amazon Nova's tool-use guide), so a Nova target receives the image
// under the `toolUseId` it answers. A Chat Completions `tool` message is text
// only, so an OpenAI target receives the result's text and the request still
// succeeds.

const CALLER_PLAINTEXT = "sk-messages-tool-result-media";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

// The eight-byte PNG signature, base64: what the caller's tool returned.
const PNG = "iVBORw0KGgo=";

const CONVERSE_REPLY = {
  output: { message: { role: "assistant", content: [{ text: "A blank image." }] } },
  stopReason: "end_turn",
  usage: { inputTokens: 30, outputTokens: 4, totalTokens: 34 },
  metrics: { latencyMs: 5 },
};
const CONVERSE_STREAM = eventStream([
  ["messageStart", { role: "assistant" }],
  ["contentBlockDelta", { contentBlockIndex: 0, delta: { text: "A blank image." } }],
  ["contentBlockStop", { contentBlockIndex: 0 }],
  ["messageStop", { stopReason: "end_turn" }],
  [
    "metadata",
    { usage: { inputTokens: 30, outputTokens: 4, totalTokens: 34 }, metrics: { latencyMs: 5 } },
  ],
]);

function messagesBody(model: string, stream: boolean) {
  return {
    model,
    max_tokens: 64,
    stream,
    tools: [
      {
        name: "screenshot",
        description: "Capture the screen",
        input_schema: { type: "object", properties: {} },
      },
    ],
    messages: [
      { role: "user", content: "What is on my screen?" },
      {
        role: "assistant",
        content: [{ type: "tool_use", id: "toolu_shot_1", name: "screenshot", input: {} }],
      },
      {
        role: "user",
        content: [
          {
            type: "tool_result",
            tool_use_id: "toolu_shot_1",
            content: [
              { type: "text", text: "here is the capture" },
              { type: "image", source: { type: "base64", media_type: "image/png", data: PNG } },
            ],
          },
        ],
      },
    ],
  };
}

describe("Messages → another protocol: a tool result's image", () => {
  let app: SpawnedApp | undefined;
  let bedrock: Server | undefined;
  let openai: OpenAiUpstream | undefined;
  const received: Array<{ path: string; body: any }> = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    bedrock = createServer((req, res) => {
      let raw = "";
      req.on("data", (c) => (raw += c));
      req.on("end", () => {
        received.push({ path: req.url ?? "", body: JSON.parse(raw || "{}") });
        if (req.url?.endsWith("/converse-stream")) {
          res.writeHead(200, { "content-type": "application/vnd.amazon.eventstream" });
          res.end(CONVERSE_STREAM);
        } else {
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify(CONVERSE_REPLY));
        }
      });
    });
    await new Promise<void>((resolve) => bedrock!.listen(0, "127.0.0.1", resolve));
    const addr = bedrock.address();
    if (addr === null || typeof addr === "string") throw new Error("no port");

    openai = await startOpenAiUpstream({
      nonStreamBody: {
        id: "chatcmpl-tool-media",
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: "gpt-4o",
        choices: [
          { index: 0, message: { role: "assistant", content: "A capture." }, finish_reason: "stop" },
        ],
        usage: { prompt_tokens: 30, completion_tokens: 3, total_tokens: 33 },
      },
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const bedrockPk = await seed.createProviderKey({
      display_name: "tool-media-bedrock-pk",
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({
        access_key_id: "AKIA-tool-result-media",
        secret_access_key: "sk-tool-result-media",
        region: "us-east-1",
      }),
      api_base: `http://127.0.0.1:${addr.port}`,
    });
    await seed.createModel({
      display_name: "tool-media-nova",
      provider: "bedrock",
      model_name: "us.amazon.nova-lite-v1:0",
      provider_key_id: bedrockPk.id,
    });
    const openaiPk = await seed.createProviderKey({
      display_name: "tool-media-openai-pk",
      secret: "sk-openai-mock",
      api_base: `${openai.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "tool-media-openai",
      provider: "openai",
      model_name: "gpt-4o",
      provider_key_id: openaiPk.id,
    });
    // Seeded last, so this key authenticating implies the whole seed set
    // has reached the gateway's snapshot.
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      await res.text();
      return res.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await openai?.close();
    await new Promise<void>((resolve) => (bedrock ? bedrock.close(() => resolve()) : resolve()));
  });

  test.for([false, true])("Nova stream=%s receives the image inside its toolResult", async (stream, ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const before = received.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "x-api-key": CALLER_PLAINTEXT, "content-type": "application/json" },
      body: JSON.stringify(messagesBody("tool-media-nova", stream)),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    expect(text).toContain("A blank image.");

    const sent = received.slice(before);
    expect(sent).toHaveLength(1);
    expect(sent[0].path).toMatch(stream ? /\/converse-stream$/ : /\/converse$/);
    const results = sent[0].body.messages
      .flatMap((m: any) => m.content)
      .filter((b: any) => b.toolResult)
      .map((b: any) => b.toolResult);
    expect(results).toHaveLength(1);
    expect(results[0].toolUseId).toBe("toolu_shot_1");
    expect(results[0].content).toEqual([
      { text: "here is the capture" },
      { image: { format: "png", source: { bytes: PNG } } },
    ]);
  });

  test("an OpenAI target receives the result's text and still answers", async (ctx) => {
    if (!etcdReachable || !app || !openai) return ctx.skip();
    const before = openai.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "x-api-key": CALLER_PLAINTEXT, "content-type": "application/json" },
      body: JSON.stringify(messagesBody("tool-media-openai", false)),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);

    const call = openai.receivedRequests
      .slice(before)
      .find((r) => r.path === "/v1/chat/completions");
    expect(call).toBeDefined();
    const toolMessages = JSON.parse(call!.body).messages.filter((m: any) => m.role === "tool");
    expect(toolMessages).toHaveLength(1);
    expect(toolMessages[0].tool_call_id).toBe("toolu_shot_1");
    expect(toolMessages[0].content).toBe("here is the capture");
  });
});
