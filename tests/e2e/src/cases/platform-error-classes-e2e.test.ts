import { createHash } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test, vi } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockControlPlane,
  waitConfigPropagation,
  type MockControlPlane,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: in platform mode a tenant never learns which upstream channel or
// account served it. An upstream's failure reaches the caller as one of the
// gateway's own error classes, with a fixed status, code and message, on
// every endpoint family and on both halves of each: the buffered response and
// the error frame that ends a stream. What the upstream actually said still
// reaches the operator, in the usage event's `error_message`.
//
// The upstream below answers the way real providers do — an OpenAI-shape
// envelope on the OpenAI routes, Anthropic's on `/v1/messages` — and every
// failure text names a provider, a cloud and a region a tenant must not see.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const CALLER_KEY = "sk-platform-errors-e2e-caller";
const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_ID_KEY";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "962887e2a142f4af6a987d72d3e4446a63007120157fc194c00b6c4bcefa9d5b";
const LEAKS = ["Anthropic", "Bedrock", "Vertex", "us-east-1", "bdrk", "awselb"];
/**
 * Headers real channels put on their answers, every one of which names the
 * channel, its account or its edge. The mock sends them on every answer.
 */
const CHANNEL_HEADERS: Record<string, string> = {
  "x-request-id": "req_bdrk_0123",
  "x-amzn-requestid": "bdrk-amzn-0123",
  "anthropic-organization-id": "org-bdrk-0123",
  "openai-organization": "org-bdrk",
  "openai-processing-ms": "123",
  "x-ratelimit-limit-requests": "9999",
  "x-ratelimit-remaining-requests": "9998",
  server: "awselb/2.0",
  via: "1.1 bdrk-edge",
  "cf-ray": "bdrk-ray-0123",
};
/** Every header the gateway itself sends on these answers. */
const GATEWAY_HEADERS = new Set([
  "cache-control",
  "connection",
  "content-length",
  "content-type",
  "date",
  "keep-alive",
  "retry-after",
  "server",
  "transfer-encoding",
  "vary",
  "x-aisix-request-id",
]);
/** A word no answer in this spec contains: the output guardrail never blocks. */
const NEVER_SAID = "zq-never-in-any-answer-9x";

type Scenario =
  | "unauthorized"
  | "rate_limited"
  | "context"
  | "midstream"
  // A stream the upstream ends with no terminal event.
  | "truncated"
  // A buffered answer that is a failed Response object.
  | "failed_object"
  // A failed Response object whose code blames the request.
  | "caller_fault"
  // An upstream that stops answering: before its answer, or mid-stream.
  | "stall"
  // A stream that carries nothing at all.
  | "empty";
const MATRIX: Scenario[] = ["unauthorized", "rate_limited", "context", "midstream"];
const ALL_SCENARIOS: Scenario[] = [...MATRIX, "truncated", "failed_object", "caller_fault", "stall", "empty"];
/** How long the mock stalls: far past the models' own timeouts below. */
const STALL_MS = 3_000;

/** What the upstream says for `scenario` when `tag` is the route asking. */
function upstreamText(scenario: Scenario, tag: string, wire: "openai" | "anthropic"): string {
  switch (scenario) {
    case "unauthorized":
      return `Invalid Anthropic API key for the Bedrock account in us-east-1 [${tag}]`;
    case "rate_limited":
      return `Anthropic rate limit reached on Bedrock in us-east-1 [${tag}]`;
    case "context":
      return wire === "anthropic"
        ? `prompt is too long: 250000 tokens > 200000 maximum (Anthropic on Bedrock us-east-1) [${tag}]`
        : `This model's maximum context length is 8192 tokens, Anthropic on Bedrock us-east-1 [${tag}]`;
    case "midstream":
    case "truncated":
    case "failed_object":
    case "stall":
    case "empty":
      return `Bedrock ThrottlingException from Anthropic in us-east-1 [${tag}]`;
    case "caller_fault":
      return `Invalid prompt: flagged by the Bedrock safety stack in us-east-1 [${tag}]`;
  }
}

interface Route {
  tag: string;
  path:
    | "/v1/chat/completions"
    | "/v1/messages"
    | "/v1/messages/count_tokens"
    | "/v1/responses"
    | "/v1/embeddings";
  /** The envelope this endpoint answers in. */
  envelope: "openai" | "anthropic";
  /** Which provider key the route's models dispatch through. */
  pk: "openai" | "anthropic" | "compatible";
  provider: string;
}

