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

// E2E: in platform mode hiding which upstream channel or account served a
// tenant is best effort, and never costs a client what it does with an
// error. An upstream's answer to a request error (a 400, 413 or 422) from an
// upstream that speaks the caller's own protocol reaches the caller as the
// upstream sent it, less the credential the gateway sent: clients act on
// those words (Claude Code compacts on "prompt is too long", the OpenAI SDK
// and Codex key off `context_length_exceeded`). Every other upstream failure,
// and any failure of a translated dispatch, reaches the caller as one of the
// gateway's own error classes, with a fixed status, code and message, on
// every endpoint family and on both halves of each: the buffered response and
// the error frame that ends a stream. What the upstream actually said still
// reaches the operator, in the usage event's `error_message`.
//
// The upstream below answers the way real providers do — an OpenAI-shape
// envelope on the OpenAI routes, Anthropic's on `/v1/messages` — and every
// failure text names a provider, a cloud and a region. A relayed request
// error carries them; nothing else the caller gets does.

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
  | "empty"
  // A refusal of one field of the request.
  | "field"
  // A context overflow in the exact words of the upstream's own API.
  | "overflow"
  // A refusal whose text quotes the credential the request was sent with.
  | "quoted_key"
  // A stream the upstream ends on a request error of its own.
  | "midstream_request"
  // A request too large for the upstream (413).
  | "too_large"
  // A request the upstream could not process (422).
  | "unprocessable"
  // A 400 that reports the platform's own account: a spend limit it set.
  | "account_limit";
const MATRIX: Scenario[] = ["unauthorized", "rate_limited", "context", "midstream"];
const ALL_SCENARIOS: Scenario[] = [
  ...MATRIX,
  "truncated",
  "failed_object",
  "caller_fault",
  "stall",
  "empty",
  "field",
  "overflow",
  "quoted_key",
  "midstream_request",
  "too_large",
  "unprocessable",
  "account_limit",
];
/**
 * Anthropic's words for a spend limit set on the account, with the date
 * access resumes (https://platform.claude.com/docs/en/api/rate-limits).
 */
const SPEND_LIMIT =
  "You have reached your specified workspace API usage limits. You will regain access on 2026-12-01 at 00:00 UTC.";
/** Anthropic's own words for a prompt over the context window, which Claude Code compacts on. */
const ANTHROPIC_OVERFLOW = "prompt is too long: 250000 tokens > 200000 maximum";
/** OpenAI's own words for the same, under the code the OpenAI SDK and Codex read. */
const OPENAI_OVERFLOW =
  "This model's maximum context length is 128000 tokens. However, your messages resulted in 250000 tokens. " +
  "Please reduce the length of the messages.";
/** What the scrubber writes in place of a credential. */
const REDACTED = "<redacted>";
/** The credentials the gateway sends upstream, seeded below. */
const UPSTREAM_SECRETS = ["sk-upstream-openai", "sk-ant-upstream", "sk-upstream-compatible"];
/** How long the mock stalls: far past the models' own timeouts below. */
const STALL_MS = 3_000;

/**
 * What the upstream says for `scenario` when `tag` is the route asking, with
 * `credential` the key the request reached it with.
 */
function upstreamText(scenario: Scenario, tag: string, wire: "openai" | "anthropic", credential = ""): string {
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
    case "field":
      // Anthropic names the field at the head of its message.
      return `max_tokens: 64001 > 64000, which is the maximum allowed number of output tokens for Anthropic on Bedrock us-east-1 [${tag}]`;
    case "overflow":
      return wire === "anthropic" ? ANTHROPIC_OVERFLOW : OPENAI_OVERFLOW;
    case "quoted_key":
      return `Invalid value for 'max_tokens' on the request authorized with ${credential} [${tag}]`;
    case "midstream_request":
      return wire === "anthropic"
        ? `messages.0.content.1.image.source.base64: image exceeds 5 MB maximum on Bedrock us-east-1 [${tag}]`
        : `Your input exceeds the context window of this model on Bedrock us-east-1 [${tag}]`;
    case "too_large":
      return `Request exceeds the maximum allowed number of bytes on Bedrock us-east-1 [${tag}]`;
    case "unprocessable":
      return `Input should be a valid list on Bedrock us-east-1 [${tag}]`;
    case "account_limit":
      return SPEND_LIMIT;
  }
}

