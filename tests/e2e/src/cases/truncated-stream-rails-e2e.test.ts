import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  eventStream,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an upstream body that closes cleanly before the answer's terminal is a
// failed request, on every upstream rail and every client protocol. Half an
// answer must not read as a finished one: not `[DONE]`, not `end_turn`, not
// `response.completed`, and not a 200 row.

const CALLER = "sk-truncated-stream-rails";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const CREDENTIAL_REF = "e2e";
const LOGSTORE = "truncated-stream-rails-store";
const PARTIAL = "half an ans";
/** A prompt carrying this gets the whole answer; every other gets it cut off. */
const WHOLE = "answer in full";
/** This one gets the answer through its finish, then the connection drops. */
const DROPPED = "answer then drop";

const sse = (...frames: unknown[]) => frames.map((f) => `data: ${JSON.stringify(f)}\n\n`).join("");
const anthropic = (type: string, rest: Record<string, unknown>) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...rest })}\n\n`;
const openaiPiece = sse({
  id: "c",
  model: "m",
  choices: [{ index: 0, delta: { role: "assistant", content: PARTIAL } }],
});
const openaiEnd = `${sse({ id: "c", model: "m", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] })}data: [DONE]\n\n`;

interface Reply {
  contentType: string;
  body: string | Buffer;
}

function reply(path: string, whole: boolean): Reply {
  const text = (body: string): Reply => ({ contentType: "text/event-stream", body });
  if (path.includes(":streamGenerateContent")) {
    const piece = sse({ candidates: [{ content: { role: "model", parts: [{ text: PARTIAL }] } }] });
    const end = sse({ candidates: [{ content: { role: "model", parts: [{ text: "." }] }, finishReason: "STOP" }] });
    return text(whole ? piece + end : piece);
  }
  if (path.includes("publishers/anthropic") && path.includes(":streamRawPredict")) {
    const piece =
      anthropic("message_start", {
        message: { id: "m", type: "message", role: "assistant", model: "c", content: [], usage: { input_tokens: 3, output_tokens: 0 } },
      }) +
      anthropic("content_block_start", { index: 0, content_block: { type: "text", text: "" } }) +
      anthropic("content_block_delta", { index: 0, delta: { type: "text_delta", text: PARTIAL } });
    const end =
      anthropic("content_block_stop", { index: 0 }) +
      anthropic("message_delta", { delta: { stop_reason: "end_turn" }, usage: { output_tokens: 2 } }) +
      anthropic("message_stop", {});
    return text(whole ? piece + end : piece);
  }
  if (path.includes("/converse-stream")) {
    const events: [string, unknown][] = [
      ["messageStart", { role: "assistant" }],
      ["contentBlockDelta", { contentBlockIndex: 0, delta: { text: PARTIAL } }],
    ];
    if (whole) {
      events.push(
        ["contentBlockStop", { contentBlockIndex: 0 }],
        ["messageStop", { stopReason: "end_turn" }],
        ["metadata", { usage: { inputTokens: 3, outputTokens: 2, totalTokens: 5 }, metrics: { latencyMs: 1 } }],
      );
    }
    return { contentType: "application/vnd.amazon.eventstream", body: eventStream(events) };
  }
  return text(whole ? openaiPiece + openaiEnd : openaiPiece);
}

const RAILS: Record<string, { pk: Record<string, unknown>; provider: string; model_name: string }> = {
  "trunc-azure": {
    pk: { provider: "azure-openai", adapter: "azure-openai", secret: "azure-key" },
    provider: "azure-openai",
    model_name: "gpt4o-prod",
  },
  "trunc-vertex-gemini": {
    pk: {
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "t", project: "p", region: "us-east5" }),
    },
    provider: "google",
    model_name: "gemini-2.5-flash",
  },
  "trunc-vertex-claude": {
    pk: {
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "t", project: "p", region: "us-east5" }),
    },
    provider: "google",
    model_name: "claude-sonnet-4-5",
  },
  "trunc-vertex-maas": {
    pk: {
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "t", project: "p", region: "us-east5" }),
    },
    provider: "google",
    model_name: "meta/llama-3.3-70b-instruct-maas",
  },
  "trunc-vertex-mistral": {
    pk: {
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "t", project: "p", region: "us-east5" }),
    },
    provider: "google",
    model_name: "mistral-large-2411",
  },
  "trunc-bedrock-converse": {
    pk: {
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({ access_key_id: "AKIA", secret_access_key: "s", region: "us-west-2" }),
    },
    provider: "bedrock",
    model_name: "amazon.nova-lite-v1:0",
  },
  "trunc-bedrock-chat": {
    pk: {
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({ access_key_id: "AKIA", secret_access_key: "s", region: "us-west-2" }),
    },
    provider: "bedrock",
    model_name: "us.xai.grok-4",
  },
};

describe("a stream that closes before its terminal is a failure", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = createServer((req, res) => {
      const parts: Buffer[] = [];
      req.on("data", (c: Buffer) => parts.push(c));
      req.on("end", () => {
        const prompt = Buffer.concat(parts).toString("utf8");
        const dropped = prompt.includes(DROPPED);
        const answer = reply(decodeURIComponent(req.url ?? ""), dropped || prompt.includes(WHOLE));
        res.writeHead(200, { "content-type": answer.contentType });
        if (dropped) {
          const body = typeof answer.body === "string" ? answer.body.replace("data: [DONE]\n\n", "") : answer.body;
          res.write(body, () => setTimeout(() => res.socket?.destroy(), 20));
          return;
        }
        res.end(answer.body);
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const addr = upstream.address();
    if (addr === null || typeof addr === "string") throw new Error("no port");

    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-ak-id",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-ak-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "truncated-stream-rails-sls",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "truncated-stream-rails-proj",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
    });
    for (const [display_name, rail] of Object.entries(RAILS)) {
      const pk = await seed.createProviderKey({
        display_name: `${display_name}-pk`,
        api_base: `http://127.0.0.1:${addr.port}`,
        ...rail.pk,
      } as Parameters<typeof seed.createProviderKey>[0]);
      await seed.createModel({
        display_name,
        provider: rail.provider,
        model_name: rail.model_name,
        provider_key_id: pk.id,
      });
    }
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: Object.keys(RAILS) });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await new Promise<void>((resolve) => (upstream ? upstream.close(() => resolve()) : resolve()));
  });

  const FAMILIES = [
    {
      route: "/v1/chat/completions",
      body: (model: string, text: string) => ({ model, stream: true, messages: [{ role: "user", content: text }] }),
      finished: "[DONE]",
    },
    {
      route: "/v1/messages",
      body: (model: string, text: string) => ({
        model,
        stream: true,
        max_tokens: 64,
        messages: [{ role: "user", content: text }],
      }),
      finished: "message_stop",
    },
    {
      route: "/v1/responses",
      body: (model: string, text: string) => ({ model, stream: true, input: text }),
      finished: "response.completed",
    },
  ];

  async function send(route: string, body: unknown) {
    const res = await fetch(`${app!.proxyUrl}${route}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { raw: await res.text(), requestId: res.headers.get("x-aisix-request-id") ?? "" };
  }

  for (const model of Object.keys(RAILS)) {
    for (const { route, body, finished } of FAMILIES) {
      test(`${model} ${route}: cut off, it fails rather than finishes`, async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const out = await send(route, body(model, "cut this one off"));
        expect(out.raw, out.raw).not.toContain(finished);
        const row = await waitForSlsLog(
          sls!,
          LOGSTORE,
          (l) => l.get("request_id") === out.requestId,
          `row for ${out.requestId}`,
          15_000,
        );
        expect(Number(row.get("status_code")), JSON.stringify([...row])).toBeGreaterThanOrEqual(500);
      });

      test(`${model} ${route}: whole, it finishes`, async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const out = await send(route, body(model, WHOLE));
        expect(out.raw, out.raw).toContain(finished);
        const row = await waitForSlsLog(
          sls!,
          LOGSTORE,
          (l) => l.get("request_id") === out.requestId,
          `row for ${out.requestId}`,
          15_000,
        );
        expect(Number(row.get("status_code")), JSON.stringify([...row])).toBe(200);
      });

      test(`${model} ${route}: the connection dropped after its finish, it still finishes`, async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const out = await send(route, body(model, DROPPED));
        expect(out.raw, out.raw).toContain(finished);
        const row = await waitForSlsLog(
          sls!,
          LOGSTORE,
          (l) => l.get("request_id") === out.requestId,
          `row for ${out.requestId}`,
          15_000,
        );
        expect(Number(row.get("status_code")), JSON.stringify([...row])).toBe(200);
      });
    }
  }
});
