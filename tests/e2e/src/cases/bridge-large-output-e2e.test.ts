import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// A translated stream has to keep what its closing event repeats, with no
// guardrail involved: `response.completed` carries the whole output on bridged
// /v1/responses, and /v1/messages holds a tool call's arguments until it can
// close the block. That is bounded, so an upstream cannot grow it without limit,
// but the bound is far above any ordinary answer: one long answer succeeds, and
// only a stream past 32 MiB fails, saying why.

const CALLER = "sk-bridge-large-output";
const CREDENTIAL_REF = "mock";
const LOGSTORE = "bridge-large-output";
const hash = (s: string) => createHash("sha256").update(s).digest("hex");

const PIECE = "z".repeat(256 * 1024);
const piecesFor = (mib: number) => Array.from({ length: mib * 4 }, () => PIECE);

const chatChunk = (delta: Record<string, unknown>, finish: string | null = null) =>
  JSON.stringify({
    id: "chatcmpl-large",
    object: "chat.completion.chunk",
    created: 1,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta, finish_reason: finish }],
  });
const chatEnd = [
  JSON.stringify({
    id: "chatcmpl-large",
    object: "chat.completion.chunk",
    created: 1,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
    usage: { prompt_tokens: 5, completion_tokens: 12, total_tokens: 17 },
  }),
  "[DONE]",
];
const textStream = (mib: number) => [
  chatChunk({ role: "assistant" }),
  ...piecesFor(mib).map((content) => chatChunk({ content })),
  ...chatEnd,
];
const toolStream = (mib: number) => [
  chatChunk({
    role: "assistant",
    tool_calls: [{ index: 0, id: "call_large", type: "function", function: { name: "lookup", arguments: "" } }],
  }),
  ...piecesFor(mib).map((args) => chatChunk({ tool_calls: [{ index: 0, function: { arguments: args } }] })),
  ...chatEnd,
];

describe("a translated stream is bounded far above an ordinary answer", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  const upstreams: OpenAiUpstream[] = [];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "bridge-large-output",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });
    const models: string[] = [];
    for (const [display, events, apis] of [
      // `apis: {}` declares no `/v1/responses`, which puts that route on the bridge.
      ["large-responses-text", textStream(2), {}],
      ["huge-responses-text", textStream(33), {}],
      ["large-messages-tool", toolStream(2), undefined],
    ] as const) {
      const upstream = await startOpenAiUpstream({ streamEvents: [...events] });
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `${display}-pk`,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
        ...(apis ? { apis } : {}),
      });
      await seed.createModel({
        display_name: display,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
      models.push(display);
    }
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: models });
    await waitConfigPropagation(
      async () => (await new ProxyClient(app!.proxyUrl, CALLER).listModels()).status === 200,
    );
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  const post = (path: string, body: Record<string, unknown>) =>
    fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${CALLER}`,
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({ ...body, stream: true }),
    });

  test("/v1/responses: a 2 MiB answer completes", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await post("/v1/responses", { model: "large-responses-text", input: "go" });
    expect(res.status).toBe(200);
    const body = await res.text();
    expect(body).toContain("response.completed");
    expect(body).not.toContain("stream_aborted");
  }, 60_000);

  test("/v1/messages: a 2 MiB tool call completes", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await post("/v1/messages", {
      model: "large-messages-tool",
      max_tokens: 64,
      messages: [{ role: "user", content: "go" }],
    });
    expect(res.status).toBe(200);
    const body = await res.text();
    expect(body).toContain("message_stop");
    expect(body).not.toContain("stream_aborted");
  }, 60_000);

  test("/v1/responses: past 32 MiB the stream fails and says why", async (ctx) => {
    if (!etcdReachable || !app || !sls) return ctx.skip();
    const res = await post("/v1/responses", { model: "huge-responses-text", input: "go" });
    expect(res.status).toBe(200);
    const body = await res.text();
    expect(body).toContain("response.failed");
    expect(body).toContain("stream_aborted");
    expect(body).toContain("32 MiB");
    expect(body).not.toContain("response.completed");

    const row = await waitForSlsLog(
      sls,
      LOGSTORE,
      (log) => log.get("requested_model") === "huge-responses-text",
      "the failed row",
    );
    expect(Number(row.get("status_code"))).toBeGreaterThanOrEqual(500);
    expect(row.get("error_class")).toBe("stream_aborted");
    expect(row.get("error_message")).toContain("32 MiB");
  }, 120_000);
});