/** The JSON the upstream answers a buffered request error of `scenario` with. */
function openAiFailureBody(text: string, error: Record<string, unknown>): Record<string, unknown> {
  return { error: { message: text, param: null, ...error } };
}

function anthropicFailureBody(text: string, type: string): Record<string, unknown> {
  return { type: "error", error: { type, message: text } };
}

/** The request error the upstream answers `scenario` with on a route of `wire`. */
function requestErrorBody(scenario: Scenario, tag: string, wire: "openai" | "anthropic", credential = "") {
  const text = upstreamText(scenario, tag, wire, credential);
  if (wire === "anthropic") return anthropicFailureBody(text, "invalid_request_error");
  switch (scenario) {
    case "field":
    case "quoted_key":
      return openAiFailureBody(text, { type: "invalid_request_error", param: "max_tokens", code: null });
    default:
      return openAiFailureBody(text, {
        type: "invalid_request_error",
        param: "messages",
        code: "context_length_exceeded",
      });
  }
}

/** The credential a request reached the upstream with, from either API's header. */
function receivedCredential(req: IncomingMessage): string {
  const bearer = (req.headers.authorization ?? "").replace(/^Bearer /, "");
  return bearer || String(req.headers["x-api-key"] ?? "");
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
  /** Whether the upstream speaks the route's own protocol, rather than being reached through a translation. */
  native: boolean;
}

const ROUTES: Route[] = [
  { tag: "chat", path: "/v1/chat/completions", envelope: "openai", pk: "openai", provider: "openai", native: true },
  // An Anthropic upstream serves `/v1/messages` in its own wire…
  {
    tag: "messages-native",
    path: "/v1/messages",
    envelope: "anthropic",
    pk: "anthropic",
    provider: "anthropic",
    native: true,
  },
  // …and any other is reached through chat completions.
  {
    tag: "messages-bridged",
    path: "/v1/messages",
    envelope: "anthropic",
    pk: "openai",
    provider: "openai",
    native: false,
  },
  // OpenAI serves `/v1/responses` itself…
  {
    tag: "responses-native",
    path: "/v1/responses",
    envelope: "openai",
    pk: "openai",
    provider: "openai",
    native: true,
  },
  // …an OpenAI-compatible vendor through chat completions.
  {
    tag: "responses-bridged",
    path: "/v1/responses",
    envelope: "openai",
    pk: "compatible",
    provider: "deepseek",
    native: false,
  },
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
  native: true,
};
const COUNT_NATIVE: Route = {
  tag: "count-native",
  path: "/v1/messages/count_tokens",
  envelope: "anthropic",
  pk: "anthropic",
  provider: "anthropic",
  native: true,
};
/** count_tokens addressed to a model whose upstream does not count tokens. */
const COUNT_OPENAI: Route = {
  tag: "count-openai",
  path: "/v1/messages/count_tokens",
  envelope: "anthropic",
  pk: "openai",
  provider: "openai",
  native: false,
};
const EMBEDDINGS: Route = {
  tag: "embeddings",
  path: "/v1/embeddings",
  envelope: "openai",
  pk: "openai",
  provider: "openai",
  native: true,
};
/** The wire a route's upstream answers in. */
const wireOf = (route: Route): "openai" | "anthropic" => (route.pk === "anthropic" ? "anthropic" : "openai");
/** Whether the upstream's answer to `scenario` on `route` is a request error the caller is relayed. */
const relays = (route: Route, scenario: Scenario) =>
  route.native && ["context", "field", "overflow", "quoted_key", "caller_fault", "midstream_request"].includes(scenario);

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

