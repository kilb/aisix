import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  pickFreePort,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：调用远端审核服务的输入护栏拦下的请求，也要占调用方的请求限额。
//
// 远端审核按次计费，一个长请求能变成十几次调用。被拦的请求要是不占限额，
// 调用方就能不停地发被拦的内容：每一次都花审核服务的钱、吃它共享的 QPS，
// 别的调用方的审核随之失败（fail_open 时就不经审核放行）。所以：
//
//   - 被远端审核拦下的请求照样算一次请求；
//   - 限额用完之后，请求在审核之前就被 429 拒掉 —— 审核服务一次也不再被调用；
//   - 只跑本地规则（关键词、PII 正则）的护栏拦下的请求不花什么，照旧不占限额。
//
// 每个带输入护栏的入口都要这样：一个漏掉的入口就是一条不受限的通道。

const MARKER = "remoterefusalratemarker";
/** 每个调用方的请求限额：两次。 */
const LIMIT = 2;
const MODEL = "rr-openai";
const VIDEO_MODEL = "rr-video";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

/** 阿里云文本审核的替身：内容含被拦词时判高风险，并记下每一次调用。 */
async function startModeration(): Promise<{ baseUrl: string; calls: () => number; close(): Promise<void> }> {
  let calls = 0;
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      calls += 1;
      let content = "";
      try {
        const sp = JSON.parse(new URLSearchParams(raw).get("ServiceParameters") ?? "{}") as {
          content?: unknown;
        };
        content = typeof sp.content === "string" ? sp.content : "";
      } catch {
        content = "";
      }
      const risky = content.includes(MARKER);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          Code: 200,
          Message: "OK",
          RequestId: "RR-REQ",
          Data: {
            RiskLevel: risky ? "high" : "none",
            Result: risky ? [{ Label: "inappropriate_oral", Confidence: 100 }] : [{ Label: "nonLabel" }],
          },
        }),
      );
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((r) => server.listen(port, "127.0.0.1", r));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    calls: () => calls,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

const flagged = `please look into ${MARKER} for me`;

const json = (body: unknown) => ({
  headers: { "content-type": "application/json" },
  body: JSON.stringify(body),
});

/** 每个入口一条被拦的请求：路径与请求体（不含鉴权头）。 */
const ENDPOINTS: Array<[string, string, () => { headers: Record<string, string>; body: BodyInit }]> = [
  ["/v1/chat/completions", "/v1/chat/completions", () => json({ model: MODEL, messages: [{ role: "user", content: flagged }] })],
  [
    "/v1/messages",
    "/v1/messages",
    () => json({ model: MODEL, max_tokens: 16, messages: [{ role: "user", content: flagged }] }),
  ],
  ["/v1/responses", "/v1/responses", () => json({ model: MODEL, input: flagged })],
  ["/v1/completions", "/v1/completions", () => json({ model: MODEL, prompt: flagged })],
  ["/v1/embeddings", "/v1/embeddings", () => json({ model: MODEL, input: flagged })],
  ["/v1/rerank", "/v1/rerank", () => json({ model: MODEL, query: flagged, documents: ["one", "two"] })],
  ["/v1/images/generations", "/v1/images/generations", () => json({ model: MODEL, prompt: flagged })],
  ["/v1/audio/speech", "/v1/audio/speech", () => json({ model: MODEL, voice: "alloy", input: flagged })],
  [
    "/v1/audio/transcriptions",
    "/v1/audio/transcriptions",
    () => {
      const form = new FormData();
      form.set("model", MODEL);
      form.set("prompt", flagged);
      form.set("file", new Blob([new Uint8Array([0xff, 0xfb, 0x90, 0x00])], { type: "audio/mpeg" }), "a.mp3");
      return { headers: {}, body: form };
    },
  ],
  [
    "/v1/audio/translations",
    "/v1/audio/translations",
    () => {
      const form = new FormData();
      form.set("model", MODEL);
      form.set("prompt", flagged);
      form.set("file", new Blob([new Uint8Array([0xff, 0xfb, 0x90, 0x00])], { type: "audio/mpeg" }), "a.mp3");
      return { headers: {}, body: form };
    },
  ],
  [
    "/v1/batches",
    "/v1/batches",
    () => {
      const req = json({
        input_file_id: "file-rr",
        endpoint: "/v1/chat/completions",
        completion_window: "24h",
        metadata: { note: flagged },
      });
      return { headers: { ...req.headers, "x-aisix-model": MODEL }, body: req.body };
    },
  ],
  [
    "/v1/fine_tuning/jobs",
    "/v1/fine_tuning/jobs",
    () => {
      const req = json({ model: "gpt-4o-mini", training_file: "file-rr", suffix: flagged });
      return { headers: { ...req.headers, "x-aisix-model": MODEL }, body: req.body };
    },
  ],
  [
    "/v1/files (batch input)",
    "/v1/files",
    () => {
      const line = JSON.stringify({
        custom_id: "r1",
        method: "POST",
        url: "/v1/chat/completions",
        body: { model: "gpt-4o-mini", messages: [{ role: "user", content: flagged }] },
      });
      const form = new FormData();
      form.append("purpose", "batch");
      form.append("file", new Blob([line + "\n"], { type: "application/jsonl" }), "input.jsonl");
      return { headers: { "x-aisix-model": MODEL }, body: form };
    },
  ],
  ["/v1/videos", "/v1/videos", () => json({ model: VIDEO_MODEL, prompt: flagged })],
  [
    "passthrough route",
    "/rr-tunnel/chat/completions",
    () => json({ model: "gpt-4o-mini", messages: [{ role: "user", content: flagged }] }),
  ],
  [
    "/a2a/:agent",
    "/a2a/rr-agent",
    () =>
      json({
        jsonrpc: "2.0",
        id: 1,
        method: "message/send",
        params: {
          message: { kind: "message", messageId: randomUUID(), role: "user", parts: [{ kind: "text", text: flagged }] },
        },
      }),
  ],
];