const ROUTES: Route[] = [
  { tag: "chat", path: "/v1/chat/completions", envelope: "openai", pk: "openai", provider: "openai" },
  // An Anthropic upstream serves `/v1/messages` in its own wire…
  { tag: "messages-native", path: "/v1/messages", envelope: "anthropic", pk: "anthropic", provider: "anthropic" },
  // …and any other is reached through chat completions.
  { tag: "messages-bridged", path: "/v1/messages", envelope: "anthropic", pk: "openai", provider: "openai" },
  // OpenAI serves `/v1/responses` itself…
  { tag: "responses-native", path: "/v1/responses", envelope: "openai", pk: "openai", provider: "openai" },
  // …an OpenAI-compatible vendor through chat completions.
  { tag: "responses-bridged", path: "/v1/responses", envelope: "openai", pk: "compatible", provider: "deepseek" },
];
const CHAT = ROUTES[0];
const MESSAGES_NATIVE = ROUTES[1];
const RESPONSES_NATIVE = ROUTES[3];
const RESPONSES_BRIDGED = ROUTES[4];
/** OpenAI's own `/v1/responses`, behind an output guardrail that can block. */
const RESPONSES_HELD: Route = {
  tag: "responses-held",
  path: "/v1/responses",
  envelope: "openai",
  pk: "openai",
  provider: "openai",
};
const COUNT_NATIVE: Route = {
  tag: "count-native",
  path: "/v1/messages/count_tokens",
  envelope: "anthropic",
  pk: "anthropic",
  provider: "anthropic",
};
/** count_tokens addressed to a model whose upstream does not count tokens. */
const COUNT_OPENAI: Route = {
  tag: "count-openai",
  path: "/v1/messages/count_tokens",
  envelope: "anthropic",
  pk: "openai",
  provider: "openai",
};
const EMBEDDINGS: Route = {
  tag: "embeddings",
  path: "/v1/embeddings",
  envelope: "openai",
  pk: "openai",
  provider: "openai",
};

const modelName = (route: Route, scenario: Scenario) => `pe-${route.tag}-${scenario}`;
/** A second model per matrix cell, for the usage-event test's own traffic. */
const usageModelName = (route: Route, scenario: Scenario) => `pe-usage-${route.tag}-${scenario}`;
/** The upstream model id: the route and the scenario the mock acts out. */
const upstreamModel = (route: Route, scenario: Scenario) => `${route.tag}--${scenario}`;

// ── the upstream ────────────────────────────────────────────────────────────

function sse(res: ServerResponse, frames: string[], stallAfter = false): void {
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", ...CHANNEL_HEADERS });
  for (const frame of frames) res.write(frame);
  if (stallAfter) {
    setTimeout(() => res.end(), STALL_MS);
    return;
  }
  res.end();
}

const dataFrame = (payload: unknown) => `data: ${JSON.stringify(payload)}\n\n`;
const eventFrame = (event: string, payload: unknown) => `event: ${event}\ndata: ${JSON.stringify(payload)}\n\n`;

/** The opening of a Responses-API stream: the response, then some text. */
const responsesOpening = () => [
  eventFrame("response.created", {
    type: "response.created",
    sequence_number: 0,
    response: { id: "resp_up_1", object: "response", created_at: 1, status: "in_progress", model: "m", output: [] },
  }),
  eventFrame("response.output_text.delta", {
    type: "response.output_text.delta",
    sequence_number: 1,
    item_id: "msg_up_1",
    output_index: 0,
    content_index: 0,
    delta: "Hel",
  }),
];