function answerOpenAi(path: string, tag: string, scenario: Scenario, res: ServerResponse, credential: string): void {
  const text = upstreamText(scenario, tag, "openai", credential);
  const fail = (status: number, error: Record<string, unknown>, headers: Record<string, string> = {}) => {
    res.writeHead(status, { "content-type": "application/json", ...CHANNEL_HEADERS, ...headers });
    res.end(JSON.stringify(openAiFailureBody(text, error)));
  };
  const responses = path.endsWith("/responses");
  switch (scenario) {
    case "overflow":
    case "quoted_key":
      res.writeHead(400, { "content-type": "application/json", ...CHANNEL_HEADERS });
      res.end(JSON.stringify(requestErrorBody(scenario, tag, "openai", credential)));
      return;
    case "unprocessable":
      return fail(422, { type: "invalid_request_error", param: "input", code: null });
    case "midstream_request":
      // A chat stream that ends on a request error of the upstream's own.
      if (!responses) {
        return sse(res, [
          dataFrame({
            id: "chatcmpl-up-1",
            object: "chat.completion.chunk",
            created: 1,
            model: "m",
            choices: [{ index: 0, delta: { role: "assistant", content: "Hel" }, finish_reason: null }],
          }),
          dataFrame(openAiFailureBody(text, { type: "invalid_request_error", param: "messages", code: "context_length_exceeded" })),
        ]);
      }
      // A Response that fails on the request: Codex reads its code.
      return sse(res, [
        ...responsesOpening(),
        eventFrame("response.failed", {
          type: "response.failed",
          sequence_number: 2,
          response: {
            id: "resp_up_1",
            object: "response",
            created_at: 1,
            status: "failed",
            model: "m",
            output: [],
            error: { code: "context_length_exceeded", message: text },
          },
        }),
      ]);
    case "unauthorized":
      return fail(401, { type: "invalid_request_error", code: "invalid_api_key" });
    case "rate_limited":
      return fail(429, { type: "requests", code: "rate_limit_exceeded" }, { "retry-after": "7" });
    case "context":
      return fail(400, { type: "invalid_request_error", param: "messages", code: "context_length_exceeded" });
    case "field":
      return fail(400, { type: "invalid_request_error", param: "max_tokens", code: null });
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

function answerAnthropic(tag: string, scenario: Scenario, res: ServerResponse, credential: string): void {
  const text = upstreamText(scenario, tag, "anthropic", credential);
  const fail = (status: number, type: string, headers: Record<string, string> = {}) => {
    res.writeHead(status, { "content-type": "application/json", ...CHANNEL_HEADERS, ...headers });
    res.end(JSON.stringify(anthropicFailureBody(text, type)));
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
    case "field":
    case "overflow":
    case "quoted_key":
    case "account_limit":
      return fail(400, "invalid_request_error");
    case "too_large":
      return fail(413, "request_too_large");
    // Some text, then a request error of the upstream's own.
    case "midstream_request":
      return sse(res, [...opening, eventFrame("error", anthropicFailureBody(text, "invalid_request_error"))]);
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
      const credential = receivedCredential(req);
      if (path.startsWith("/v1/messages")) answerAnthropic(tag, scenario, res, credential);
      else answerOpenAi(path, tag, scenario, res, credential);
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
   * No header the gateway does not send itself, though the upstream sent a
   * channel's worth of them, and nothing the upstream wrote about itself in
   * those it does send.
   */
  const expectGatewayHeaders = (answer: Answer) => {
    const headerText = [...answer.headers.entries()].map(([k, v]) => `${k}: ${v}`).join("\n");
    for (const leak of LEAKS) {
      expect(headerText, `headers leak ${leak}`).not.toContain(leak);
    }
    const foreign = [...answer.headers.keys()].filter((name) => !GATEWAY_HEADERS.has(name));
    expect(foreign, headerText).toEqual([]);
  };

  /**
   * Nothing the upstream wrote about itself is anywhere in what the caller
   * got: not in the body, and not in the headers.
   */
  const expectNoLeak = (answer: Answer) => {
    for (const leak of LEAKS) {
      expect(answer.text, `body leaks ${leak}`).not.toContain(leak);
    }
    expectGatewayHeaders(answer);
  };

  /**
   * A request error relayed as the upstream sent it: its own status and body,
   * nothing of the gateway's credentials, and only the gateway's headers.
   */
  const expectRelayed = (answer: Answer, status: number, body: unknown) => {
    expect(answer.status, answer.text).toBe(status);
    const received = JSON.parse(answer.text) as { type?: string; error?: Record<string, unknown> };
    // An OpenAI error object's `param` and `code` are optional, and SDKs read
    // a missing one as `null`: one the upstream sent as `null` may be left out.
    if (received.type === undefined && received.error) {
      received.error.param ??= null;
      received.error.code ??= null;
    }
    expect(received).toEqual(body);
    for (const secret of UPSTREAM_SECRETS) {
      expect(answer.text, `body carries ${secret}`).not.toContain(secret);
    }
    expectGatewayHeaders(answer);
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
    await model(MESSAGES_NATIVE, "field");
    for (const route of [CHAT, MESSAGES_NATIVE]) {
      await model(route, "overflow");
      await model(route, "quoted_key");
    }
    await model(MESSAGES_NATIVE, "midstream_request");
    await model(RESPONSES_NATIVE, "midstream_request");
    await model(CHAT, "midstream_request");
    await model(COUNT_NATIVE, "too_large");
    await model(EMBEDDINGS, "unprocessable");
    await model(MESSAGES_NATIVE, "account_limit");
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
      const answer = await call(route, "context", false);
      // An upstream of the route's own protocol is relayed in its own words;
      // one reached through a translation answers in another protocol's, so
      // the caller gets the class.
      if (route.native) expectRelayed(answer, 400, requestErrorBody("context", route.tag, wireOf(route)));
      else expectClass(route, answer, CONTEXT);
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
    // A failed Response's code is one the Responses API defines for it.
    expect(body.error?.code).toBe("server_error");
    expect(body.error?.message).toBe(UNAVAILABLE.message);
    expectNoLeak(answer);
  });

  test("responses-native: a failed Response whose code blames the request is relayed with its own error", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const answer = await call(RESPONSES_NATIVE, "caller_fault", false);
    const body = JSON.parse(answer.text) as { status?: string; error?: Record<string, unknown> };
    expect(body.status, answer.text).toBe("failed");
    // The Responses API's code for a turn that fails the same way again,
    // and the upstream's own words for why.
    expect(body.error).toEqual({
      code: "invalid_prompt",
      message: upstreamText("caller_fault", RESPONSES_NATIVE.tag, "openai"),
    });
    expectGatewayHeaders(answer);
  });

  test("messages-native: an upstream refusal of one field reaches the caller with the field it names", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const answer = await call(MESSAGES_NATIVE, "field", false);
    expectRelayed(answer, 400, requestErrorBody("field", MESSAGES_NATIVE.tag, "anthropic"));
    expect(JSON.parse(answer.text).error.message).toMatch(/^max_tokens: /);
  });

  describe("a native request error reaches the caller in the upstream's own words", () => {
    test("messages-native: Anthropic's context overflow, which Claude Code compacts on", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(MESSAGES_NATIVE, "overflow", false);
      expectRelayed(answer, 400, {
        type: "error",
        error: { type: "invalid_request_error", message: "prompt is too long: 250000 tokens > 200000 maximum" },
      });
    });

    test("chat: OpenAI's context overflow, under the code the OpenAI SDK reads", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(CHAT, "overflow", false);
      expectRelayed(answer, 400, {
        error: {
          message: OPENAI_OVERFLOW,
          type: "invalid_request_error",
          param: "messages",
          code: "context_length_exceeded",
        },
      });
    });

    test.for([CHAT, MESSAGES_NATIVE])(
      "$tag: the credential the gateway sent is not in what the upstream quotes back",
      async (route, ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const answer = await call(route, "quoted_key", false);
        expectRelayed(answer, 400, requestErrorBody("quoted_key", route.tag, wireOf(route), REDACTED));
      },
    );

    test("messages-native: a request error the upstream ends a stream on is its own error event", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(MESSAGES_NATIVE, "midstream_request", true);
      expect(answer.status, answer.text).toBe(200);
      const errors = ssePayloads(answer.text).filter((payload) => payload.type === "error");
      expect(errors, answer.text).toEqual([
        anthropicFailureBody(upstreamText("midstream_request", MESSAGES_NATIVE.tag, "anthropic"), "invalid_request_error"),
      ]);
      expectGatewayHeaders(answer);
    });

    test("chat: a request error the upstream ends a stream on is its own error frame", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(CHAT, "midstream_request", true);
      expect(answer.status, answer.text).toBe(200);
      const errors = ssePayloads(answer.text).filter((payload) => payload.error !== undefined);
      expect(errors, answer.text).toEqual([
        {
          error: {
            message: upstreamText("midstream_request", CHAT.tag, "openai"),
            type: "invalid_request_error",
            param: "messages",
            code: "context_length_exceeded",
          },
        },
      ]);
      expectGatewayHeaders(answer);
    });

    test("count_tokens: a request too large keeps its own 413", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(COUNT_NATIVE, "too_large", false);
      expectRelayed(
        answer,
        413,
        anthropicFailureBody(upstreamText("too_large", COUNT_NATIVE.tag, "anthropic"), "request_too_large"),
      );
    });

    test("embeddings: a request the upstream could not process keeps its own 422", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(EMBEDDINGS, "unprocessable", false);
      expectRelayed(answer, 422, {
        error: {
          message: upstreamText("unprocessable", EMBEDDINGS.tag, "openai"),
          type: "invalid_request_error",
          param: "input",
          code: null,
        },
      });
    });

    test("responses-native: a Response the upstream fails on the request keeps its own error", async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const answer = await call(RESPONSES_NATIVE, "midstream_request", true);
      expect(answer.status, answer.text).toBe(200);
      const failed = ssePayloads(answer.text).filter((payload) => payload.type === "response.failed");
      expect(failed.length, answer.text).toBe(1);
      expect((failed[0].response as { error?: unknown }).error).toEqual({
        code: "context_length_exceeded",
        message: upstreamText("midstream_request", RESPONSES_NATIVE.tag, "openai"),
      });
      expectGatewayHeaders(answer);
    });
  });

  test("responses-native: a previous_response_id the gateway cannot open carries the code clients resend on", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    // An id nobody was handed. Had it reached the upstream, the model's
    // scenario would have answered with an outage.
    const res = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_KEY}`, "content-type": "application/json" },
      body: JSON.stringify({
        model: modelName(RESPONSES_NATIVE, "unauthorized"),
        input: "hi",
        previous_response_id: "resp_0123456789abcdef0123456789abcdef",
      }),
    });
    const answer: Answer = { status: res.status, headers: res.headers, text: await res.text() };
    expect(answer.status, answer.text).toBe(400);
    expect(JSON.parse(answer.text)).toEqual({
      error: {
        message: "Previous response not found.",
        type: "invalid_request_error",
        param: "previous_response_id",
        code: "previous_response_not_found",
      },
    });
    expectNoLeak(answer);
  });

  test("messages-native: a 400 that reports the platform's own spend limit is the model being unavailable", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    // It describes an account every tenant shares: the caller learns neither
    // the limit nor when it resets, and may retry.
    const answer = await call(MESSAGES_NATIVE, "account_limit", false);
    expectClass(MESSAGES_NATIVE, answer, UNAVAILABLE);
    expect(answer.text).not.toContain("2026-12-01");
    expect(answer.text).not.toContain("usage limits");
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
    const answer = await call(COUNT_OPENAI, "unauthorized", false);
    expectClass(COUNT_OPENAI, answer, INVALID);
    // The field the caller can change is the model it named.
    expect(JSON.parse(answer.text).error.param).toBe("model");
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
        // A relayed request error carries the upstream's words by design.
        if (relays(route, scenario)) expectGatewayHeaders(answer);
        else expectNoLeak(answer);
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