const callerFor = (label: string) => `sk-rr-${sha256(label).slice(0, 12)}`;

/** 起一个网关：所有入口用到的模型、路由与 agent，加上给定的护栏，每个入口一个限额为 LIMIT 的调用方。 */
async function setup(guardrail: Record<string, unknown>, probe: string) {
  const etcd = new EtcdClient();
  if (!(await etcd.ping())) return undefined;
  const upstream = await startOpenAiUpstream();
  const app = await spawnApp();
  const seed = new SeedClient(etcd, app.etcdPrefix);
  const pk = await seed.createProviderKey({
    display_name: "rr-pk",
    provider: "openai",
    adapter: "openai",
    secret: "sk-mock",
    api_base: `${upstream.baseUrl}/v1`,
  });
  await seed.createModel({ display_name: MODEL, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: pk.id });
  const videoPk = await seed.createProviderKey({
    display_name: "rr-video-pk",
    provider: "alibaba",
    secret: "sk-mock",
    api_base: upstream.baseUrl,
  });
  await seed.createModel({
    display_name: VIDEO_MODEL,
    provider: "alibaba",
    model_name: "wan-mock",
    provider_key_id: videoPk.id,
  });
  await seed.createPassthroughRoute({
    name: "rr-route",
    path_prefix: "/rr-tunnel",
    target_url: `${upstream.baseUrl}/v1`,
    provider_key_id: pk.id,
    protocol: "openai_chat",
  });
  await seed.update("a2a_agents", randomUUID(), {
    name: "rr-agent",
    url: `${upstream.baseUrl}/a2a`,
    protocol_version: "0.3",
    auth_type: "none",
    enabled: true,
  });
  await seed.createGuardrail(guardrail);
  for (const [label] of ENDPOINTS) {
    await seed.createApiKey({
      key_hash: sha256(callerFor(label)),
      allowed_models: ["*"],
      allowed_routes: ["*"],
      allowed_agents: ["*"],
      rate_limit: { rpd: LIMIT },
    });
  }
  // 最后写一个不限额的探针 key：它能鉴权、被拦词被拦下时，前面的配置都已生效。
  await seed.createApiKey({ key_hash: sha256(probe), allowed_models: ["*"] });
  await waitConfigPropagation(async () => {
    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${probe}` },
      body: JSON.stringify({ model: MODEL, messages: [{ role: "user", content: flagged }] }),
    });
    await res.text();
    return res.status === 422;
  });
  return { app, upstream };
}

async function send(app: SpawnedApp, label: string, path: string, build: (typeof ENDPOINTS)[number][2]) {
  const { headers, body } = build();
  const res = await fetch(`${app.proxyUrl}${path}`, {
    method: "POST",
    headers: { ...headers, authorization: `Bearer ${callerFor(label)}` },
    body,
  });
  return { status: res.status, text: await res.text() };
}

describe("a refusal by a remote moderation service counts against the caller's request limit", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let moderation: Awaited<ReturnType<typeof startModeration>> | undefined;

  beforeAll(async () => {
    moderation = await startModeration();
    const ready = await setup(
      {
        name: "rr-aliyun",
        enabled: true,
        hook_point: "input",
        fail_open: false,
        kind: "aliyun_text_moderation",
        region: "cn-shanghai",
        endpoint: moderation.baseUrl,
        access_key_id: "LTAI_E2E",
        access_key_secret: "e2e-secret",
        risk_level_threshold: "high",
      },
      "sk-rr-probe-remote",
    );
    app = ready?.app;
    upstream = ready?.upstream;
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await moderation?.close();
  });

  for (const [label, path, build] of ENDPOINTS) {
    test(`${label}: refusals use the limit, and past it the service is not called`, async (ctx) => {
      if (!app || !moderation) return ctx.skip();
      const statuses: number[] = [];
      const bodies: string[] = [];
      const atStart = moderation.calls();
      let beforeLast = atStart;
      for (let i = 0; i <= LIMIT; i++) {
        if (i === LIMIT) beforeLast = moderation.calls();
        const res = await send(app, label, path, build);
        statuses.push(res.status);
        bodies.push(res.text.slice(0, 200));
      }
      expect(statuses, bodies.join("\n")).toEqual([...Array(LIMIT).fill(422), 429]);
      // 被拦下的那几次确实问过审核服务……
      expect(beforeLast - atStart).toBeGreaterThanOrEqual(LIMIT);
      // ……限额用完之后的那次则没有。
      expect(moderation.calls(), "the over-limit request still reached the moderation service").toBe(beforeLast);
    });
  }
});

describe("a refusal by local rules still costs the caller nothing", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    const ready = await setup(
      {
        name: "rr-keyword",
        enabled: true,
        hook_point: "input",
        kind: "keyword",
        patterns: [{ kind: "literal", value: MARKER }],
      },
      "sk-rr-probe-local",
    );
    app = ready?.app;
    upstream = ready?.upstream;
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  for (const [label, path, build] of ENDPOINTS) {
    test(`${label}: every refusal is a 422, none uses the limit`, async (ctx) => {
      if (!app) return ctx.skip();
      const statuses: number[] = [];
      for (let i = 0; i <= LIMIT; i++) statuses.push((await send(app, label, path, build)).status);
      expect(statuses).toEqual(Array(LIMIT + 1).fill(422));
    });
  }
});

/** Presidio 分析服务的替身：文本里有 EMAIL 时报一个 EMAIL_ADDRESS，并记下每一次调用。 */
const EMAIL = "carol.refusal@example.com";
async function startPresidio(): Promise<{ baseUrl: string; calls: () => number; close(): Promise<void> }> {
  let calls = 0;
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      let text = "";
      try {
        const body = JSON.parse(raw) as { text?: unknown };
        text = typeof body.text === "string" ? body.text : "";
      } catch {
        text = "";
      }
      res.setHeader("content-type", "application/json");
      if (req.url?.startsWith("/analyze")) {
        calls += 1;
        const at = text.indexOf(EMAIL);
        res.end(
          JSON.stringify(at >= 0 ? [{ entity_type: "EMAIL_ADDRESS", start: at, end: at + EMAIL.length, score: 0.9 }] : []),
        );
        return;
      }
      res.end(JSON.stringify({ text, items: [] }));
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((r) => server.listen(port, "127.0.0.1", r));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    calls: () => calls,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

describe("/v1/messages/count_tokens under a remote data-loss prevention check", () => {
  // count_tokens 只跑数据防泄漏一类的护栏（PII、Presidio）：Presidio 是其中唯一
  // 调用远端服务的，所以这条入口要单独用它来验。
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let presidio: Awaited<ReturnType<typeof startPresidio>> | undefined;
  const CALLER = "sk-rr-count-tokens";
  const PROBE = "sk-rr-count-tokens-probe";

  const countTokens = (key: string) =>
    fetch(`${app!.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
      body: JSON.stringify({ model: MODEL, messages: [{ role: "user", content: `write to ${EMAIL}` }] }),
    });

  beforeAll(async () => {
    const etcd = new EtcdClient();
    if (!(await etcd.ping())) return;
    presidio = await startPresidio();
    upstream = await startOpenAiUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "rr-ct-pk",
      provider: "openai",
      adapter: "openai",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({ display_name: MODEL, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: pk.id });
    await seed.createGuardrail({
      name: "rr-presidio",
      enabled: true,
      hook_point: "input",
      fail_open: false,
      kind: "presidio",
      analyzer_url: presidio.baseUrl,
      anonymizer_url: presidio.baseUrl,
      entities: [{ type: "EMAIL_ADDRESS", action: "block" }],
      language: "en",
      score_threshold: 0.5,
    });
    await seed.createApiKey({ key_hash: sha256(CALLER), allowed_models: ["*"], rate_limit: { rpd: LIMIT } });
    await seed.createApiKey({ key_hash: sha256(PROBE), allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await countTokens(PROBE);
      await res.text();
      return res.status === 422;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await presidio?.close();
  });

  test("refusals use the limit, and past it the service is not called", async (ctx) => {
    if (!app || !presidio) return ctx.skip();
    const statuses: number[] = [];
    const atStart = presidio.calls();
    let beforeLast = atStart;
    for (let i = 0; i <= LIMIT; i++) {
      if (i === LIMIT) beforeLast = presidio.calls();
      const res = await countTokens(CALLER);
      statuses.push(res.status);
      await res.text();
    }
    expect(statuses).toEqual([...Array(LIMIT).fill(422), 429]);
    expect(beforeLast - atStart).toBeGreaterThanOrEqual(LIMIT);
    expect(presidio.calls(), "the over-limit request still reached the service").toBe(beforeLast);
  });
});