function answerOpenAi(path: string, tag: string, scenario: Scenario, res: ServerResponse): void {
  const text = upstreamText(scenario, tag, "openai");
  const fail = (status: number, error: Record<string, unknown>, headers: Record<string, string> = {}) => {
    res.writeHead(status, { "content-type": "application/json", ...CHANNEL_HEADERS, ...headers });
    res.end(JSON.stringify({ error: { message: text, param: null, ...error } }));
  };
  const responses = path.endsWith("/responses");
  switch (scenario) {
    case "unauthorized":
      return fail(401, { type: "invalid_request_error", code: "invalid_api_key" });
    case "rate_limited":
      return fail(429, { type: "requests", code: "rate_limit_exceeded" }, { "retry-after": "7" });
    case "context":
      return fail(400, { type: "invalid_request_error", param: "messages", code: "context_length_exceeded" });
    case "failed_object":
    case "caller_fault":
      res.writeHead(200, { "content-type": "application/json", ...CHANNEL_HEADERS });
      res.end(
        JSON.stringify({
          id: "resp_up_1",
          object: "response",
          created_at: 1,
          status: "failed",
          model: "m",
          output: [],
          error: { code: scenario === "caller_fault" ? "invalid_prompt" : "server_error", message: text },
        }),
      );
      return;
    case "stall":
      // No answer before the model's timeout runs out.
      setTimeout(() => fail(500, { type: "server_error" }), STALL_MS);
      return;
    case "empty":
      return sse(res, ["data: [DONE]\n\n"]);
    case "truncated":
      // The connection ends after some text, with no terminal event.
      return sse(res, responsesOpening());
    case "midstream":
      if (responses) {
        return sse(res, [
          ...responsesOpening(),
          eventFrame("error", { type: "error", sequence_number: 2, code: "server_error", message: text, param: null }),
        ]);
      }
      return sse(res, [
        dataFrame({
          id: "chatcmpl-up-1",
          object: "chat.completion.chunk",
          created: 1,
          model: "m",
          choices: [{ index: 0, delta: { role: "assistant", content: "Hel" }, finish_reason: null }],
        }),
        dataFrame({ error: { message: text, type: "server_error", param: null, code: null } }),
      ]);
  }
}

function answerAnthropic(tag: string, scenario: Scenario, res: ServerResponse): void {
  const text = upstreamText(scenario, tag, "anthropic");
  const fail = (status: number, type: string, headers: Record<string, string> = {}) => {
    res.writeHead(status, { "content-type": "application/json", ...CHANNEL_HEADERS, ...headers });
    res.end(JSON.stringify({ type: "error", error: { type, message: text } }));
  };
  const opening = [
    eventFrame("message_start", {
      type: "message_start",
      message: {
        id: "msg_up_1",
        type: "message",
        role: "assistant",
        model: "m",
        content: [],
        stop_reason: null,
        stop_sequence: null,
        usage: { input_tokens: 5, output_tokens: 1 },
      },
    }),
    eventFrame("content_block_start", { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
    eventFrame("content_block_delta", {
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "Hel" },
    }),
  ];
  switch (scenario) {
    case "unauthorized":
      return fail(401, "authentication_error");
    case "rate_limited":
      return fail(429, "rate_limit_error", { "retry-after": "7" });
    case "context":
      return fail(400, "invalid_request_error");
    // The connection ends after some text: no `message_stop`, no error.
    case "truncated":
      return sse(res, opening);
    // Some text, then nothing more until long past the model's timeout.
    case "stall":
      return sse(res, opening, true);
    case "failed_object":
    case "caller_fault":
    case "empty":
    case "midstream":
      return sse(res, [
        eventFrame("message_start", {
          type: "message_start",
          message: {
            id: "msg_up_1",
            type: "message",
            role: "assistant",
            model: "m",
            content: [],
            stop_reason: null,
            stop_sequence: null,
            usage: { input_tokens: 5, output_tokens: 1 },
          },
        }),
        eventFrame("content_block_start", {
          type: "content_block_start",
          index: 0,
          content_block: { type: "text", text: "" },
        }),
        eventFrame("content_block_delta", {
          type: "content_block_delta",
          index: 0,
          delta: { type: "text_delta", text: "Hel" },
        }),
        eventFrame("error", { type: "error", error: { type: "overloaded_error", message: text } }),
      ]);
  }
}

async function startUpstream(): Promise<{ server: Server; base: string }> {
  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = (req.url ?? "").split("?")[0];
      let model = "";
      try {
        model = String((JSON.parse(Buffer.concat(chunks).toString("utf8")) as { model?: unknown }).model ?? "");
      } catch {
        // Not a JSON body: nothing this upstream serves.
      }
      const [tag, scenario] = model.split("--") as [string, Scenario | undefined];
      if (scenario === undefined || !ALL_SCENARIOS.includes(scenario)) {
        res.writeHead(404, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: { message: `no such model ${model}`, type: "invalid_request_error" } }));
        return;
      }
      if (path.startsWith("/v1/messages")) answerAnthropic(tag, scenario, res);
      else answerOpenAi(path, tag, scenario, res);
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, base: `http://127.0.0.1:${(server.address() as AddressInfo).port}` };
}

// ── the caller ──────────────────────────────────────────────────────────────

interface Answer {
  status: number;
  headers: Headers;
  text: string;
}

