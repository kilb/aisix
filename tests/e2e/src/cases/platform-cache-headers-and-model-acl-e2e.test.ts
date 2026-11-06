import { createHash } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  waitForLogLine,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: in platform mode one gateway serves many tenants, and the answer a
// tenant gets must not tell it anything about the others.
//
// - Whether the cache served an answer is a debug fact. A hit is still served,
//   but unless the operator turned `proxy.expose_debug_headers` on, nothing on
//   the response says the cache answered: under a shared scope that would tell
//   a key that somebody else already asked the same thing. With the debug
//   headers on, a hit says so, on every endpoint.
// - A model that exists but that the caller's key may not use answers exactly
//   like a model that does not exist, on every endpoint: status, headers and
//   body. Otherwise a key could map another tenant's catalog by asking for
//   names. On a call that tried to use the model, the operator's access log
//   still records the refusal for what it was.
//
// The upstream's request log is the ground truth for a hit: two identical
// requests that reach it once were answered from the cache the second time.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const CALLER_KEY = "sk-platform-cache-acl-e2e-caller";
const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_ID_KEY";
const PLATFORM_SECRET = "platform-cache-acl-e2e-secret-0123456789abcdef";
const ACCESS_LINE = "proxy request completed";

/** The models the caller's key may use. */
const VISIBLE = { gpt: "pc-gpt", claude: "pc-claude", embed: "pc-embed" } as const;
/** Models that exist but that the caller's key may not use. */
const HIDDEN = { gpt: "pc-hidden-gpt", claude: "pc-hidden-claude", embed: "pc-hidden-embed" } as const;
/** Names no model carries, each as long as its hidden counterpart. */
const ABSENT = { gpt: "pc-absent-gpt", claude: "pc-absent-claude", embed: "pc-absent-embed" } as const;
/** What the hidden models are called upstream; no request may ever carry one. */
const HIDDEN_UPSTREAM_PREFIX = "upstream-hidden-";

type Family = keyof typeof VISIBLE;

// ── the upstream ────────────────────────────────────────────────────────────

const dataFrame = (payload: unknown) => `data: ${JSON.stringify(payload)}\n\n`;
const eventFrame = (event: string, payload: unknown) =>
  `event: ${event}\ndata: ${JSON.stringify(payload)}\n\n`;

function sse(res: ServerResponse, frames: string[]): void {
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  for (const frame of frames) res.write(frame);
  res.end();
}

function json(res: ServerResponse, body: unknown): void {
  res.writeHead(200, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function answerChat(stream: boolean, res: ServerResponse): void {
  if (!stream) {
    return json(res, {
      id: "chatcmpl-pc-1",
      object: "chat.completion",
      created: 1,
      model: "gpt-4o-mini",
      choices: [{ index: 0, message: { role: "assistant", content: "pc answer" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 },
    });
  }
  const chunk = (delta: Record<string, unknown>, finish: string | null, usage?: unknown) => ({
    id: "chatcmpl-pc-1",
    object: "chat.completion.chunk",
    created: 1,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta, finish_reason: finish }],
    ...(usage ? { usage } : {}),
  });
  sse(res, [
    dataFrame(chunk({ role: "assistant", content: "pc answer" }, null)),
    dataFrame(chunk({}, "stop", { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 })),
    "data: [DONE]\n\n",
  ]);
}

function answerResponses(stream: boolean, res: ServerResponse): void {
  const completed = {
    id: "resp_pc_1",
    object: "response",
    created_at: 1,
    status: "completed",
    model: "gpt-4o-mini",
    output: [
      {
        id: "msg_pc_1",
        type: "message",
        role: "assistant",
        status: "completed",
        content: [{ type: "output_text", text: "pc answer", annotations: [] }],
      },
    ],
    usage: { input_tokens: 5, output_tokens: 2, total_tokens: 7 },
  };
  if (!stream) return json(res, completed);
  sse(res, [
    eventFrame("response.created", {
      type: "response.created",
      sequence_number: 0,
      response: { ...completed, status: "in_progress", output: [], usage: null },
    }),
    eventFrame("response.output_text.delta", {
      type: "response.output_text.delta",
      sequence_number: 1,
      item_id: "msg_pc_1",
      output_index: 0,
      content_index: 0,
      delta: "pc answer",
    }),
    eventFrame("response.completed", { type: "response.completed", sequence_number: 2, response: completed }),
  ]);
}

function answerMessages(stream: boolean, res: ServerResponse): void {
  const message = {
    id: "msg_pc_1",
    type: "message",
    role: "assistant",
    model: "claude-3-5-haiku-20241022",
    content: [{ type: "text", text: "pc answer" }],
    stop_reason: "end_turn",
    stop_sequence: null,
    usage: { input_tokens: 5, output_tokens: 2 },
  };
  if (!stream) return json(res, message);
  sse(res, [
    eventFrame("message_start", {
      type: "message_start",
      message: { ...message, content: [], stop_reason: null, usage: { input_tokens: 5, output_tokens: 1 } },
    }),
    eventFrame("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
    eventFrame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "pc answer" },
    }),
    eventFrame("content_block_stop", { type: "content_block_stop", index: 0 }),
    eventFrame("message_delta", {
      type: "message_delta",
      delta: { stop_reason: "end_turn", stop_sequence: null },
      usage: { output_tokens: 2 },
    }),
    eventFrame("message_stop", { type: "message_stop" }),
  ]);
}

