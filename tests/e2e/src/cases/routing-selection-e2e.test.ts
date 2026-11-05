import { createServer, type Server } from "node:http";
import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  AdminClient,
  EtcdClient,
  ProxyClient,
  SeedClient,
  pickFreePort,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type OpenAiUpstreamOptions,
  type SpawnedApp,
} from "../harness/index.js";

// Which upstream a request reaches, from a caller's and an operator's point
// of view: each case seeds a group, drives real requests through it, and
// reads the answer and what every mock upstream received.

const CALLER = "sk-routing-selection-caller";
const LIMITED_CALLER = "sk-routing-selection-limited";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

function chatBody(content: string) {
  return {
    id: `cmpl-${content}`,
    object: "chat.completion",
    created: 0,
    model: "gpt-4o-mini",
    choices: [{ index: 0, message: { role: "assistant", content }, finish_reason: "stop" }],
    usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
  };
}

function messagesBody(text: string) {
  return {
    id: `msg-${text}`,
    type: "message",
    role: "assistant",
    model: "claude",
    content: [{ type: "text", text }],
    stop_reason: "end_turn",
    usage: { input_tokens: 1, output_tokens: 1 },
  };
}

function responsesBody(text: string) {
  return {
    id: `resp-${text}`,
    object: "response",
    status: "completed",
    output: [
      {
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text }],
      },
    ],
    usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 },
  };
}

const err500 = {
  status: 500,
  errorBody: { error: { message: "boom", type: "server_error" } },
};

const ANTHROPIC_SSE = [
  'event: message_start\ndata: {"type":"message_start","message":{"id":"msg-1","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}\n\n',
  'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"anthropic stream served"}}\n\n',
  'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}\n\n',
  'event: message_stop\ndata: {"type":"message_stop"}\n\n',
];

const RESPONSES_SSE = [
  'data: {"type":"response.created","response":{"id":"resp-1","status":"in_progress"}}\n\n',
  'data: {"type":"response.output_text.delta","delta":"responses stream served"}\n\n',
  'data: {"type":"response.completed","response":{"id":"resp-1","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}\n\n',
  "data: [DONE]\n\n",
];

interface EmbeddingMock {
  baseUrl: string;
  /** The `input` of every request received. */
  batches: string[][];
  close(): Promise<void>;
}