function requestBody(route: Route, model: string, stream: boolean): Record<string, unknown> {
  switch (route.path) {
    case "/v1/chat/completions":
      return { model, stream, messages: [{ role: "user", content: "hi" }] };
    case "/v1/messages":
      return { model, stream, max_tokens: 64, messages: [{ role: "user", content: "hi" }] };
    case "/v1/messages/count_tokens":
      return { model, messages: [{ role: "user", content: "hi" }] };
    case "/v1/responses":
      return { model, stream, input: "hi" };
    case "/v1/embeddings":
      return { model, input: "hi" };
  }
}

/** Every JSON payload an SSE body carries, in order. */
function ssePayloads(text: string): Record<string, unknown>[] {
  const out: Record<string, unknown>[] = [];
  for (const frame of text.split(/\n\n/)) {
    const data = frame
      .split("\n")
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trimStart())
      .join("\n");
    if (data === "" || data === "[DONE]") continue;
    try {
      out.push(JSON.parse(data) as Record<string, unknown>);
    } catch {
      // A frame that is not JSON carries no error payload.
    }
  }
  return out;
}

/** The `error.code` an error payload carries, in any of the three shapes. */
function payloadErrorCode(payload: Record<string, unknown>): unknown {
  const nested = payload.error as Record<string, unknown> | undefined;
  const response = payload.response as { error?: { code?: unknown } } | undefined;
  if (payload.type === "error" && typeof payload.code === "string") return payload.code;
  if (nested && typeof nested === "object") return nested.code;
  return response?.error?.code;
}

interface Expected {
  status: number;
  code: string;
  openaiType: string;
  anthropicType: string;
  message: string;
}

const UNAVAILABLE: Expected = {
  status: 503,
  code: "upstream_unavailable",
  openaiType: "server_error",
  anthropicType: "overloaded_error",
  message: "The model is temporarily unavailable. Retry later.",
};
const RATE_LIMITED: Expected = {
  status: 429,
  code: "rate_limit_exceeded",
  openaiType: "rate_limit_error",
  anthropicType: "rate_limit_error",
  message: "Rate limit exceeded. Retry later.",
};
const CONTEXT: Expected = {
  status: 400,
  code: "context_length_exceeded",
  openaiType: "invalid_request_error",
  anthropicType: "invalid_request_error",
  message: "prompt is too long: the request exceeds the model's context length.",
};
const INVALID: Expected = {
  status: 400,
  code: "invalid_request",
  openaiType: "invalid_request_error",
  anthropicType: "invalid_request_error",
  message: "The request is invalid.",
};