interface Upstream {
  server: Server;
  base: string;
  /** Every request received: its path and the `model` its body named. */
  received: Array<{ path: string; model: string }>;
}

async function startUpstream(): Promise<Upstream> {
  const received: Upstream["received"] = [];
  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = (req.url ?? "").split("?")[0];
      let body: { model?: unknown; stream?: unknown } = {};
      try {
        body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as typeof body;
      } catch {
        // Not a JSON body; answered by path alone.
      }
      received.push({ path, model: String(body.model ?? "") });
      const stream = body.stream === true;
      if (path.endsWith("/chat/completions")) return answerChat(stream, res);
      if (path.endsWith("/responses")) return answerResponses(stream, res);
      if (path.endsWith("/messages")) return answerMessages(stream, res);
      if (path.endsWith("/embeddings")) {
        return json(res, {
          object: "list",
          model: "text-embedding-3-small",
          data: [{ object: "embedding", index: 0, embedding: [0.25, 0.5] }],
          usage: { prompt_tokens: 5, total_tokens: 5 },
        });
      }
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: `no route ${path}`, type: "invalid_request_error" } }));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, base: `http://127.0.0.1:${(server.address() as AddressInfo).port}`, received };
}

// ── the caller ──────────────────────────────────────────────────────────────

interface Answer {
  status: number;
  headers: Headers;
  text: string;
}

interface Endpoint {
  name: string;
  path: string;
  family: Family;
  /** Called the way the Anthropic SDK calls: `x-api-key` and `anthropic-version`. */
  anthropic: boolean;
  /** `undefined` for a GET. */
  body?: (model: string, text: string) => Record<string, unknown>;
}

const CHAT: Endpoint = {
  name: "chat completions",
  path: "/v1/chat/completions",
  family: "gpt",
  anthropic: false,
  body: (model, text) => ({ model, messages: [{ role: "user", content: text }] }),
};
const MESSAGES: Endpoint = {
  name: "messages",
  path: "/v1/messages",
  family: "claude",
  anthropic: true,
  body: (model, text) => ({ model, max_tokens: 64, messages: [{ role: "user", content: text }] }),
};
const RESPONSES: Endpoint = {
  name: "responses",
  path: "/v1/responses",
  family: "gpt",
  anthropic: false,
  body: (model, text) => ({ model, input: text }),
};
const EMBEDDINGS: Endpoint = {
  name: "embeddings",
  path: "/v1/embeddings",
  family: "embed",
  anthropic: false,
  body: (model, text) => ({ model, input: text }),
};
const COUNT_TOKENS: Endpoint = {
  name: "count_tokens",
  path: "/v1/messages/count_tokens",
  family: "claude",
  anthropic: true,
  body: (model, text) => ({ model, messages: [{ role: "user", content: text }] }),
};
const streamed = (e: Endpoint): Endpoint => ({
  ...e,
  name: `${e.name}, streamed`,
  body: (model, text) => ({ ...e.body!(model, text), stream: true }),
});

/** Every endpoint a response cache covers. */
const CACHED = [CHAT, MESSAGES, RESPONSES, EMBEDDINGS];
/** Every way a caller can name a model and be told whether it can have it. */
const PROBES: Endpoint[] = [
  CHAT,
  streamed(CHAT),
  MESSAGES,
  streamed(MESSAGES),
  COUNT_TOKENS,
  RESPONSES,
  streamed(RESPONSES),
  EMBEDDINGS,
  { name: "model retrieve", path: "/v1/models/{model}", family: "gpt", anthropic: false },
  { name: "model retrieve, Anthropic shape", path: "/v1/models/{model}", family: "claude", anthropic: true },
];

async function call(app: SpawnedApp, endpoint: Endpoint, model: string, text = "hello"): Promise<Answer> {
  const headers: Record<string, string> = endpoint.anthropic
    ? { "x-api-key": CALLER_KEY, "anthropic-version": "2023-06-01" }
    : { authorization: `Bearer ${CALLER_KEY}` };
  const init: RequestInit = endpoint.body
    ? {
        method: "POST",
        headers: { ...headers, "content-type": "application/json" },
        body: JSON.stringify(endpoint.body(model, text)),
      }
    : { method: "GET", headers };
  const res = await fetch(`${app.proxyUrl}${endpoint.path.replace("{model}", model)}`, init);
  return { status: res.status, headers: res.headers, text: await res.text() };
}

