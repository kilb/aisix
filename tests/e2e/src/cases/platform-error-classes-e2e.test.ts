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
const PLATFORM_SECRET = "platform-errors-e2e-secret-0123456789abcdef";
const LEAKS = ["Anthropic", "Bedrock", "us-east-1"];

type Scenario = "unauthorized" | "rate_limited" | "context" | "midstream";
const SCENARIOS: Scenario[] = ["unauthorized", "rate_limited", "context", "midstream"];

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
      return `Bedrock ThrottlingException from Anthropic in us-east-1 [${tag}]`;
  }
}

interface Route {
  tag: string;
  path: "/v1/chat/completions" | "/v1/messages" | "/v1/responses";
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

const modelName = (route: Route, scenario: Scenario) => `pe-${route.tag}-${scenario}`;
/** The upstream model id: the route and the scenario the mock acts out. */
const upstreamModel = (route: Route, scenario: Scenario) => `${route.tag}--${scenario}`;

// ── the upstream ────────────────────────────────────────────────────────────

function sse(res: ServerResponse, frames: string[]): void {
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
  for (const frame of frames) res.write(frame);
  res.end();
}

const dataFrame = (payload: unknown) => `data: ${JSON.stringify(payload)}\n\n`;
const eventFrame = (event: string, payload: unknown) => `event: ${event}\ndata: ${JSON.stringify(payload)}\n\n`;

function answerOpenAi(path: string, tag: string, scenario: Scenario, res: ServerResponse): void {
  const text = upstreamText(scenario, tag, "openai");
  const fail = (status: number, error: Record<string, unknown>, headers: Record<string, string> = {}) => {
    res.writeHead(status, { "content-type": "application/json", ...headers });
    res.end(JSON.stringify({ error: { message: text, param: null, ...error } }));
  };
  switch (scenario) {
    case "unauthorized":
      return fail(401, { type: "invalid_request_error", code: "invalid_api_key" });
    case "rate_limited":
      return fail(429, { type: "requests", code: "rate_limit_exceeded" }, { "retry-after": "7" });
    case "context":
      return fail(400, { type: "invalid_request_error", param: "messages", code: "context_length_exceeded" });
    case "midstream":
      if (path.endsWith("/responses")) {
        return sse(res, [
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
    res.writeHead(status, { "content-type": "application/json", ...headers });
    res.end(JSON.stringify({ type: "error", error: { type, message: text } }));
  };
  switch (scenario) {
    case "unauthorized":
      return fail(401, "authentication_error");
    case "rate_limited":
      return fail(429, "rate_limit_error", { "retry-after": "7" });
    case "context":
      return fail(400, "invalid_request_error");
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
      if (scenario === undefined || !SCENARIOS.includes(scenario)) {
        res.writeHead(404, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: { message: `no such model ${model}`, type: "invalid_request_error" } }));
        return;
      }
      if (path === "/v1/messages") answerAnthropic(tag, scenario, res);
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
    case "/v1/responses":
      return { model, stream, input: "hi" };
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

describe("platform mode: upstream failures reach the caller as the gateway's own classes", () => {
  let cp: MockControlPlane | undefined;
  let app: SpawnedApp | undefined;
  let upstream: { server: Server; base: string } | undefined;
  let etcdReachable = false;

  const call = async (route: Route, scenario: Scenario, stream: boolean): Promise<Answer> => {
    const res = await fetch(`${app!.proxyUrl}${route.path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_KEY}`,
        "content-type": "application/json",
        ...(route.envelope === "anthropic" ? { "anthropic-version": "2023-06-01" } : {}),
      },
      body: JSON.stringify(requestBody(route, modelName(route, scenario), stream)),
    });
    return { status: res.status, headers: res.headers, text: await res.text() };
  };

  /** Nothing the upstream wrote about itself is anywhere in what the caller got. */
  const expectNoLeak = (answer: Answer) => {
    const headerText = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
    for (const leak of LEAKS) {
      expect(answer.text, `body leaks ${leak}`).not.toContain(leak);
      expect(headerText, `headers leak ${leak}`).not.toContain(leak);
    }
  };

  /** The buffered answer's error block, in the route's envelope. */
  const expectClass = (
    route: Route,
    answer: Answer,
    expected: { status: number; code: string; openaiType: string; anthropicType: string; message: string },
  ) => {
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
    for (const route of ROUTES) {
      for (const scenario of SCENARIOS) {
        await seed.createModel({
          display_name: modelName(route, scenario),
          provider: route.provider,
          model_name: upstreamModel(route, scenario),
          provider_key_id: pks[route.pk].id,
        });
      }
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
      expectClass(route, await call(route, "unauthorized", false), {
        status: 503,
        code: "upstream_unavailable",
        openaiType: "server_error",
        anthropicType: "overloaded_error",
        message: "The model is temporarily unavailable. Retry later.",
      });
    });

    test("an upstream 429 is a rate limit that keeps the upstream's Retry-After", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(route, "rate_limited", false);
      expectClass(route, answer, {
        status: 429,
        code: "rate_limited",
        openaiType: "rate_limit_error",
        anthropicType: "rate_limit_error",
        message: "Rate limit exceeded. Retry later.",
      });
      expect(answer.headers.get("retry-after")).toBe("7");
    });

    test("an upstream context overflow keeps its meaning", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      expectClass(route, await call(route, "context", false), {
        status: 400,
        code: "context_length_exceeded",
        openaiType: "invalid_request_error",
        anthropicType: "invalid_request_error",
        message: "The request exceeds the model's context length.",
      });
    });

    test("an upstream error in the middle of a stream ends it with the class, not the upstream's text", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(route, "midstream", true);
      expect(answer.status, answer.text).toBe(200);
      expect(answer.headers.get("content-type") ?? "").toContain("text/event-stream");
      const codes = ssePayloads(answer.text)
        .map(payloadErrorCode)
        .filter((code) => code !== undefined && code !== null);
      expect(codes, answer.text).toContain("upstream_unavailable");
      expectNoLeak(answer);
    });
  });

  test("the operator still reads what the upstream said, in each failure's usage event", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    const expected = ROUTES.flatMap((route) =>
      SCENARIOS.map((scenario) => ({
        model: modelName(route, scenario),
        text: upstreamText(scenario, route.tag, route.pk === "anthropic" ? "anthropic" : "openai"),
      })),
    );
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