describe("platform mode: upstream failures reach the caller as the gateway's own classes", () => {
  let cp: MockControlPlane | undefined;
  let app: SpawnedApp | undefined;
  let upstream: { server: Server; base: string } | undefined;
  let etcdReachable = false;

  const callModel = async (route: Route, model: string, stream: boolean): Promise<Answer> => {
    const res = await fetch(`${app!.proxyUrl}${route.path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_KEY}`,
        "content-type": "application/json",
        ...(route.envelope === "anthropic" ? { "anthropic-version": "2023-06-01" } : {}),
      },
      body: JSON.stringify(requestBody(route, model, stream)),
    });
    return { status: res.status, headers: res.headers, text: await res.text() };
  };
  const call = (route: Route, scenario: Scenario, stream: boolean) =>
    callModel(route, modelName(route, scenario), stream);

  /**
   * Nothing the upstream wrote about itself is anywhere in what the caller
   * got: not in the body, and no header the gateway does not send itself,
   * though the upstream sent a channel's worth of them.
   */
  const expectNoLeak = (answer: Answer) => {
    const headerText = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
    for (const leak of LEAKS) {
      expect(answer.text, `body leaks ${leak}`).not.toContain(leak);
      expect(headerText, `headers leak ${leak}`).not.toContain(leak);
    }
    const foreign = [...answer.headers.keys()].filter((name) => !GATEWAY_HEADERS.has(name));
    expect(foreign, headerText).toEqual([]);
  };

  /** The buffered answer's error block, in the route's envelope. */
  const expectClass = (route: Route, answer: Answer, expected: Expected) => {
    expect(answer.status, answer.text).toBe(expected.status);
    const body = JSON.parse(answer.text) as { type?: string; error: Record<string, unknown> };
    expect(body.error.code).toBe(expected.code);
    expect(body.error.message).toBe(expected.message);
    if (route.envelope === "anthropic") {
      expect(body.type).toBe("error");
      expect(body.error.type).toBe(expected.anthropicType);
    } else {
      expect(body.error.type).toBe(expected.openaiType);
    }
    expectNoLeak(answer);
  };

  /** A stream that ends on an upstream failure ends on the class's frame. */
  const expectStreamEndsUnavailable = (answer: Answer) => {
    expect(answer.status, answer.text).toBe(200);
    expect(answer.headers.get("content-type") ?? "").toContain("text/event-stream");
    const codes = ssePayloads(answer.text)
      .map(payloadErrorCode)
      .filter((code) => code !== undefined && code !== null);
    expect(codes, answer.text).toContain("upstream_unavailable");
    expectNoLeak(answer);
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startUpstream();
    cp = await startMockControlPlane();
    app = await spawnApp({
      extra: { managed: cp.managed },
      awaitProxyListener: false,
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    const seed = new SeedClient(etcd, `${app.etcdPrefix}/${cp.envId}`);

    const pks = {
      openai: await seed.createProviderKey({
        display_name: "pe-pk-openai",
        secret: "sk-upstream-openai",
        api_base: `${upstream.base}/v1`,
      }),
      anthropic: await seed.createProviderKey({
        display_name: "pe-pk-anthropic",
        provider: "anthropic",
        adapter: "anthropic",
        secret: "sk-ant-upstream",
        api_base: upstream.base,
      }),
      compatible: await seed.createProviderKey({
        display_name: "pe-pk-compatible",
        provider: "deepseek",
        adapter: "openai",
        secret: "sk-upstream-compatible",
        api_base: `${upstream.base}/v1`,
      }),
    };
    const model = (
      route: Route,
      scenario: Scenario,
      name = modelName(route, scenario),
      extra: Record<string, unknown> = {},
    ) =>
      seed.createModel({
        display_name: name,
        provider: route.provider,
        model_name: upstreamModel(route, scenario),
        provider_key_id: pks[route.pk].id,
        ...extra,
      });
    for (const route of ROUTES) {
      for (const scenario of MATRIX) {
        await model(route, scenario);
        await model(route, scenario, usageModelName(route, scenario));
      }
    }
    await model(RESPONSES_NATIVE, "failed_object");
    await model(COUNT_NATIVE, "unauthorized");
    await model(COUNT_OPENAI, "unauthorized");
    await model(EMBEDDINGS, "rate_limited");
    await model(MESSAGES_NATIVE, "truncated");
    await model(MESSAGES_NATIVE, "stall", undefined, { stream_timeout: 500, retries: 0 });
    await model(CHAT, "stall", undefined, { timeout: 500, retries: 0 });
    await model(RESPONSES_NATIVE, "caller_fault");
    await model(RESPONSES_BRIDGED, "empty");
    // A block-capable output guardrail holds the whole stream back before
    // releasing it, which is a path of its own.
    const held = await seed.createGuardrail(
      {
        name: "pe-held-output",
        enabled: true,
        hook_point: "output",
        kind: "keyword",
        patterns: [{ kind: "literal", value: NEVER_SAID }],
      },
      { attach: false },
    );
    for (const scenario of ["midstream", "truncated"] as Scenario[]) {
      const heldModel = await model(RESPONSES_HELD, scenario);
      await seed.attachGuardrailToModel(held.id, heldModel.id);
    }
    // The caller key goes in last: once it authenticates, every model is in.
    await seed.createApiKey({ key_hash: hash(CALLER_KEY), allowed_models: ["*"] });

    await waitConfigPropagation(async () => {
      try {
        return (await new ProxyClient(app!.proxyUrl, CALLER_KEY).listModels()).status === 200;
      } catch (err) {
        // A managed gateway binds its proxy listener once the first
        // configuration applies; until then the connection is refused.
        // Anything else is a real failure.
        const refused = err as { code?: string; cause?: { code?: string } };
        if ((refused.code ?? refused.cause?.code) === "ECONNREFUSED") return false;
        throw err;
      }
    });
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await cp?.close();
    await new Promise<void>((resolve) => (upstream ? upstream.server.close(() => resolve()) : resolve()));
  });

  describe.each(ROUTES)("$tag", (route) => {
    test("an upstream 401 is the model being unavailable, not the caller's credential", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectClass(route, await call(route, "unauthorized", false), UNAVAILABLE);
    });

    test("an upstream 429 is a rate limit that keeps the upstream's Retry-After", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(route, "rate_limited", false);
      expectClass(route, answer, RATE_LIMITED);
      expect(answer.headers.get("retry-after")).toBe("7");
    });

    test("an upstream context overflow keeps its meaning", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectClass(route, await call(route, "context", false), CONTEXT);
    });

    test("an upstream error in the middle of a stream ends it with the class, not the upstream's text", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectStreamEndsUnavailable(await call(route, "midstream", true));
    });
  });

  test("responses-native: a failed Response object carries the class in place of the upstream's error", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const answer = await call(RESPONSES_NATIVE, "failed_object", false);
    const body = JSON.parse(answer.text) as { object?: string; status?: string; error?: Record<string, unknown> };
    expect(body.object, answer.text).toBe("response");
    expect(body.status).toBe("failed");
    expect(body.error?.code).toBe(UNAVAILABLE.code);
    expect(body.error?.message).toBe(UNAVAILABLE.message);
    expectNoLeak(answer);
  });

  test("responses-native: a failed Response whose code blames the request is an invalid request", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const answer = await call(RESPONSES_NATIVE, "caller_fault", false);
    const body = JSON.parse(answer.text) as { status?: string; error?: Record<string, unknown> };
    expect(body.status, answer.text).toBe("failed");
    expect(body.error?.code).toBe(INVALID.code);
    expect(body.error?.message).toBe(INVALID.message);
    expectNoLeak(answer);
  });

  test("messages-native: a stream the upstream cuts off ends with the class", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expectStreamEndsUnavailable(await call(MESSAGES_NATIVE, "truncated", true));
  });

  test("messages-native: an upstream that stops answering mid-stream ends the stream with the class", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    // The body ends on the class's frame rather than being cut off.
    expectStreamEndsUnavailable(await call(MESSAGES_NATIVE, "stall", true));
  });

  test("chat: an upstream that does not answer in time is the model being unavailable", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expectClass(CHAT, await call(CHAT, "stall", false), UNAVAILABLE);
  });

  test("responses-bridged: an upstream stream that carries nothing ends with the class", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expectStreamEndsUnavailable(await call(RESPONSES_BRIDGED, "empty", true));
  });

  describe("responses-held: behind an output guardrail that holds the stream back", () => {
    test("an upstream error in the middle of the stream ends it with the class", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectStreamEndsUnavailable(await call(RESPONSES_HELD, "midstream", true));
    });

    test("a stream the upstream cuts off ends with the class", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectStreamEndsUnavailable(await call(RESPONSES_HELD, "truncated", true));
    });
  });

  test("count_tokens: an upstream 401 is the model being unavailable", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expectClass(COUNT_NATIVE, await call(COUNT_NATIVE, "unauthorized", false), UNAVAILABLE);
  });

  test("count_tokens: a model whose upstream cannot count is an invalid request that names no provider", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    expectClass(COUNT_OPENAI, await call(COUNT_OPENAI, "unauthorized", false), INVALID);
  });

  test("embeddings: an upstream 429 is a rate limit that keeps the upstream's Retry-After", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const answer = await call(EMBEDDINGS, "rate_limited", false);
    expectClass(EMBEDDINGS, answer, RATE_LIMITED);
    expect(answer.headers.get("retry-after")).toBe("7");
  });

  test("the operator still reads what the upstream said, in each failure's usage event", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    // This test's own traffic, on models no other test addresses.
    const expected: Array<{ model: string; text: string }> = [];
    for (const route of ROUTES) {
      for (const scenario of MATRIX) {
        const model = usageModelName(route, scenario);
        const answer = await callModel(route, model, scenario === "midstream");
        expectNoLeak(answer);
        expected.push({
          model,
          text: upstreamText(scenario, route.tag, route.pk === "anthropic" ? "anthropic" : "openai"),
        });
      }
    }
    const errorMessages = () => {
      const byModel = new Map<string, string[]>();
      for (const body of cp!.telemetryBodies) {
        let parsed: { events?: Array<Record<string, unknown>> };
        try {
          parsed = JSON.parse(body) as typeof parsed;
        } catch {
          continue;
        }
        for (const event of parsed.events ?? []) {
          const model = String(event.requested_model ?? "");
          const list = byModel.get(model) ?? [];
          list.push(String(event.error_message ?? ""));
          byModel.set(model, list);
        }
      }
      return byModel;
    };
    await vi.waitFor(
      () => {
        const byModel = errorMessages();
        const missing = expected.filter(
          ({ model, text }) => !(byModel.get(model) ?? []).some((message) => message.includes(text)),
        );
        expect(missing.map((m) => m.model)).toEqual([]);
      },
      { timeout: 30_000, interval: 500 },
    );
  }, 60_000);
});