/** The names of the cache headers an answer carries. */
const cacheHeaders = (answer: Answer) =>
  [...answer.headers.keys()].filter((name) => name.startsWith("x-aisix-cache"));

/** Headers whose value differs on every response, whatever it answers. */
const PER_RESPONSE = new Set(["date", "x-aisix-request-id"]);

/**
 * An answer with the model name the caller sent replaced by a placeholder,
 * and the headers every response carries a fresh value of blanked: what is
 * left is everything the answer says about the model.
 */
function shape(answer: Answer, model: string) {
  return {
    status: answer.status,
    headers: Object.fromEntries(
      [...answer.headers.entries()].map(([name, value]) => [
        name,
        PER_RESPONSE.has(name) ? "<per response>" : value.replaceAll(model, "<model>"),
      ]),
    ),
    body: answer.text.replaceAll(model, "<model>"),
  };
}

// ── the gateways ────────────────────────────────────────────────────────────

async function spawnPlatformGateway(etcd: EtcdClient, upstream: Upstream, exposeDebugHeaders: boolean) {
  const app = await spawnApp({
    exposeDebugHeaders,
    // The access log is how the operator sees a refusal.
    logLevel: "info",
    extraEnv: {
      AISIX_PROXY__PLATFORM__ENABLED: "true",
      AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
      [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
    },
  });
  const seed = new SeedClient(etcd, app.etcdPrefix);
  const openai = await seed.createProviderKey({
    display_name: "pc-pk-openai",
    secret: "sk-upstream-openai",
    api_base: `${upstream.base}/v1`,
  });
  const anthropic = await seed.createProviderKey({
    display_name: "pc-pk-anthropic",
    provider: "anthropic",
    adapter: "anthropic",
    secret: "sk-ant-upstream",
    api_base: upstream.base,
  });
  const upstreamOf: Record<Family, { provider: string; pk: string; model: string }> = {
    gpt: { provider: "openai", pk: openai.id, model: "gpt-4o-mini" },
    claude: { provider: "anthropic", pk: anthropic.id, model: "claude-3-5-haiku-20241022" },
    embed: { provider: "openai", pk: openai.id, model: "text-embedding-3-small" },
  };
  for (const family of Object.keys(VISIBLE) as Family[]) {
    const { provider, pk, model } = upstreamOf[family];
    await seed.createModel({ display_name: VISIBLE[family], provider, model_name: model, provider_key_id: pk });
    await seed.createModel({
      display_name: HIDDEN[family],
      provider,
      model_name: `${HIDDEN_UPSTREAM_PREFIX}${family}`,
      provider_key_id: pk,
    });
  }
  await seed.createCachePolicy({
    name: "pc-cache",
    enabled: true,
    backend: "memory",
    ttl_seconds: 300,
    applies_to: "all",
  });
  // The caller key goes in last: once it authenticates, everything above is in.
  await seed.createApiKey({ key_hash: hash(CALLER_KEY), allowed_models: Object.values(VISIBLE) });
  await waitConfigPropagation(
    async () => (await new ProxyClient(app.proxyUrl, CALLER_KEY).listModels()).status === 200,
  );
  return app;
}

describe("platform mode: cache headers and model permissions tell a tenant nothing", () => {
  let upstream: Upstream | undefined;
  let quiet: SpawnedApp | undefined;
  let debug: SpawnedApp | undefined;
  let etcdReachable = false;

  const upstreamCalls = (path: string) => upstream!.received.filter((r) => r.path.endsWith(path)).length;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startUpstream();
    quiet = await spawnPlatformGateway(etcd, upstream, false);
    debug = await spawnPlatformGateway(etcd, upstream, true);
  }, 120_000);

  afterAll(async () => {
    await quiet?.exit();
    await debug?.exit();
    await new Promise<void>((resolve) => (upstream ? upstream.server.close(() => resolve()) : resolve()));
  });

  test("without debug headers a hit is served, and no answer says the cache was involved", async (ctx) => {
    if (!etcdReachable || !quiet) return ctx.skip();
    const failures: string[] = [];
    for (const endpoint of CACHED) {
      const text = `quiet gateway, buffered, via ${endpoint.name}`;
      const before = upstreamCalls(endpoint.path);
      const first = await call(quiet, endpoint, VISIBLE[endpoint.family], text);
      const second = await call(quiet, endpoint, VISIBLE[endpoint.family], text);
      expect(first.status, `${endpoint.name}: ${first.text}`).toBe(200);
      expect(second.status, `${endpoint.name}: ${second.text}`).toBe(200);
      // The cache still works: the second answer never reached the upstream.
      expect(upstreamCalls(endpoint.path) - before, `${endpoint.name}: upstream calls`).toBe(1);
      for (const [which, answer] of [
        ["first", first],
        ["repeated", second],
      ] as const) {
        const named = cacheHeaders(answer);
        if (named.length > 0) failures.push(`${endpoint.name}, ${which} request: ${named.join(", ")}`);
      }
    }
    for (const endpoint of [CHAT, MESSAGES, RESPONSES].map(streamed)) {
      const answer = await call(quiet, endpoint, VISIBLE[endpoint.family], `quiet gateway via ${endpoint.name}`);
      expect(answer.status, `${endpoint.name}: ${answer.text}`).toBe(200);
      expect(answer.headers.get("content-type") ?? "", endpoint.name).toContain("text/event-stream");
      const named = cacheHeaders(answer);
      if (named.length > 0) failures.push(`${endpoint.name}: ${named.join(", ")}`);
    }
    expect(failures, `cache headers on a platform gateway without debug headers:\n${failures.join("\n")}`).toEqual(
      [],
    );
  });

  test("with debug headers on, a hit says so on every endpoint", async (ctx) => {
    if (!etcdReachable || !debug) return ctx.skip();
    const failures: string[] = [];
    for (const endpoint of CACHED) {
      const text = `debug gateway via ${endpoint.name}`;
      const before = upstreamCalls(endpoint.path);
      const first = await call(debug, endpoint, VISIBLE[endpoint.family], text);
      const second = await call(debug, endpoint, VISIBLE[endpoint.family], text);
      expect(first.status, `${endpoint.name}: ${first.text}`).toBe(200);
      expect(second.status, `${endpoint.name}: ${second.text}`).toBe(200);
      expect(upstreamCalls(endpoint.path) - before, `${endpoint.name}: upstream calls`).toBe(1);
      const said = second.headers.get("x-aisix-cache");
      if (said !== "hit") failures.push(`${endpoint.name}: x-aisix-cache=${said}, want hit`);
    }
    expect(failures, failures.join("\n")).toEqual([]);
  });

  describe.each([
    { gateway: "without debug headers", app: () => quiet },
    { gateway: "with debug headers", app: () => debug },
  ])("$gateway", ({ app }) => {
    test("the model list names the models the key may use and none it may not", async (ctx) => {
      const gateway = app();
      if (!etcdReachable || !gateway) return ctx.skip();
      for (const anthropic of [false, true]) {
        const list: Endpoint = { name: "model list", path: "/v1/models", family: "gpt", anthropic };
        const answer = await call(gateway, list, "");
        const shape = anthropic ? "Anthropic shape" : "OpenAI shape";
        expect(answer.status, `${shape}: ${answer.text}`).toBe(200);
        const ids = (JSON.parse(answer.text) as { data: Array<{ id: string }> }).data.map((m) => m.id);
        expect(ids, shape).toEqual(expect.arrayContaining(Object.values(VISIBLE)));
        for (const hidden of Object.values(HIDDEN)) expect(ids, shape).not.toContain(hidden);
      }
    });

    test.for(PROBES.map((probe) => ({ probe, name: probe.name })))(
      "$name: a model the key may not use answers exactly like one that does not exist",
      async ({ probe }, ctx) => {
        const gateway = app();
        if (!etcdReachable || !gateway) return ctx.skip();
        const hidden = HIDDEN[probe.family];
        const absent = ABSENT[probe.family];
        const forUnknown = await call(gateway, probe, absent);
        const forHidden = await call(gateway, probe, hidden);
        expect(forUnknown.status, forUnknown.text).toBe(404);
        expect(shape(forHidden, hidden)).toEqual(shape(forUnknown, absent));
        expect(
          upstream!.received.filter((r) => r.model.startsWith(HIDDEN_UPSTREAM_PREFIX)),
          "a refused request never reaches the upstream",
        ).toEqual([]);

        // A call that tried to use the model leaves the operator a record of
        // what really happened: the key was refused a model it may not use.
        // (A retrieve is a catalog lookup; only its caller side is pinned.)
        if (!probe.body) return;
        const requestId = forHidden.headers.get("x-aisix-request-id");
        expect(requestId, "every answer carries x-aisix-request-id").toBeTruthy();
        const line = await waitForLogLine(
          gateway,
          (l) => l.includes(ACCESS_LINE) && l.includes(`request_id="${requestId}"`),
          `the access-log line of ${probe.name} for ${hidden}`,
        );
        expect(line).toContain("status_code=403");
      },
    );
  });
});
