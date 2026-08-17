import { createHash, randomUUID } from "node:crypto";
import OpenAI, { APIError } from "openai";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: `kind: "pii"` guardrail (#932 / AISIX-Cloud#932) — in-process
// sensitive-data detection with per-detector `mask` / `block` actions.
//
// - mask on the REQUEST: the caller's prompt PII is rewritten to
//   [<DETECTOR>_REDACTED] before it reaches the upstream (verified via the
//   mock upstream's received body).
// - mask on the RESPONSE (non-streaming + streaming): the model's reply is
//   rewritten before it reaches the caller; the streaming case splits the
//   value across chunk boundaries to pin the channel-reassembly path.
// - block: a block-action detector rejects with the standard 422
//   content_filter envelope, and the matched value never appears in it.
//
// Detector values below are synthetic: the china_id_card sample is the
// canonical ISO 7064 documentation example, the bank card is the classic
// Luhn test number.

const CALLER = "sk-pii-e2e-caller";
const hash = (s: string) => createHash("sha256").update(s).digest("hex");

const EMAIL = "alice@example.com";
const CN_ID = "11010519491231002X"; // valid ISO 7064 MOD 11-2 check digit
const CARD = "4111111111111111"; // passes Luhn
const ANTHROPIC_OUTPUT_KEY_NONSTREAM = "anthropic-output-key-nonstream";
const ANTHROPIC_OUTPUT_KEY_STREAM = "anthropic-output-key-stream";
const ANTHROPIC_OUTPUT_KEY_START = "anthropic-output-key-start";
const TOOL_IDENTIFIER = "sk-abcdefghijklmnopqrstuv";
const ANTHROPIC_OUTPUT_IDENTIFIER_NONSTREAM = "anthropic-output-identifier-nonstream";
const ANTHROPIC_OUTPUT_IDENTIFIER_STREAM = "anthropic-output-identifier-stream";

async function waitForModels(app: SpawnedApp, apiKey: string, models: string[]) {
  await waitConfigPropagation(async () => {
    const response = await fetch(`${app.proxyUrl}/v1/models`, {
      headers: { authorization: `Bearer ${apiKey}` },
    });
    if (response.status === 401) {
      await response.text();
      return false;
    }
    if (response.status !== 200) {
      throw new Error(`model propagation probe returned ${response.status}`);
    }
    const body = (await response.json()) as { data?: Array<{ id?: string }> };
    const ids = new Set(body.data?.map((model) => model.id));
    return models.every((model) => ids.has(model));
  });
}

async function usageStatusCounter(
  app: SpawnedApp,
  handler: string,
  statusClass: string,
): Promise<number> {
  const response = await fetch(`${app.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  const line = (await response.text())
    .split("\n")
    .find(
      (candidate) =>
        candidate.startsWith("aisix_usage_events_emitted_total{") &&
        candidate.includes(`handler="${handler}"`) &&
        candidate.includes(`status_code="${statusClass}"`),
    );
  return line ? Number(line.trim().split(/\s+/).at(-1)) : 0;
}

describe("pii guardrail e2e: mask + block on request and response", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let anthropicUpstream: import("node:http").Server | undefined;
  let seed: SeedClient | undefined;
  let etcd: EtcdClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Non-streaming upstream: echoes a reply CONTAINING an email, so the
    // output mask has something to rewrite.
    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-pii",
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: "gpt-4o-mini",
        choices: [
          {
            index: 0,
            message: {
              role: "assistant",
              content: `you can reach the customer at ${EMAIL} today`,
            },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 5, completion_tokens: 8, total_tokens: 13 },
      },
    });

    // Streaming upstream: the SAME email split across two delta chunks —
    // per-chunk masking would miss it; only the hold-back channel
    // reassembly catches the span.
    streamUpstream = await startOpenAiUpstream({
      streamEvents: [
        '{"id":"strm-pii","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}',
        '{"id":"strm-pii","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"mail alice@exam"},"finish_reason":null}]}',
        '{"id":"strm-pii","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"ple.com now"},"finish_reason":null}]}',
        '{"id":"strm-pii","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}',
        "[DONE]",
      ],
      eventDelayMs: 20,
    });

    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "pii-e2e-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "pii-e2e",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    const streamPk = await seed.createProviderKey({
      display_name: "pii-stream-e2e-pk",
      secret: "sk-mock",
      api_base: `${streamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "pii-stream-e2e",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: streamPk.id,
    });
    // One env-wide pii guardrail: email masks (redact-and-continue),
    // china_id_card blocks (reject).
    await seed.createGuardrail({
      name: "pii-e2e-guard",
      enabled: true,
      hook_point: "both",
      kind: "pii",
      detectors: [
        { type: "email", action: "mask" },
        { type: "china_id_card", action: "block" },
        { type: "api_key", action: "mask" },
      ],
    });
    // Seed authentication after models and the env-wide guardrail. Once each
    // key can discover its model, the ordered watch has applied every earlier
    // row without probing the masking behavior these tests verify.
    await seed.createApiKey({
      key_hash: hash(CALLER),
      allowed_models: ["pii-e2e"],
    });
    await seed.createApiKey({
      key_hash: hash(`${CALLER}-stream`),
      allowed_models: ["pii-stream-e2e"],
    });
    await waitForModels(app, CALLER, ["pii-e2e"]);
    await waitForModels(app, `${CALLER}-stream`, ["pii-stream-e2e"]);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await streamUpstream?.close();
    if (anthropicUpstream?.listening) {
      await new Promise<void>((resolve, reject) =>
        anthropicUpstream!.close((e) => (e ? reject(e) : resolve())),
      );
    }
  });

  const client = () =>
    new OpenAI({
      apiKey: CALLER,
      baseURL: `${app!.proxyUrl}/v1`,
      maxRetries: 0,
    });

  test("mask: request PII is rewritten before the upstream, response PII before the caller", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    const res = await client().chat.completions.create({
      model: "pii-e2e",
      messages: [
        { role: "user", content: `contact me at ${EMAIL} about the order` },
      ],
    });

    // Response side: the model's reply had the email; the caller sees the
    // mask token and never the value.
    const reply = res.choices[0]?.message?.content ?? "";
    expect(reply).toContain("[EMAIL_REDACTED]");
    expect(reply).not.toContain(EMAIL);

    // Request side: the upstream received the MASKED prompt — the value
    // never left the gateway. (Structure preserved: only the span is
    // replaced.)
    const lastReq = upstream.receivedRequests.at(-1);
    expect(lastReq).toBeDefined();
    const upstreamBody = lastReq!.body;
    expect(upstreamBody).toContain("[EMAIL_REDACTED]");
    expect(upstreamBody).toContain("about the order");
    expect(upstreamBody).not.toContain(EMAIL);
  });

  test("block: a block-action detector rejects with 422 content_filter, value not echoed", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    const upstreamHitsBefore = upstream.receivedRequests.length;
    let caught: unknown;
    try {
      await client().chat.completions.create({
        model: "pii-e2e",
        messages: [{ role: "user", content: `my id number is ${CN_ID}` }],
      });
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(APIError);
    if (!(caught instanceof APIError)) throw new Error("unreachable");
    expect(caught.status).toBe(422);
    expect((caught.error as { type?: unknown })?.type).toBe("content_filter");
    // #153 / #932 no-leak: the matched value never appears in the envelope;
    // the guardrail name does (#519 B.4b).
    const blob = JSON.stringify(caught.error ?? {}) + (caught.message ?? "");
    expect(blob).not.toContain(CN_ID);
    expect(blob).toContain("guardrail 'pii-e2e-guard'");
    // Input block fires pre-dispatch: the upstream is never hit.
    expect(upstream.receivedRequests.length).toBe(upstreamHitsBefore);
  });

  test("mask does NOT block: a mask-only match still gets a 200", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    // A bank card would only matter to a block detector — none is
    // configured for it, and email is mask-action, so the request goes
    // through (masked) rather than 422ing.
    const res = await client().chat.completions.create({
      model: "pii-e2e",
      messages: [{ role: "user", content: `card ${CARD} email ${EMAIL}` }],
    });
    expect(res.choices[0]?.message?.content ?? "").toContain("[EMAIL_REDACTED]");
  });

  test("streaming mask: a span split across delta chunks is reassembled and masked (#932)", async (ctx) => {
    if (!etcdReachable || !app || !streamUpstream) {
      ctx.skip();
      return;
    }

    const streamCaller = `${CALLER}-stream`;
    const doStream = () =>
      fetch(`${app!.proxyUrl}/v1/chat/completions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${streamCaller}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model: "pii-stream-e2e",
          messages: [{ role: "user", content: "innocent prompt" }],
          stream: true,
        }),
      });

    const res = await doStream();
    expect(res.status).toBe(200);
    const wire = await res.text();

    // The email was split "alice@exam" + "ple.com" across two chunks —
    // channel reassembly at the hold-back release must still catch it.
    expect(wire).toContain("[EMAIL_REDACTED]");
    expect(wire).not.toContain(EMAIL);
    expect(wire).not.toContain("alice@exam");
    // Clean stream contract: [DONE] present, no error event.
    expect(wire).toContain("data: [DONE]");
    expect(wire).not.toContain("event: error");
    // Non-content fields survive the rewrite (finish_reason intact).
    expect(wire).toContain('"finish_reason":"stop"');
  });

  test("monitor mode: enforcement_mode=monitor observes but does not mask", async (ctx) => {
    if (!etcdReachable) {
      ctx.skip();
      return;
    }

    // Dedicated app instance (its own etcd prefix / env), so the env-wide
    // masking guardrail from the main suite cannot interfere — this env
    // carries ONLY the monitor-mode pii guardrail.
    const monUpstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-mon",
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: "gpt-4o-mini",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: `reply with ${EMAIL}` },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 5, completion_tokens: 8, total_tokens: 13 },
      },
    });
    const monApp = await spawnApp();
    const monSeed = new SeedClient(new EtcdClient(), monApp.etcdPrefix);
    const monPk = await monSeed.createProviderKey({
      display_name: "pii-mon-pk",
      secret: "sk-mock",
      api_base: `${monUpstream.baseUrl}/v1`,
    });
    await monSeed.createModel({
      display_name: "pii-mon-e2e",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: monPk.id,
    });
    const monCaller = `${CALLER}-mon`;
    await monSeed.createGuardrail({
      name: "pii-mon-guard",
      enabled: true,
      hook_point: "both",
      enforcement_mode: "monitor",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
    });
    await monSeed.createApiKey({
      key_hash: hash(monCaller),
      allowed_models: ["pii-mon-e2e"],
    });
    await waitForModels(monApp, monCaller, ["pii-mon-e2e"]);

    const monClient = new OpenAI({
      apiKey: monCaller,
      baseURL: `${monApp.proxyUrl}/v1`,
      maxRetries: 0,
    });
    const res = await monClient.chat.completions.create({
      model: "pii-mon-e2e",
      messages: [{ role: "user", content: `mask me maybe: ${EMAIL}` }],
    });
    // Monitor mode: content flows UNCHANGED in both directions; the
    // would-be mask counts land in ops logs only.
    expect(res.choices[0]?.message?.content ?? "").toContain(EMAIL);
    const lastReq = monUpstream.receivedRequests.at(-1);
    expect(lastReq!.body).toContain(EMAIL);

    await monApp.exit();
    await monUpstream.close();
  });

  test("/v1/messages passthrough: request masked before the Anthropic upstream", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    // Anthropic-native mock upstream: capture the received body, return a
    // minimal non-streaming message whose reply carries an email so the
    // output mask has something to rewrite too.
    const received: unknown[] = [];
    const { createServer } = await import("node:http");
    anthropicUpstream = createServer((req, res) => {
      const chunks: Buffer[] = [];
      req.on("data", (c: Buffer) => chunks.push(c));
      req.on("end", () => {
        const request = JSON.parse(Buffer.concat(chunks).toString()) as {
          messages?: unknown;
        };
        received.push(request);
        const requestText = JSON.stringify(request.messages ?? []);
        if (requestText.includes(ANTHROPIC_OUTPUT_IDENTIFIER_STREAM)) {
          const frames = [
            {
              type: "message_start",
              message: {
                id: "msg_pii_identifier_stream",
                type: "message",
                role: "assistant",
                model: "claude-3-5-haiku-20241022",
                content: [],
                stop_reason: null,
                usage: { input_tokens: 4, output_tokens: 0 },
              },
            },
            {
              type: "content_block_start",
              index: 0,
              content_block: {
                type: "tool_use",
                id: "toolu_pii_identifier",
                name: TOOL_IDENTIFIER,
                input: {},
              },
            },
            { type: "content_block_stop", index: 0 },
            {
              type: "message_delta",
              delta: { stop_reason: "tool_use" },
              usage: { output_tokens: 6 },
            },
            { type: "message_stop" },
          ];
          res.writeHead(200, { "content-type": "text/event-stream" });
          res.end(
            frames
              .map((frame) => `event: ${frame.type}\ndata: ${JSON.stringify(frame)}\n\n`)
              .join(""),
          );
          return;
        }
        if (requestText.includes(ANTHROPIC_OUTPUT_KEY_START)) {
          const frames = [
            {
              type: "message_start",
              message: {
                id: "msg_pii_start_key",
                type: "message",
                role: "assistant",
                model: "claude-3-5-haiku-20241022",
                content: [],
                stop_reason: null,
                usage: { input_tokens: 4, output_tokens: 0 },
              },
            },
            {
              type: "content_block_start",
              index: 0,
              content_block: {
                type: "tool_use",
                id: "toolu_pii_start",
                name: "lookup",
                input: { [EMAIL]: "safe" },
              },
            },
            { type: "content_block_stop", index: 0 },
            {
              type: "message_delta",
              delta: { stop_reason: "tool_use" },
              usage: { output_tokens: 6 },
            },
            { type: "message_stop" },
          ];
          res.writeHead(200, { "content-type": "text/event-stream" });
          res.end(
            frames
              .map((frame) => `event: ${frame.type}\ndata: ${JSON.stringify(frame)}\n\n`)
              .join(""),
          );
          return;
        }
        if (requestText.includes(ANTHROPIC_OUTPUT_KEY_STREAM)) {
          const frames = [
            {
              type: "message_start",
              message: {
                id: "msg_pii_stream_key",
                type: "message",
                role: "assistant",
                model: "claude-3-5-haiku-20241022",
                content: [],
                stop_reason: null,
                usage: { input_tokens: 4, output_tokens: 0 },
              },
            },
            {
              type: "content_block_start",
              index: 0,
              content_block: {
                type: "tool_use",
                id: "toolu_pii",
                name: "lookup",
                input: {},
              },
            },
            {
              type: "content_block_delta",
              index: 0,
              delta: {
                type: "input_json_delta",
                partial_json: '{"alice@exam',
              },
            },
            {
              type: "content_block_delta",
              index: 0,
              delta: {
                type: "input_json_delta",
                partial_json: 'ple.com":"safe"}',
              },
            },
            { type: "content_block_stop", index: 0 },
            {
              type: "message_delta",
              delta: { stop_reason: "tool_use" },
              usage: { output_tokens: 6 },
            },
            { type: "message_stop" },
          ];
          res.writeHead(200, { "content-type": "text/event-stream" });
          res.end(
            frames
              .map((frame) => `event: ${frame.type}\ndata: ${JSON.stringify(frame)}\n\n`)
              .join(""),
          );
          return;
        }

        const content = requestText.includes(ANTHROPIC_OUTPUT_KEY_NONSTREAM)
          ? [
              {
                type: "tool_use",
                id: "toolu_pii",
                name: "lookup",
                input: { [EMAIL]: "safe" },
              },
            ]
          : requestText.includes(ANTHROPIC_OUTPUT_IDENTIFIER_NONSTREAM)
            ? [
                {
                  type: "tool_use",
                  id: "toolu_pii_identifier",
                  name: TOOL_IDENTIFIER,
                  input: {},
                },
              ]
          : [{ type: "text", text: `reach them at ${EMAIL} ok` }];
        res.writeHead(200, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            id: "msg_pii",
            type: "message",
            role: "assistant",
            model: "claude-3-5-haiku-20241022",
            content,
            stop_reason: "end_turn",
            usage: { input_tokens: 4, output_tokens: 6 },
          }),
        );
      });
    });
    await new Promise<void>((resolve) => anthropicUpstream!.listen(0, resolve));
    const anthPort = (anthropicUpstream.address() as { port: number }).port;

    const anthPk = await seed.createProviderKey({
      display_name: "pii-anth-pk",
      secret: "sk-ant-mock",
      api_base: `http://127.0.0.1:${anthPort}`,
      provider: "anthropic",
      adapter: "anthropic",
    });
    await seed.createModel({
      display_name: "pii-anth-e2e",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: anthPk.id,
    });
    const anthCaller = `${CALLER}-anth`;
    await seed.createApiKey({
      key_hash: hash(anthCaller),
      allowed_models: ["pii-anth-e2e"],
    });
    await waitForModels(app, anthCaller, ["pii-anth-e2e"]);

    const malformedBaseline = received.length;
    const malformed = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: { hidden_text: EMAIL },
      }),
    });
    expect(malformed.status).toBe(400);
    expect(await malformed.text()).not.toContain(EMAIL);
    expect(received).toHaveLength(malformedBaseline);

    const call = () =>
      fetch(`${app!.proxyUrl}/v1/messages`, {
        method: "POST",
        headers: { "content-type": "application/json", "x-api-key": anthCaller },
        body: JSON.stringify({
          model: "pii-anth-e2e",
          max_tokens: 32,
          messages: [
            { role: "user", content: `write to ${EMAIL} please` },
          ],
        }),
      });

    const res = await call();
    expect(res.status).toBe(200);
    const body = (await res.json()) as { content?: Array<{ text?: string }> };
    // Response side masked for the caller…
    expect(body.content?.[0]?.text ?? "").toContain("[EMAIL_REDACTED]");
    expect(JSON.stringify(body)).not.toContain(EMAIL);
    // …and the request side was masked before the upstream.
    const lastReq = received.at(-1);
    const upstreamBlob = JSON.stringify(lastReq);
    expect(upstreamBlob).toContain("[EMAIL_REDACTED]");
    expect(upstreamBlob).not.toContain(EMAIL);

    const beforeRejectedRequest = received.length;
    const rejectedMetadata = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [{ role: "user", content: "clean" }],
        metadata: { user_id: EMAIL },
      }),
    });
    expect(rejectedMetadata.status).toBe(422);
    expect(await rejectedMetadata.text()).not.toContain(EMAIL);
    expect(received).toHaveLength(beforeRejectedRequest);

    const rejected = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: "lookup",
            input_schema: {
              type: "object",
              properties: { [EMAIL]: { type: "string" } },
            },
          },
        ],
      }),
    });
    expect(rejected.status).toBe(422);
    expect(await rejected.text()).not.toContain(EMAIL);
    expect(received).toHaveLength(beforeRejectedRequest);

    const rejectedHistory = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [
          {
            role: "assistant",
            content: [
              {
                type: "tool_use",
                id: "toolu_history",
                name: "lookup",
                input: { [EMAIL]: "safe" },
              },
            ],
          },
        ],
      }),
    });
    expect(rejectedHistory.status).toBe(422);
    expect(await rejectedHistory.text()).not.toContain(EMAIL);
    expect(received).toHaveLength(beforeRejectedRequest);

    const rejectedToolName = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: TOOL_IDENTIFIER,
            input_schema: { type: "object", properties: {} },
          },
        ],
      }),
    });
    expect(rejectedToolName.status).toBe(422);
    expect(await rejectedToolName.text()).not.toContain(TOOL_IDENTIFIER);
    expect(received).toHaveLength(beforeRejectedRequest);

    const rejectedToolChoice = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: "lookup",
            input_schema: { type: "object", properties: {} },
          },
        ],
        tool_choice: { type: "tool", name: TOOL_IDENTIFIER },
      }),
    });
    expect(rejectedToolChoice.status).toBe(422);
    expect(await rejectedToolChoice.text()).not.toContain(TOOL_IDENTIFIER);
    expect(received).toHaveLength(beforeRejectedRequest);

    const rejectedOutputSchema = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [{ role: "user", content: "clean" }],
        output_config: {
          format: {
            type: "json_schema",
            schema: {
              type: "object",
              properties: { [EMAIL]: { type: "string" } },
            },
          },
        },
      }),
    });
    expect(rejectedOutputSchema.status).toBe(422);
    expect(await rejectedOutputSchema.text()).not.toContain(EMAIL);
    expect(received).toHaveLength(beforeRejectedRequest);

    const rejectedHistoricalIdentifier = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [
          {
            role: "assistant",
            content: [
              {
                type: "tool_use",
                id: TOOL_IDENTIFIER,
                name: TOOL_IDENTIFIER,
                input: {},
              },
            ],
          },
        ],
      }),
    });
    expect(rejectedHistoricalIdentifier.status).toBe(422);
    expect(await rejectedHistoricalIdentifier.text()).not.toContain(TOOL_IDENTIFIER);
    expect(received).toHaveLength(beforeRejectedRequest);

    const beforeRejectedOutput = received.length;
    const rejectedOutput = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [{ role: "user", content: ANTHROPIC_OUTPUT_KEY_NONSTREAM }],
      }),
    });
    expect(rejectedOutput.status).toBe(422);
    expect(await rejectedOutput.text()).not.toContain(EMAIL);
    expect(received).toHaveLength(beforeRejectedOutput + 1);
    expect(JSON.stringify(received.at(-1))).toContain(ANTHROPIC_OUTPUT_KEY_NONSTREAM);

    const beforeRejectedStreamOutput = received.length;
    const streamUsageBaseline = await usageStatusCounter(app, "messages", "4xx");
    const rejectedStreamOutput = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        stream: true,
        messages: [{ role: "user", content: ANTHROPIC_OUTPUT_KEY_STREAM }],
      }),
    });
    expect(rejectedStreamOutput.status).toBe(200);
    const streamBody = await rejectedStreamOutput.text();
    expect(streamBody).toContain("content_filter");
    expect(streamBody).not.toContain(EMAIL);
    expect(streamBody).not.toContain("alice@exam");
    expect(streamBody).not.toContain("ple.com");
    expect(streamBody).not.toContain("msg_pii_stream_key");
    expect(streamBody).not.toContain("content_block_start");
    expect(received).toHaveLength(beforeRejectedStreamOutput + 1);
    expect(JSON.stringify(received.at(-1))).toContain(ANTHROPIC_OUTPUT_KEY_STREAM);
    await waitConfigPropagation(
      async () =>
        (await usageStatusCounter(app!, "messages", "4xx")) ===
        streamUsageBaseline + 1,
    );

    const beforeRejectedStartOutput = received.length;
    const startUsageBaseline = await usageStatusCounter(app, "messages", "4xx");
    const rejectedStartOutput = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        stream: true,
        messages: [{ role: "user", content: ANTHROPIC_OUTPUT_KEY_START }],
      }),
    });
    expect(rejectedStartOutput.status).toBe(200);
    const startBody = await rejectedStartOutput.text();
    expect(startBody).toContain("content_filter");
    expect(startBody).not.toContain(EMAIL);
    expect(startBody).not.toContain("msg_pii_start_key");
    expect(startBody).not.toContain("content_block_start");
    expect(received).toHaveLength(beforeRejectedStartOutput + 1);
    expect(JSON.stringify(received.at(-1))).toContain(ANTHROPIC_OUTPUT_KEY_START);
    await waitConfigPropagation(
      async () =>
        (await usageStatusCounter(app!, "messages", "4xx")) ===
        startUsageBaseline + 1,
    );

    const identifierNonStreamBaseline = received.length;
    const identifierNonStream = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        messages: [
          { role: "user", content: ANTHROPIC_OUTPUT_IDENTIFIER_NONSTREAM },
        ],
      }),
    });
    expect(identifierNonStream.status).toBe(422);
    expect(await identifierNonStream.text()).not.toContain(TOOL_IDENTIFIER);
    expect(received).toHaveLength(identifierNonStreamBaseline + 1);

    const identifierStreamBaseline = received.length;
    const identifierStream = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": anthCaller },
      body: JSON.stringify({
        model: "pii-anth-e2e",
        max_tokens: 32,
        stream: true,
        messages: [
          { role: "user", content: ANTHROPIC_OUTPUT_IDENTIFIER_STREAM },
        ],
      }),
    });
    expect(identifierStream.status).toBe(200);
    const identifierStreamBody = await identifierStream.text();
    expect(identifierStreamBody).toContain("content_filter");
    expect(identifierStreamBody).not.toContain(TOOL_IDENTIFIER);
    expect(identifierStreamBody).not.toContain("content_block_start");
    expect(received).toHaveLength(identifierStreamBaseline + 1);
  });

  test("/v1/messages cross-provider rejects sensitive tool keys in non-streaming and streaming output", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    let toolUpstream: OpenAiUpstream | undefined;
    let toolStreamUpstream: OpenAiUpstream | undefined;
    try {
      toolUpstream = await startOpenAiUpstream({
        scriptedResponses: [
          {
            nonStreamBody: {
              id: "cmpl-pii-tool-key",
              object: "chat.completion",
              created: Math.floor(Date.now() / 1000),
              model: "gpt-4o-mini",
              choices: [
                {
                  index: 0,
                  message: {
                    role: "assistant",
                    content: null,
                    tool_calls: [
                      {
                        id: "call_pii",
                        type: "function",
                        function: {
                          name: "lookup",
                          arguments: JSON.stringify({ [EMAIL]: "safe" }),
                        },
                      },
                    ],
                  },
                  finish_reason: "tool_calls",
                },
              ],
              usage: { prompt_tokens: 4, completion_tokens: 6, total_tokens: 10 },
            },
          },
          {
            nonStreamBody: {
              id: "cmpl-pii-tool-identifier",
              object: "chat.completion",
              created: Math.floor(Date.now() / 1000),
              model: "gpt-4o-mini",
              choices: [
                {
                  index: 0,
                  message: {
                    role: "assistant",
                    content: null,
                    tool_calls: [
                      {
                        id: "call_pii_identifier",
                        type: "function",
                        function: { name: TOOL_IDENTIFIER, arguments: "{}" },
                      },
                    ],
                  },
                  finish_reason: "tool_calls",
                },
              ],
              usage: { prompt_tokens: 4, completion_tokens: 6, total_tokens: 10 },
            },
          },
        ],
      });
      toolStreamUpstream = await startOpenAiUpstream({
        scriptedResponses: [
          {
            streamEvents: [
              '{"id":"strm-pii-tool-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}',
              '{"id":"strm-pii-tool-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_pii","type":"function","function":{"name":"lookup","arguments":"{\\"alice@exam"}}]},"finish_reason":null}]}',
              '{"id":"strm-pii-tool-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ple.com\\":\\"safe\\"}"}}]},"finish_reason":null}]}',
              '{"id":"strm-pii-tool-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}',
              "[DONE]",
            ],
          },
          {
            streamEvents: [
              '{"id":"strm-pii-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}',
              `{"id":"strm-pii-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_pii_identifier","type":"function","function":{"name":"${TOOL_IDENTIFIER.slice(0, 13)}","arguments":""}}]},"finish_reason":null}]}`,
              `{"id":"strm-pii-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"${TOOL_IDENTIFIER.slice(13)}","arguments":"{}"}}]},"finish_reason":null}]}`,
              '{"id":"strm-pii-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}',
              "[DONE]",
            ],
          },
        ],
      });

      const nonStreamPk = await seed.createProviderKey({
        display_name: "pii-messages-tool-pk",
        secret: "sk-mock",
        api_base: `${toolUpstream.baseUrl}/v1`,
      });
      const streamPk = await seed.createProviderKey({
        display_name: "pii-messages-tool-stream-pk",
        secret: "sk-mock",
        api_base: `${toolStreamUpstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: "pii-messages-tool",
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: nonStreamPk.id,
      });
      await seed.createModel({
        display_name: "pii-messages-tool-stream",
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: streamPk.id,
      });
      const caller = `${CALLER}-messages-tool`;
      await seed.createApiKey({
        key_hash: hash(caller),
        allowed_models: ["pii-messages-tool", "pii-messages-tool-stream"],
      });

      await waitForModels(app, caller, [
        "pii-messages-tool",
        "pii-messages-tool-stream",
      ]);

      const request = async (model: string, stream: boolean) =>
        fetch(`${app!.proxyUrl}/v1/messages`, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "x-api-key": caller,
          },
          body: JSON.stringify({
            model,
            max_tokens: 32,
            stream,
            messages: [{ role: "user", content: "use the lookup tool" }],
          }),
        });

      const nonStreamBaseline = toolUpstream.receivedRequests.length;
      const nonStream = await request("pii-messages-tool", false);
      const nonStreamBody = await nonStream.text();
      expect(nonStream.status).toBe(422);
      expect(nonStreamBody).toContain("invalid_request_error");
      expect(nonStreamBody).not.toContain(EMAIL);
      expect(toolUpstream.receivedRequests).toHaveLength(nonStreamBaseline + 1);
      expect(toolUpstream.receivedRequests.at(-1)!.body).toContain(
        "use the lookup tool",
      );

      const streamBaseline = toolStreamUpstream.receivedRequests.length;
      const stream = await request("pii-messages-tool-stream", true);
      const streamBody = await stream.text();
      expect(stream.status).toBe(200);
      expect(streamBody).toContain("content_filter");
      expect(streamBody).not.toContain(EMAIL);
      expect(streamBody).not.toContain("alice@exam");
      expect(streamBody).not.toContain("ple.com");
      expect(streamBody).not.toContain("strm-pii-tool-key");
      expect(toolStreamUpstream.receivedRequests).toHaveLength(streamBaseline + 1);
      expect(toolStreamUpstream.receivedRequests.at(-1)!.body).toContain(
        "use the lookup tool",
      );

      const identifierNonStreamBaseline = toolUpstream.receivedRequests.length;
      const identifierNonStream = await request("pii-messages-tool", false);
      const identifierNonStreamBody = await identifierNonStream.text();
      expect(identifierNonStream.status).toBe(422);
      expect(identifierNonStreamBody).toContain("invalid_request_error");
      expect(identifierNonStreamBody).not.toContain(TOOL_IDENTIFIER);
      expect(toolUpstream.receivedRequests).toHaveLength(
        identifierNonStreamBaseline + 1,
      );

      const identifierStreamBaseline = toolStreamUpstream.receivedRequests.length;
      const identifierStream = await request("pii-messages-tool-stream", true);
      const identifierStreamBody = await identifierStream.text();
      expect(identifierStream.status).toBe(200);
      expect(identifierStreamBody).toContain("content_filter");
      expect(identifierStreamBody).not.toContain(TOOL_IDENTIFIER);
      expect(identifierStreamBody).not.toContain("strm-pii-tool-identifier");
      expect(toolStreamUpstream.receivedRequests).toHaveLength(
        identifierStreamBaseline + 1,
      );
    } finally {
      await toolUpstream?.close();
      await toolStreamUpstream?.close();
    }
  });

  test("chat and Responses reject sensitive tool keys across input and output modes", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    let chatUpstream: OpenAiUpstream | undefined;
    let chatStreamUpstream: OpenAiUpstream | undefined;
    let responsesUpstream: OpenAiUpstream | undefined;
    let responsesStreamUpstream: OpenAiUpstream | undefined;
    try {
      chatUpstream = await startOpenAiUpstream({
        nonStreamBody: {
          id: "cmpl-pii-family-key",
          object: "chat.completion",
          created: Math.floor(Date.now() / 1000),
          model: "gpt-4o-mini",
          choices: [
            {
              index: 0,
              message: {
                role: "assistant",
                content: null,
                tool_calls: [
                  {
                    id: "call_pii_family",
                    type: "function",
                    function: {
                      name: "lookup",
                      arguments: JSON.stringify({ [EMAIL]: "safe" }),
                    },
                  },
                ],
              },
              finish_reason: "tool_calls",
            },
          ],
          usage: { prompt_tokens: 4, completion_tokens: 6, total_tokens: 10 },
        },
      });
      chatStreamUpstream = await startOpenAiUpstream({
        streamEvents: [
          '{"id":"strm-pii-family-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}',
          '{"id":"strm-pii-family-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_pii_family","type":"function","function":{"name":"lookup","arguments":"{\\"alice@exam"}}]},"finish_reason":null}]}',
          '{"id":"strm-pii-family-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ple.com\\":\\"safe\\"}"}}]},"finish_reason":null}]}',
          '{"id":"strm-pii-family-key","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}',
          "[DONE]",
        ],
      });
      responsesUpstream = await startOpenAiUpstream({
        nonStreamBody: {
          id: "resp_pii_family_key",
          object: "response",
          status: "completed",
          model: "gpt-4o-mini",
          output: [
            {
              type: "function_call",
              id: "fc_pii_family",
              call_id: "call_pii_family",
              name: "lookup",
              arguments: JSON.stringify({ [EMAIL]: "safe" }),
            },
          ],
          usage: { input_tokens: 4, output_tokens: 6, total_tokens: 10 },
        },
      });
      responsesStreamUpstream = await startOpenAiUpstream({
        streamEvents: [
          JSON.stringify({
            type: "response.created",
            response: { id: "resp_pii_family_stream", status: "in_progress" },
          }),
          JSON.stringify({
            type: "response.function_call_arguments.delta",
            item_id: "fc_pii_family",
            output_index: 0,
            delta: '{"alice@exam',
          }),
          JSON.stringify({
            type: "response.function_call_arguments.delta",
            item_id: "fc_pii_family",
            output_index: 0,
            delta: 'ple.com":"safe"}',
          }),
          JSON.stringify({
            type: "response.completed",
            response: {
              id: "resp_pii_family_stream",
              status: "completed",
              output: [],
              usage: { input_tokens: 4, output_tokens: 6, total_tokens: 10 },
            },
          }),
          "[DONE]",
        ],
      });

      const models = [
        ["pii-chat-tool", "openai", chatUpstream],
        ["pii-chat-tool-stream", "openai", chatStreamUpstream],
        ["pii-responses-tool", "openai", responsesUpstream],
        ["pii-responses-tool-stream", "openai", responsesStreamUpstream],
        ["pii-responses-bridge-tool", "deepseek", chatUpstream],
        ["pii-responses-bridge-tool-stream", "deepseek", chatStreamUpstream],
      ] as const;
      for (const [name, provider, target] of models) {
        const pk = await seed.createProviderKey({
          display_name: `${name}-pk`,
          secret: "sk-mock",
          api_base: `${target.baseUrl}/v1`,
          provider,
          adapter: "openai",
        });
        await seed.createModel({
          display_name: name,
          provider,
          model_name: "gpt-4o-mini",
          provider_key_id: pk.id,
        });
      }
      const caller = `${CALLER}-family-tool`;
      const modelNames = models.map(([name]) => name);
      await seed.createApiKey({
        key_hash: hash(caller),
        allowed_models: modelNames,
      });
      await waitForModels(app, caller, modelNames);

      const post = (path: string, body: unknown) =>
        fetch(`${app!.proxyUrl}${path}`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${caller}`,
            "content-type": "application/json",
          },
          body: JSON.stringify(body),
        });

      const chatMetadataBaseline = chatUpstream.receivedRequests.length;
      const rejectedChatMetadata = await post("/v1/chat/completions", {
        model: "pii-chat-tool",
        messages: [{ role: "user", content: "clean" }],
        user: EMAIL,
        metadata: { tenant: EMAIL },
      });
      expect(rejectedChatMetadata.status).toBe(422);
      expect(await rejectedChatMetadata.text()).not.toContain(EMAIL);
      expect(chatUpstream.receivedRequests).toHaveLength(chatMetadataBaseline);

      const responsesMetadataBaseline = responsesUpstream.receivedRequests.length;
      const rejectedResponsesMetadata = await post("/v1/responses", {
        model: "pii-responses-tool",
        input: "clean",
        safety_identifier: EMAIL,
        metadata: { tenant: EMAIL },
      });
      expect(rejectedResponsesMetadata.status).toBe(422);
      expect(await rejectedResponsesMetadata.text()).not.toContain(EMAIL);
      expect(responsesUpstream.receivedRequests).toHaveLength(
        responsesMetadataBaseline,
      );

      const chatTools = (
        property: string,
        defaultValue: string,
        name = "lookup",
      ) => [
        {
          type: "function",
          function: {
            name,
            description: `contact ${defaultValue}`,
            parameters: {
              type: "object",
              properties: { [property]: { type: "string", default: defaultValue } },
            },
          },
        },
      ];
      const responseTools = (
        property: string,
        defaultValue: string,
        name = "lookup",
      ) => [
        {
          type: "function",
          name,
          description: `contact ${defaultValue}`,
          parameters: {
            type: "object",
            properties: { [property]: { type: "string", default: defaultValue } },
          },
        },
      ];

      const toolDefinitionCases = [
        {
          path: "/v1/chat/completions",
          model: "pii-chat-tool",
          upstream: chatUpstream,
          request: (tools: unknown) => ({
            model: "pii-chat-tool",
            messages: [{ role: "user", content: "chat tool definition marker" }],
            tools,
          }),
          choiceRequest: (name: string) => ({
            model: "pii-chat-tool",
            messages: [{ role: "user", content: "chat tool choice marker" }],
            tools: chatTools("owner", "safe"),
            tool_choice: { type: "function", function: { name } },
          }),
          schemaRequest: (property: string) => ({
            model: "pii-chat-tool",
            messages: [{ role: "user", content: "chat schema marker" }],
            response_format: {
              type: "json_schema",
              json_schema: {
                name: "result",
                schema: {
                  type: "object",
                  properties: { [property]: { type: "string" } },
                },
              },
            },
          }),
          tools: chatTools,
        },
        {
          path: "/v1/responses",
          model: "pii-responses-tool",
          upstream: responsesUpstream,
          request: (tools: unknown) => ({
            model: "pii-responses-tool",
            input: "native Responses tool definition marker",
            tools,
          }),
          choiceRequest: (name: string) => ({
            model: "pii-responses-tool",
            input: "native Responses tool choice marker",
            tools: responseTools("owner", "safe"),
            tool_choice: { type: "function", name },
          }),
          schemaRequest: (property: string) => ({
            model: "pii-responses-tool",
            input: "native Responses schema marker",
            text: {
              format: {
                type: "json_schema",
                name: "result",
                schema: {
                  type: "object",
                  properties: { [property]: { type: "string" } },
                },
              },
            },
          }),
          tools: responseTools,
        },
        {
          path: "/v1/responses",
          model: "pii-responses-bridge-tool",
          upstream: chatUpstream,
          request: (tools: unknown) => ({
            model: "pii-responses-bridge-tool",
            input: "translated Responses tool definition marker",
            tools,
          }),
          choiceRequest: (name: string) => ({
            model: "pii-responses-bridge-tool",
            input: "translated Responses tool choice marker",
            tools: responseTools("owner", "safe"),
            tool_choice: { type: "function", name },
          }),
          schemaRequest: (property: string) => ({
            model: "pii-responses-bridge-tool",
            input: "translated Responses schema marker",
            text: {
              format: {
                type: "json_schema",
                name: "result",
                schema: {
                  type: "object",
                  properties: { [property]: { type: "string" } },
                },
              },
            },
          }),
          tools: responseTools,
        },
      ] as const;

      for (const toolCase of toolDefinitionCases) {
        const valueBaseline = toolCase.upstream.receivedRequests.length;
        const masked = await post(
          toolCase.path,
          toolCase.request(toolCase.tools("owner", EMAIL)),
        );
        // These fixtures deliberately return a sensitive output key, so the
        // terminal 422 is output-side. The exact +1 upstream assertion proves
        // the input definition passed after being masked.
        expect(masked.status).toBe(422);
        expect(await masked.text()).not.toContain(EMAIL);
        expect(toolCase.upstream.receivedRequests).toHaveLength(valueBaseline + 1);
        const forwarded = toolCase.upstream.receivedRequests.at(-1)!.body;
        expect(forwarded).not.toContain(EMAIL);
        expect(forwarded).toContain("[EMAIL_REDACTED]");

        const keyBaseline = toolCase.upstream.receivedRequests.length;
        const rejected = await post(
          toolCase.path,
          toolCase.request(toolCase.tools(EMAIL, "safe")),
        );
        expect(rejected.status).toBe(422);
        expect(await rejected.text()).not.toContain(EMAIL);
        expect(toolCase.upstream.receivedRequests).toHaveLength(keyBaseline);

        const nameBaseline = toolCase.upstream.receivedRequests.length;
        const rejectedName = await post(
          toolCase.path,
          toolCase.request(toolCase.tools("owner", "safe", TOOL_IDENTIFIER)),
        );
        expect(rejectedName.status).toBe(422);
        expect(await rejectedName.text()).not.toContain(TOOL_IDENTIFIER);
        expect(toolCase.upstream.receivedRequests).toHaveLength(nameBaseline);

        const choiceBaseline = toolCase.upstream.receivedRequests.length;
        const rejectedChoice = await post(
          toolCase.path,
          toolCase.choiceRequest(TOOL_IDENTIFIER),
        );
        expect(rejectedChoice.status).toBe(422);
        expect(await rejectedChoice.text()).not.toContain(TOOL_IDENTIFIER);
        expect(toolCase.upstream.receivedRequests).toHaveLength(choiceBaseline);

        const schemaBaseline = toolCase.upstream.receivedRequests.length;
        const rejectedSchema = await post(
          toolCase.path,
          toolCase.schemaRequest(EMAIL),
        );
        expect(rejectedSchema.status).toBe(422);
        expect(await rejectedSchema.text()).not.toContain(EMAIL);
        expect(toolCase.upstream.receivedRequests).toHaveLength(schemaBaseline);
      }

      const chatInputBaseline = chatUpstream.receivedRequests.length;
      const chatInput = await post("/v1/chat/completions", {
        model: "pii-chat-tool",
        messages: [
          {
            role: "assistant",
            content: null,
            tool_calls: [
              {
                id: "call_history",
                type: "function",
                function: {
                  name: "lookup",
                  arguments: JSON.stringify({ [EMAIL]: "safe" }),
                },
              },
            ],
          },
        ],
      });
      expect(chatInput.status).toBe(422);
      expect(await chatInput.text()).not.toContain(EMAIL);
      expect(chatUpstream.receivedRequests).toHaveLength(chatInputBaseline);

      const chatIdentifierInput = await post("/v1/chat/completions", {
        model: "pii-chat-tool",
        messages: [
          {
            role: "assistant",
            content: null,
            tool_calls: [
              {
                id: TOOL_IDENTIFIER,
                type: "function",
                function: { name: TOOL_IDENTIFIER, arguments: "{}" },
              },
            ],
          },
        ],
      });
      expect(chatIdentifierInput.status).toBe(422);
      expect(await chatIdentifierInput.text()).not.toContain(TOOL_IDENTIFIER);
      expect(chatUpstream.receivedRequests).toHaveLength(chatInputBaseline);

      const responsesInputBaseline = responsesUpstream.receivedRequests.length;
      const responsesInput = await post("/v1/responses", {
        model: "pii-responses-tool",
        input: [
          {
            type: "function_call",
            call_id: "call_history",
            name: "lookup",
            arguments: JSON.stringify({ [EMAIL]: "safe" }),
          },
        ],
      });
      expect(responsesInput.status).toBe(422);
      expect(await responsesInput.text()).not.toContain(EMAIL);
      expect(responsesUpstream.receivedRequests).toHaveLength(responsesInputBaseline);

      const responsesIdentifierInput = await post("/v1/responses", {
        model: "pii-responses-tool",
        input: [
          {
            type: "function_call",
            call_id: "call_history_identifier",
            name: TOOL_IDENTIFIER,
            arguments: "{}",
          },
        ],
      });
      expect(responsesIdentifierInput.status).toBe(422);
      expect(await responsesIdentifierInput.text()).not.toContain(TOOL_IDENTIFIER);
      expect(responsesUpstream.receivedRequests).toHaveLength(responsesInputBaseline);

      const responsesApprovalInput = await post("/v1/responses", {
        model: "pii-responses-tool",
        input: [
          {
            type: "mcp_approval_response",
            approval_request_id: TOOL_IDENTIFIER,
            approve: true,
            reason: "safe",
          },
        ],
      });
      expect(responsesApprovalInput.status).toBe(422);
      expect(await responsesApprovalInput.text()).not.toContain(TOOL_IDENTIFIER);
      expect(responsesUpstream.receivedRequests).toHaveLength(responsesInputBaseline);

      const nativeResultBaseline = responsesUpstream.receivedRequests.length;
      const nativeResult = await post("/v1/responses", {
        model: "pii-responses-tool",
        input: [
          {
            type: "function_call_output",
            call_id: "call_native_result",
            output: [{ type: "input_text", text: `array result ${EMAIL}` }],
          },
          {
            type: "custom_tool_call_output",
            call_id: "call_custom_result",
            output: `custom result ${EMAIL}`,
          },
          {
            type: "mcp_approval_response",
            approval_request_id: "approval_result",
            approve: true,
            reason: `approved for ${EMAIL}`,
          },
        ],
      });
      expect(nativeResult.status).toBe(422);
      expect(await nativeResult.text()).not.toContain(EMAIL);
      expect(responsesUpstream.receivedRequests).toHaveLength(nativeResultBaseline + 1);
      const nativeForwarded = responsesUpstream.receivedRequests.at(-1)!.body;
      expect(nativeForwarded).not.toContain(EMAIL);
      expect(nativeForwarded.match(/\[EMAIL_REDACTED\]/g)?.length).toBe(3);

      const bridgeResultBaseline = chatUpstream.receivedRequests.length;
      const bridgeResult = await post("/v1/responses", {
        model: "pii-responses-bridge-tool",
        input: [
          {
            type: "function_call_output",
            call_id: "call_bridge_result",
            output: [{ type: "input_text", text: `bridge result ${EMAIL}` }],
          },
        ],
      });
      expect(bridgeResult.status).toBe(422);
      expect(await bridgeResult.text()).not.toContain(EMAIL);
      expect(chatUpstream.receivedRequests).toHaveLength(bridgeResultBaseline + 1);
      const bridgeForwarded = chatUpstream.receivedRequests.at(-1)!.body;
      expect(bridgeForwarded).not.toContain(EMAIL);
      expect(bridgeForwarded).toContain("[EMAIL_REDACTED]");

      const outputCases = [
        {
          path: "/v1/chat/completions",
          model: "pii-chat-tool",
          stream: false,
          upstream: chatUpstream,
          marker: "chat-tool-output-marker",
          expectedStatus: 422,
        },
        {
          path: "/v1/chat/completions",
          model: "pii-chat-tool-stream",
          stream: true,
          upstream: chatStreamUpstream,
          marker: "chat-tool-stream-output-marker",
          expectedStatus: 200,
        },
        {
          path: "/v1/responses",
          model: "pii-responses-tool",
          stream: false,
          upstream: responsesUpstream,
          marker: "responses-tool-output-marker",
          expectedStatus: 422,
        },
        {
          path: "/v1/responses",
          model: "pii-responses-tool-stream",
          stream: true,
          upstream: responsesStreamUpstream,
          marker: "responses-tool-stream-output-marker",
          expectedStatus: 422,
        },
        {
          path: "/v1/responses",
          model: "pii-responses-bridge-tool",
          stream: false,
          upstream: chatUpstream,
          marker: "responses-bridge-tool-output-marker",
          expectedStatus: 422,
        },
        {
          path: "/v1/responses",
          model: "pii-responses-bridge-tool-stream",
          stream: true,
          upstream: chatStreamUpstream,
          marker: "responses-bridge-tool-stream-output-marker",
          expectedStatus: 200,
        },
      ] as const;
      for (const outputCase of outputCases) {
        const baseline = outputCase.upstream.receivedRequests.length;
        const handler = outputCase.path === "/v1/chat/completions" ? "chat" : "responses";
        const usageBaseline = await usageStatusCounter(app, handler, "4xx");
        const body =
          outputCase.path === "/v1/chat/completions"
            ? {
                model: outputCase.model,
                stream: outputCase.stream,
                messages: [{ role: "user", content: outputCase.marker }],
              }
            : {
                model: outputCase.model,
                stream: outputCase.stream,
                input: outputCase.marker,
              };
        const response = await post(outputCase.path, body);
        const responseBody = await response.text();
        expect(response.status).toBe(outputCase.expectedStatus);
        expect(responseBody).toContain("content_filter");
        expect(responseBody).not.toContain(EMAIL);
        expect(responseBody).not.toContain("alice@exam");
        expect(responseBody).not.toContain("ple.com");
        expect(outputCase.upstream.receivedRequests).toHaveLength(baseline + 1);
        const received = outputCase.upstream.receivedRequests.at(-1)!;
        expect(received.body).toContain(outputCase.marker);
        expect(received.path).toBe(
          outputCase.path === "/v1/responses" &&
            outputCase.model.includes("bridge")
            ? "/v1/chat/completions"
            : outputCase.path,
        );
        await waitConfigPropagation(
          async () =>
            (await usageStatusCounter(app!, handler, "4xx")) === usageBaseline + 1,
        );
      }
    } finally {
      await chatUpstream?.close();
      await chatStreamUpstream?.close();
      await responsesUpstream?.close();
      await responsesStreamUpstream?.close();
    }
  });

  test("chat and Responses reject mask-matching output tool names", async (ctx) => {
    if (!etcdReachable || !app || !seed) {
      ctx.skip();
      return;
    }

    let chatIdentifierUpstream: OpenAiUpstream | undefined;
    let responsesIdentifierUpstream: OpenAiUpstream | undefined;
    try {
      chatIdentifierUpstream = await startOpenAiUpstream({
        scriptedResponses: [
          {
            nonStreamBody: {
              id: "cmpl-tool-identifier",
              object: "chat.completion",
              created: Math.floor(Date.now() / 1000),
              model: "gpt-4o-mini",
              choices: [
                {
                  index: 0,
                  message: {
                    role: "assistant",
                    content: null,
                    tool_calls: [
                      {
                        id: "call_tool_identifier",
                        type: "function",
                        function: { name: TOOL_IDENTIFIER, arguments: "{}" },
                      },
                    ],
                  },
                  finish_reason: "tool_calls",
                },
              ],
              usage: { prompt_tokens: 4, completion_tokens: 6, total_tokens: 10 },
            },
          },
          {
            streamEvents: [
              '{"id":"strm-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}',
              `{"id":"strm-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_tool_identifier","type":"function","function":{"name":"${TOOL_IDENTIFIER.slice(0, 13)}","arguments":""}}]},"finish_reason":null}]}`,
              `{"id":"strm-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"${TOOL_IDENTIFIER.slice(13)}","arguments":"{}"}}]},"finish_reason":null}]}`,
              '{"id":"strm-tool-identifier","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}',
              "[DONE]",
            ],
          },
        ],
      });
      responsesIdentifierUpstream = await startOpenAiUpstream({
        scriptedResponses: [
          {
            nonStreamBody: {
              id: "resp_tool_identifier",
              object: "response",
              status: "completed",
              model: "gpt-4o-mini",
              output: [
                {
                  type: "function_call",
                  id: "fc_tool_identifier",
                  call_id: "call_tool_identifier",
                  name: TOOL_IDENTIFIER,
                  arguments: "{}",
                },
              ],
              usage: { input_tokens: 4, output_tokens: 6, total_tokens: 10 },
            },
          },
          {
            streamEvents: [
              JSON.stringify({
                type: "response.created",
                response: { id: "resp_tool_identifier_stream", status: "in_progress" },
              }),
              JSON.stringify({
                type: "response.output_item.added",
                output_index: 0,
                item: {
                  type: "function_call",
                  id: "fc_tool_identifier_stream",
                  call_id: "call_tool_identifier_stream",
                  name: TOOL_IDENTIFIER,
                  arguments: "{}",
                },
              }),
              JSON.stringify({
                type: "response.completed",
                response: {
                  id: "resp_tool_identifier_stream",
                  status: "completed",
                  output: [],
                  usage: { input_tokens: 4, output_tokens: 6, total_tokens: 10 },
                },
              }),
              "[DONE]",
            ],
          },
        ],
      });

      const models = [
        ["pii-chat-identifier-output", chatIdentifierUpstream],
        ["pii-responses-identifier-output", responsesIdentifierUpstream],
      ] as const;
      for (const [name, target] of models) {
        const pk = await seed.createProviderKey({
          display_name: `${name}-pk`,
          secret: "sk-mock",
          api_base: `${target.baseUrl}/v1`,
        });
        await seed.createModel({
          display_name: name,
          provider: "openai",
          model_name: "gpt-4o-mini",
          provider_key_id: pk.id,
        });
      }
      const caller = `${CALLER}-tool-identifier-output`;
      const modelNames = models.map(([name]) => name);
      await seed.createApiKey({
        key_hash: hash(caller),
        allowed_models: modelNames,
      });
      await waitForModels(app, caller, modelNames);

      const cases = [
        {
          path: "/v1/chat/completions",
          model: "pii-chat-identifier-output",
          upstream: chatIdentifierUpstream,
          statuses: [422, 200],
        },
        {
          path: "/v1/responses",
          model: "pii-responses-identifier-output",
          upstream: responsesIdentifierUpstream,
          statuses: [422, 422],
        },
      ] as const;
      for (const outputCase of cases) {
        for (const [index, stream] of [false, true].entries()) {
          const baseline = outputCase.upstream.receivedRequests.length;
          const body =
            outputCase.path === "/v1/chat/completions"
              ? {
                  model: outputCase.model,
                  stream,
                  messages: [
                    { role: "user", content: `tool identifier output ${stream}` },
                  ],
                }
              : {
                  model: outputCase.model,
                  stream,
                  input: `tool identifier output ${stream}`,
                };
          const response = await fetch(`${app.proxyUrl}${outputCase.path}`, {
            method: "POST",
            headers: {
              authorization: `Bearer ${caller}`,
              "content-type": "application/json",
            },
            body: JSON.stringify(body),
          });
          const responseBody = await response.text();
          expect(response.status).toBe(outputCase.statuses[index]);
          expect(responseBody).toContain("content_filter");
          expect(responseBody).not.toContain(TOOL_IDENTIFIER);
          expect(responseBody).not.toContain("tool_identifier");
          expect(outputCase.upstream.receivedRequests).toHaveLength(baseline + 1);
          expect(outputCase.upstream.receivedRequests.at(-1)!.body).toContain(
            `tool identifier output ${stream}`,
          );
        }
      }
    } finally {
      await chatIdentifierUpstream?.close();
      await responsesIdentifierUpstream?.close();
    }
  });
});