/** Every text maps to one fixed vector, so whatever is asked matches route 0. */
async function startEmbeddingMock(): Promise<EmbeddingMock> {
  const batches: string[][] = [];
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      const parsed = JSON.parse(raw || "{}") as { input?: string | string[] };
      const inputs = Array.isArray(parsed.input) ? parsed.input : [parsed.input ?? ""];
      batches.push(inputs);
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          object: "list",
          model: "embed-mock",
          data: inputs.map((_, index) => ({
            object: "embedding",
            index,
            embedding: [1, 0],
          })),
          usage: { prompt_tokens: inputs.length, total_tokens: inputs.length },
        }),
      );
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    batches,
    async close() {
      server.closeAllConnections?.();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("routing selection e2e", () => {
  let app: SpawnedApp | undefined;
  let admin: AdminClient | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  const upstreams: OpenAiUpstream[] = [];
  const closers: Array<() => Promise<void>> = [];
  const up: Record<string, OpenAiUpstream> = {};
  const modelIds: Record<string, string> = {};
  let embedMock: EmbeddingMock | undefined;
  let limitEmbedMock: EmbeddingMock | undefined;
  let releaseOlderAnswer: () => void = () => {};

  async function upstream(name: string, opts: OpenAiUpstreamOptions): Promise<OpenAiUpstream> {
    const u = await startOpenAiUpstream(opts);
    upstreams.push(u);
    up[name] = u;
    return u;
  }

  /** A direct OpenAI-wire model behind its own mock and provider key. */
  async function member(
    name: string,
    opts: OpenAiUpstreamOptions,
    extra: Record<string, unknown> = {},
  ): Promise<void> {
    const u = await upstream(name, opts);
    const pk = await seed!.createProviderKey({
      display_name: `${name}-pk`,
      secret: "sk-mock",
      api_base: `${u.baseUrl}/v1`,
    });
    const m = await seed!.createModel({
      display_name: name,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
      ...extra,
    });
    modelIds[name] = m.id;
  }

  /** A direct Anthropic-wire model behind its own mock and provider key. */
  async function anthropicMember(
    name: string,
    opts: OpenAiUpstreamOptions,
    extra: Record<string, unknown> = {},
  ): Promise<void> {
    const u = await upstream(name, opts);
    const pk = await seed!.createProviderKey({
      display_name: `${name}-pk`,
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: u.baseUrl,
    });
    await seed!.createModel({
      display_name: name,
      provider: "anthropic",
      model_name: "claude",
      provider_key_id: pk.id,
      ...extra,
    });
  }

  const group = (display_name: string, routing: Record<string, unknown>) =>
    seed!.createModel({ display_name, routing });

  async function post(
    path: string,
    body: unknown,
    headers: Record<string, string> = {},
    key = CALLER,
  ): Promise<Response> {
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "content-type": "application/json",
        ...headers,
      },
      body: JSON.stringify(body),
    });
  }

  const chat = (model: string, headers: Record<string, string> = {}) =>
    post(
      "/v1/chat/completions",
      { model, messages: [{ role: "user", content: "hello" }] },
      headers,
    );

  const answerOf = async (res: Response): Promise<string> => {
    const body = (await res.json()) as {
      choices?: Array<{ message?: { content?: string } }>;
    };
    return body.choices?.[0]?.message?.content ?? "";
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    app = await spawnApp({ admin: false });
    admin = new AdminClient(app.adminUrl, app.adminKey, app.metricsUrl);
    seed = new SeedClient(etcd, app.etcdPrefix);

    // ── cooling targets and the fallback budget ──
    await member("cb-a", err500, { retries: 0, cooldown: { enabled: true, default_seconds: 300 } });
    await member("cb-b", err500, { retries: 0, cooldown: { enabled: true, default_seconds: 300 } });
    await member("cb-c", { nonStreamBody: chatBody("served by c") });
    await group("cb-group", {
      strategy: "failover",
      targets: [{ model: "cb-a" }, { model: "cb-b" }, { model: "cb-c" }],
      max_fallbacks: 1,
    });

    await member("tier-a", err500, { retries: 0, cooldown: { enabled: true, default_seconds: 300 } });
    await member("tier-backup", { nonStreamBody: chatBody("served by backup") });
    await group("tier-group", {
      strategy: "failover",
      targets: [{ model: "tier-a" }, { model: "tier-backup", priority: -1 }],
      max_fallbacks: 0,
    });

    // ── interleaved callers keep round robin going ──
    for (const name of ["rr-eu1", "rr-eu2", "rr-us1", "rr-us2"]) {
      await member(name, { nonStreamBody: chatBody(name) });
    }
    await group("rr-group", {
      strategy: "round_robin",
      targets: [
        { model: "rr-eu1", tags: ["eu"] },
        { model: "rr-eu2", tags: ["eu"] },
        { model: "rr-us1", tags: ["us"] },
        { model: "rr-us2", tags: ["us"] },
      ],
    });

    // ── a drained target ──
    await member("w-live", { nonStreamBody: chatBody("w-live") });
    await member("w-drained", { nonStreamBody: chatBody("w-drained") });
    await group("w-group", {
      strategy: "round_robin",
      targets: [
        { model: "w-live", weight: 1 },
        { model: "w-drained", weight: 0 },
      ],
    });

    // ── a member deleted while the group still names it ──
    await member("del-survivor", { nonStreamBody: chatBody("del-survivor") });
    await group("del-group", {
      strategy: "failover",
      targets: [{ model: "del-secret-member" }, { model: "del-survivor" }],
    });
    await group("del-empty", {
      strategy: "failover",
      targets: [{ model: "del-secret-member" }],
    });

    // ── a member whose own configuration cannot be used ──
    const liveEverywhere = await upstream("uu-live", {
      pathBodies: {
        "/v1/chat/completions": chatBody("uu-live"),
        "/v1/responses": responsesBody("uu-live"),
        "/v1/embeddings": {
          object: "list",
          model: "m",
          data: [{ object: "embedding", index: 0, embedding: [1, 0] }],
          usage: { prompt_tokens: 1, total_tokens: 1 },
        },
      },
    });
    const liveEverywherePk = await seed.createProviderKey({
      display_name: "uu-live-pk",
      secret: "sk-mock",
      api_base: `${liveEverywhere.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "uu-live",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: liveEverywherePk.id,
    });
    await seed.createModel({
      display_name: "uu-no-key",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: "no-such-provider-key",
    });
    const badSecretPk = await seed.createProviderKey({
      display_name: "uu-bad-secret-pk",
      secret: "sk-live\n-injected",
      api_base: `${liveEverywhere.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "uu-bad-secret",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: badSecretPk.id,
    });
    await group("uu-no-key-group", {
      strategy: "failover",
      targets: [{ model: "uu-no-key" }, { model: "uu-live" }],
    });
    await group("uu-bad-secret-group", {
      strategy: "failover",
      targets: [{ model: "uu-bad-secret" }, { model: "uu-live" }],
    });
    await group("uu-only-no-key", {
      strategy: "failover",
      targets: [{ model: "uu-no-key" }],
    });
    await anthropicMember("uu-live-anthropic", { nonStreamBody: messagesBody("uu-live-anthropic") });
    await seed.createModel({
      display_name: "uu-no-key-anthropic",
      provider: "anthropic",
      model_name: "claude",
      provider_key_id: "no-such-provider-key",
    });
    await group("uu-no-key-messages", {
      strategy: "failover",
      targets: [{ model: "uu-no-key-anthropic" }, { model: "uu-live-anthropic" }],
    });
    await group("uu-only-no-key-messages", {
      strategy: "failover",
      targets: [{ model: "uu-no-key-anthropic" }],
    });

    // ── least_latency and a target that stopped answering ──
    await member("lat-broken", err500, { retries: 0 });
    await member("lat-ok", { nonStreamBody: chatBody("lat-ok"), responseDelayMs: 50 });
    await group("lat-group", {
      strategy: "least_latency",
      targets: [{ model: "lat-broken" }, { model: "lat-ok" }],
    });

    // ── a cooldown and the request that started before it ──
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    releaseOlderAnswer = release;
    await member(
      "older-target",
      {
        scriptedResponses: [
          { responseGate: gate, nonStreamBody: chatBody("older answer") },
          {
            status: 429,
            errorBody: { error: { message: "slow down", type: "rate_limit_error" } },
          },
        ],
        nonStreamBody: chatBody("newer answer"),
      },
      { cooldown: { enabled: true, default_seconds: 300 } },
    );

    // ── least_busy while a stream waits for its head ──
    const sse = [
      JSON.stringify({
        id: "s",
        object: "chat.completion.chunk",
        model: "gpt-4o-mini",
        choices: [{ index: 0, delta: { content: "hi" }, finish_reason: "stop" }],
      }),
      "[DONE]",
    ];
    await member("busy-slow", { streamEvents: sse, responseDelayMs: 1500 });
    await member("busy-idle", { streamEvents: sse });
    await group("busy-group", {
      strategy: "least_busy",
      targets: [{ model: "busy-slow" }, { model: "busy-idle" }],
    });

    // ── a same-target retry and the member's request limit ──
    const flaky = (body: Record<string, unknown>): OpenAiUpstreamOptions => ({
      scriptedResponses: [err500],
      ...body,
    });
    const once = { rate_limit: { rpm: 1 } };
    await member("rl-chat", flaky({ nonStreamBody: chatBody("rl-chat") }), once);
    await member(
      "rl-chat-stream",
      flaky({
        streamEvents: [
          JSON.stringify({
            id: "s",
            object: "chat.completion.chunk",
            model: "gpt-4o-mini",
            choices: [{ index: 0, delta: { content: "rl" }, finish_reason: "stop" }],
          }),
          "[DONE]",
        ],
      }),
      once,
    );
    await member("rl-responses", flaky({ nonStreamBody: responsesBody("rl-responses") }), once);
    await anthropicMember("rl-messages", flaky({ nonStreamBody: messagesBody("rl-messages") }), once);
    for (const name of ["rl-chat", "rl-chat-stream", "rl-responses", "rl-messages"]) {
      await group(`${name}-group`, {
        strategy: "failover",
        targets: [{ model: name }],
        retries: 1,
      });
    }

    // ── a failure reported as a stream's first frame ──
    const firstFrameBudget = { stream_timeout: 5000 };
    await anthropicMember(
      "ff-messages-bad",
      {
        rawSseChunks: [
          'event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"provider overloaded"}}\n\n',
        ],
      },
      { retries: 0, ...firstFrameBudget },
    );
    await anthropicMember(
      "ff-messages-ok",
      { rawSseChunks: ANTHROPIC_SSE },
      firstFrameBudget,
    );
    await group("ff-messages", {
      strategy: "failover",
      targets: [{ model: "ff-messages-bad" }, { model: "ff-messages-ok" }],
    });
    await member(
      "ff-responses-bad",
      {
        rawSseChunks: [
          'data: {"type":"error","code":"server_error","message":"provider overloaded"}\n\n',
        ],
      },
      { retries: 0, ...firstFrameBudget },
    );
    await member("ff-responses-ok", { rawSseChunks: RESPONSES_SSE }, firstFrameBudget);
    await group("ff-responses", {
      strategy: "failover",
      targets: [{ model: "ff-responses-bad" }, { model: "ff-responses-ok" }],
    });

    // ── token counting on a group only part of which can answer it ──
    await member("ct-openai", { nonStreamBody: chatBody("ct-openai") });
    await anthropicMember("ct-anthropic", {
      nonStreamBody: { input_tokens: 7 },
      pathBodies: { "/v1/messages": messagesBody("ct-anthropic") },
    });
    await group("ct-mixed", {
      strategy: "round_robin",
      targets: [{ model: "ct-openai" }, { model: "ct-anthropic" }],
      max_fallbacks: 0,
    });
    await anthropicMember("ct-rr-a", {
      nonStreamBody: { input_tokens: 7 },
      pathBodies: { "/v1/messages": messagesBody("ct-rr-a") },
    });
    await anthropicMember("ct-rr-b", {
      nonStreamBody: { input_tokens: 7 },
      pathBodies: { "/v1/messages": messagesBody("ct-rr-b") },
    });
    await group("ct-rr", {
      strategy: "round_robin",
      targets: [{ model: "ct-rr-a" }, { model: "ct-rr-b" }],
    });

    // ── semantic routers ──
    embedMock = await startEmbeddingMock();
    limitEmbedMock = await startEmbeddingMock();
    closers.push(embedMock.close, limitEmbedMock.close);
    await member("sem-served", { nonStreamBody: chatBody("sem-served") });
    await member("sem-default", { nonStreamBody: chatBody("sem-default") });
    const embedding = async (name: string, mock: EmbeddingMock) => {
      const pk = await seed!.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: `${mock.baseUrl}/v1`,
      });
      const doc = {
        display_name: name,
        provider: "openai",
        model_name: "embed-old",
        provider_key_id: pk.id,
        embedding: { dimensions: 2, normalize: true },
      };
      const m = await seed!.createModel(doc);
      modelIds[name] = m.id;
      return doc;
    };
    embedDoc = await embedding("sem-embed", embedMock);
    await embedding("sem-limit-embed", limitEmbedMock);
    const router = (name: string, embedding_model: string, target: string) =>
      seed!.createModel({
        display_name: name,
        semantic: {
          embedding_model,
          routes: [
            { name: "only", target, examples: ["an example utterance"], threshold: 0.5 },
          ],
          default: "sem-default",
          match: { threshold: 0.5 },
        },
      });
    await router("sem-router", "sem-embed", "sem-served");
    await router("sem-limited", "sem-limit-embed", "sem-served");
    await router("sem-dangling", "sem-embed", "sem-deleted-target");

    // Last, so a gate on these keys implies every resource above.
    await seed.createApiKey({
      key_hash: sha256(LIMITED_CALLER),
      allowed_models: ["sem-limited"],
      rate_limit: { rpm: 1 },
    });
    await seed.createApiKey({ key_hash: sha256(CALLER), allowed_models: ["*"] });
    await waitConfigPropagation(
      async () => (await new ProxyClient(app!.proxyUrl, CALLER).listModels()).status === 200,
    );
  });

  let embedDoc: Record<string, unknown> = {};

  afterAll(async () => {
    releaseOlderAnswer();
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
    await Promise.all(closers.map((c) => c()));
  });

  test("cooling targets do not use up max_fallbacks, so a healthy target is still reached", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    // The first request finds a and b broken; its cap ends the walk there.
    expect((await chat("cb-group")).status).toBeGreaterThanOrEqual(500);
    expect(up["cb-c"]!.receivedRequests).toHaveLength(0);

    const a = up["cb-a"]!.receivedRequests.length;
    const b = up["cb-b"]!.receivedRequests.length;
    const res = await chat("cb-group");
    expect(res.status).toBe(200);
    expect(await answerOf(res)).toBe("served by c");
    expect(up["cb-a"]!.receivedRequests.length).toBe(a);
    expect(up["cb-b"]!.receivedRequests.length).toBe(b);
  });

  test("a cooling tier leaves the backup tier reachable under max_fallbacks: 0", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expect((await chat("tier-group")).status).toBeGreaterThanOrEqual(500);
    const res = await chat("tier-group");
    expect(res.status).toBe(200);
    expect(await answerOf(res)).toBe("served by backup");
  });

  test("callers with different routing tags each keep their own round robin going", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (let i = 0; i < 8; i++) {
      const res = await chat("rr-group", {
        "x-aisix-routing-tags": i % 2 === 0 ? "eu" : "us",
      });
      expect(res.status).toBe(200);
    }
    for (const name of ["rr-eu1", "rr-eu2", "rr-us1", "rr-us2"]) {
      expect(up[name]!.receivedRequests, name).toHaveLength(2);
    }
  });

  test("a target at weight 0 gets no traffic", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (let i = 0; i < 6; i++) {
      const res = await chat("w-group");
      expect(res.status).toBe(200);
      expect(await answerOf(res)).toBe("w-live");
    }
    expect(up["w-drained"]!.receivedRequests).toHaveLength(0);
  });

  test("a deleted member costs the group that member, and no refusal names it", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const served = await chat("del-group");
    expect(served.status).toBe(200);
    expect(await answerOf(served)).toBe("del-survivor");

    const refused = await chat("del-empty");
    expect(refused.status).toBe(400);
    expect(await refused.text()).not.toContain("del-secret-member");
  });

  test("a member whose own configuration is unusable fails over on every endpoint", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const cases: Array<[string, Response]> = [
      ["chat, no provider key", await chat("uu-no-key-group")],
      ["chat, unusable secret", await chat("uu-bad-secret-group")],
      [
        "responses",
        await post("/v1/responses", { model: "uu-no-key-group", input: "hello" }),
      ],
      [
        "embeddings",
        await post("/v1/embeddings", { model: "uu-no-key-group", input: "hello" }),
      ],
      [
        "messages",
        await post("/v1/messages", {
          model: "uu-no-key-messages",
          max_tokens: 8,
          messages: [{ role: "user", content: "hello" }],
        }),
      ],
    ];
    for (const [name, res] of cases) {
      expect(res.status, `${name}: ${await res.clone().text()}`).toBe(200);
    }
  });

  test("a group with no usable member is refused without naming a member or its key", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const hello = [{ role: "user", content: "hello" }];
    const cases: Array<[string, Response]> = [
      ["chat", await chat("uu-only-no-key")],
      ["responses", await post("/v1/responses", { model: "uu-only-no-key", input: "hello" })],
      ["embeddings", await post("/v1/embeddings", { model: "uu-only-no-key", input: "hello" })],
      [
        "messages",
        await post("/v1/messages", {
          model: "uu-only-no-key-messages",
          max_tokens: 8,
          messages: hello,
        }),
      ],
      [
        "count_tokens",
        await post("/v1/messages/count_tokens", {
          model: "uu-only-no-key-messages",
          messages: hello,
        }),
      ],
    ];
    for (const [name, res] of cases) {
      const text = await res.text();
      expect(res.status, `${name}: ${text}`).toBe(400);
      expect(text, name).not.toContain("uu-no-key");
      expect(text, name).not.toContain("no-such-provider-key");
    }
  });

  test("least_latency stops trying a target first once it has failed", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    // With no latency on record the broken target is probed first, and the
    // request falls over to the working one.
    const first = await chat("lat-group");
    expect(first.status).toBe(200);
    expect(await answerOf(first)).toBe("lat-ok");
    const probes = up["lat-broken"]!.receivedRequests.length;
    expect(probes).toBeGreaterThan(0);

    for (let i = 0; i < 4; i++) {
      const res = await chat("lat-group");
      expect(res.status).toBe(200);
    }
    expect(up["lat-broken"]!.receivedRequests.length).toBe(probes);
  });

  test("a request that began before a cooldown does not end it by succeeding", async (ctx) => {
    if (!etcdReachable || !app || !admin) return ctx.skip();
    const older = chat("older-target");
    await expect
      .poll(() => up["older-target"]!.receivedRequests.length, { timeout: 5000 })
      .toBe(1);
    // A later request is refused and cools the target.
    expect((await chat("older-target")).status).toBe(429);
    const statusOf = async () =>
      (await admin!.listModelStatuses()).find((r) => r.id === modelIds["older-target"])?.status;
    expect(await statusOf()).toBe("cooldown");

    releaseOlderAnswer();
    const res = await older;
    expect(res.status).toBe(200);
    expect(await answerOf(res)).toBe("older answer");
    expect(await statusOf()).toBe("cooldown");
  });

  test("least_busy counts a stream from when it is sent, not from when it answers", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const stream = (content: string) =>
      post("/v1/chat/completions", {
        model: "busy-group",
        stream: true,
        messages: [{ role: "user", content }],
      });
    // Both idle: the first goes to the first declared target and waits for
    // its response head there.
    const first = stream("first");
    await expect
      .poll(() => up["busy-slow"]!.receivedRequests.length, { timeout: 5000 })
      .toBe(1);
    const second = await stream("second");
    expect(second.status).toBe(200);
    await second.text();
    expect(up["busy-idle"]!.receivedRequests).toHaveLength(1);
    expect(up["busy-slow"]!.receivedRequests).toHaveLength(1);
    await (await first).text();
  });

  test("a same-target retry is not charged against the member's request limit", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const hello = [{ role: "user", content: "hello" }];
    const cases: Array<[string, () => Promise<Response>]> = [
      ["chat", () => chat("rl-chat-group")],
      [
        "chat stream",
        () =>
          post("/v1/chat/completions", {
            model: "rl-chat-stream-group",
            stream: true,
            messages: hello,
          }),
      ],
      ["responses", () => post("/v1/responses", { model: "rl-responses-group", input: "hello" })],
      [
        "messages",
        () =>
          post("/v1/messages", { model: "rl-messages-group", max_tokens: 8, messages: hello }),
      ],
    ];
    for (const [name, send] of cases) {
      const res = await send();
      const text = await res.text();
      expect(res.status, `${name}: ${text}`).toBe(200);
    }
    // The first attempt failed and the retry answered, one request's worth.
    for (const name of ["rl-chat", "rl-chat-stream", "rl-responses", "rl-messages"]) {
      expect(up[name]!.receivedRequests, name).toHaveLength(2);
    }
  });

  test("a failure reported as a stream's first frame fails over like a failed status", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const messages = await post("/v1/messages", {
      model: "ff-messages",
      stream: true,
      max_tokens: 8,
      messages: [{ role: "user", content: "hello" }],
    });
    expect(messages.status).toBe(200);
    const messagesText = await messages.text();
    expect(messagesText).toContain("anthropic stream served");
    expect(messagesText).not.toContain("overloaded_error");

    const responses = await post("/v1/responses", {
      model: "ff-responses",
      stream: true,
      input: "hello",
    });
    expect(responses.status).toBe(200);
    const responsesText = await responses.text();
    expect(responsesText).toContain("responses stream served");
    expect(responsesText).not.toContain("provider overloaded");
  });

  test("token counting reaches the target that can answer it, however the group is capped", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (let i = 0; i < 4; i++) {
      const res = await post("/v1/messages/count_tokens", {
        model: "ct-mixed",
        messages: [{ role: "user", content: "hello" }],
      });
      expect(res.status, await res.clone().text()).toBe(200);
    }
    expect(up["ct-openai"]!.receivedRequests).toHaveLength(0);
  });

  test("a client alternating token counting with messages still spreads its messages", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const hello = [{ role: "user", content: "hello" }];
    for (let i = 0; i < 4; i++) {
      await post("/v1/messages/count_tokens", { model: "ct-rr", messages: hello });
      await post("/v1/messages", { model: "ct-rr", max_tokens: 8, messages: hello });
    }
    const generations = (name: string) =>
      up[name]!.receivedRequests.filter((r) => r.path === "/v1/messages").length;
    expect(generations("ct-rr-a")).toBe(2);
    expect(generations("ct-rr-b")).toBe(2);
  });

  test("a caller over its rate limit costs a semantic router no embedding call", async (ctx) => {
    if (!etcdReachable || !app || !limitEmbedMock) return ctx.skip();
    const ask = () =>
      post(
        "/v1/chat/completions",
        { model: "sem-limited", messages: [{ role: "user", content: "hello" }] },
        {},
        LIMITED_CALLER,
      );
    await waitConfigPropagation(
      async () =>
        (await new ProxyClient(app!.proxyUrl, LIMITED_CALLER).listModels()).status === 200,
    );
    expect((await ask()).status).toBe(200);
    const calls = limitEmbedMock.batches.length;
    expect(calls).toBeGreaterThan(0);
    expect((await ask()).status).toBe(429);
    expect(limitEmbedMock.batches.length).toBe(calls);
  });

  test("a semantic route whose target was deleted falls through to default", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await chat("sem-dangling");
    expect(res.status).toBe(200);
    expect(await answerOf(res)).toBe("sem-default");
  });

  test("example vectors are embedded again when the embedding row names another model", async (ctx) => {
    if (!etcdReachable || !app || !seed || !embedMock) return ctx.skip();
    const example = "an example utterance";
    expect((await chat("sem-router")).status).toBe(200);
    expect(embedMock.batches.some((b) => b.includes(example))).toBe(true);
    // Steady state: the examples are cached, only the prompt is embedded.
    embedMock.batches.length = 0;
    expect((await chat("sem-router")).status).toBe(200);
    expect(embedMock.batches.some((b) => b.includes(example))).toBe(false);

    await seed.update("models", modelIds["sem-embed"]!, {
      ...embedDoc,
      model_name: "embed-new",
    });
    await waitConfigPropagation(async () => {
      embedMock!.batches.length = 0;
      expect((await chat("sem-router")).status).toBe(200);
      return embedMock!.batches.some((b) => b.includes(example));
    });
  });
});
