import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  decodedTextFor,
  EtcdClient,
  ProxyClient,
  SeedClient,
  pickFreePort,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: `mandatory: true` on a guardrail that MASKS.
//
// `mandatory` is documented as changing one thing: a remote guardrail that
// cannot reach its upstream blocks the request instead of failing open. It
// says nothing about what happens when the guardrail CAN evaluate, so a
// mandatory masking guardrail must keep masking exactly as it would without
// the flag — on every endpoint, on the request and on the (streamed)
// response — and a mandatory monitor-mode guardrail must keep reporting what
// it would have masked. Only an outage of the guardrail service changes the
// outcome: the request is refused instead of passing unscanned.
//
// The guardrail under test is a Presidio PII anonymizer (analyze → anonymize,
// <https://microsoft.github.io/presidio/api-docs/api-docs.html>) masking
// e-mail addresses; the mock below implements both calls and answers 503 for
// any text carrying OUTAGE.

const EMAIL = "alice@example.com";
const MASK = "<EMAIL_ADDRESS>";
const OUTAGE = "presidiooutagemarker";
const hash = (s: string) => createHash("sha256").update(s).digest("hex");

interface PresidioMock {
  baseUrl: string;
  close(): Promise<void>;
}

async function startPresidioMock(): Promise<PresidioMock> {
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      const body = JSON.parse(raw || "{}") as Record<string, unknown>;
      const text = typeof body.text === "string" ? body.text : "";
      if (req.url?.startsWith("/analyze")) {
        if (text.includes(OUTAGE)) {
          res.statusCode = 503;
          res.end("mock presidio outage");
          return;
        }
        const results: Array<Record<string, unknown>> = [];
        for (let at = text.indexOf(EMAIL); at >= 0; at = text.indexOf(EMAIL, at + 1)) {
          results.push({ entity_type: "EMAIL_ADDRESS", start: at, end: at + EMAIL.length, score: 0.85 });
        }
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(results));
        return;
      }
      if (req.url?.startsWith("/anonymize")) {
        const spans = (body.analyzer_results ?? []) as Array<{ entity_type: string; start: number; end: number }>;
        let out = text;
        const items: Array<Record<string, unknown>> = [];
        for (const r of [...spans].sort((a, b) => b.start - a.start)) {
          const replacement = `<${r.entity_type}>`;
          out = out.slice(0, r.start) + replacement + out.slice(r.end);
          items.push({
            operator: "replace",
            entity_type: r.entity_type,
            start: r.start,
            end: r.start + replacement.length,
            text: replacement,
          });
        }
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify({ text: out, items }));
        return;
      }
      res.statusCode = 404;
      res.end("unknown path");
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

const presidioGuardrail = (name: string, baseUrl: string, extra: Record<string, unknown>) => ({
  name,
  enabled: true,
  fail_open: true,
  mandatory: true,
  kind: "presidio",
  analyzer_url: baseUrl,
  anonymizer_url: baseUrl,
  entities: [{ type: "EMAIL_ADDRESS", action: "mask" }],
  default_action: "mask",
  operator: "replace",
  language: "en",
  score_threshold: 0.5,
  ...extra,
});

// The upstream model replies with the e-mail address too, so the response
// side has something to mask.
const REPLY = `you can reach the customer at ${EMAIL} today`;
const CHAT_BODY = {
  id: "cmpl-mandatory",
  object: "chat.completion",
  created: 1_700_000_000,
  model: "gpt-4o-mini",
  choices: [{ index: 0, message: { role: "assistant", content: REPLY }, finish_reason: "stop" }],
  usage: { prompt_tokens: 9, completion_tokens: 9, total_tokens: 18 },
};
const RESPONSES_BODY = {
  id: "resp-mandatory",
  object: "response",
  created_at: 1_700_000_000,
  status: "completed",
  model: "gpt-4o-mini",
  output: [
    {
      type: "message",
      id: "msg-mandatory",
      status: "completed",
      role: "assistant",
      content: [{ type: "output_text", text: REPLY, annotations: [] }],
    },
  ],
  usage: { input_tokens: 9, output_tokens: 9, total_tokens: 18 },
};
const COMPLETIONS_BODY = {
  id: "cmpl-legacy-mandatory",
  object: "text_completion",
  created: 1_700_000_000,
  model: "gpt-4o-mini",
  choices: [{ index: 0, text: REPLY, finish_reason: "stop", logprobs: null }],
  usage: { prompt_tokens: 9, completion_tokens: 9, total_tokens: 18 },
};
// A Responses-API stream whose output text splits the address across two
// deltas (<https://platform.openai.com/docs/api-reference/responses-streaming>).
const RESPONSES_STREAM_EVENTS = [
  { type: "response.created", response: { ...RESPONSES_BODY, status: "in_progress", output: [] } },
  { type: "response.output_item.added", output_index: 0, item: { ...RESPONSES_BODY.output[0], status: "in_progress", content: [] } },
  { type: "response.content_part.added", item_id: "msg-mandatory", output_index: 0, content_index: 0, part: { type: "output_text", text: "", annotations: [] } },
  { type: "response.output_text.delta", item_id: "msg-mandatory", output_index: 0, content_index: 0, delta: "mail alice@exam" },
  { type: "response.output_text.delta", item_id: "msg-mandatory", output_index: 0, content_index: 0, delta: "ple.com now" },
  { type: "response.output_text.done", item_id: "msg-mandatory", output_index: 0, content_index: 0, text: `mail ${EMAIL} now` },
  {
    type: "response.completed",
    response: {
      ...RESPONSES_BODY,
      output: [{ ...RESPONSES_BODY.output[0], content: [{ type: "output_text", text: `mail ${EMAIL} now`, annotations: [] }] }],
    },
  },
].map((e, i) => JSON.stringify({ ...e, sequence_number: i }));
const STREAM_EVENTS = [
  '{"id":"strm-mandatory","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}',
  '{"id":"strm-mandatory","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"mail alice@exam"},"finish_reason":null}]}',
  '{"id":"strm-mandatory","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"ple.com now"},"finish_reason":null}]}',
  '{"id":"strm-mandatory","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}',
  "[DONE]",
];

/** Data payloads of a complete `text/event-stream` body, `[DONE]` excluded. */
function sseData(body: string): Array<Record<string, unknown>> {
  return body
    .replace(/\r\n/g, "\n")
    .split("\n\n")
    .flatMap((block) => {
      const data = block
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).trim())
        .join("\n");
      return data && data !== "[DONE]" ? [JSON.parse(data) as Record<string, unknown>] : [];
    });
}

describe("a mandatory masking guardrail masks, and blocks only when it cannot evaluate", () => {
  const CALLER = "sk-mandatory-mask-caller";
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let outageUpstream: OpenAiUpstream | undefined;
  let responsesStreamUpstream: OpenAiUpstream | undefined;
  let claudeUpstream: OpenAiUpstream | undefined;
  let presidio: PresidioMock | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    presidio = await startPresidioMock();
    upstream = await startOpenAiUpstream({
      pathBodies: { "/chat/completions": CHAT_BODY, "/responses": RESPONSES_BODY, "/v1/completions": COMPLETIONS_BODY },
    });
    responsesStreamUpstream = await startOpenAiUpstream({ streamEvents: RESPONSES_STREAM_EVENTS, eventDelayMs: 20 });
    // An Anthropic Messages upstream, for /v1/messages/count_tokens.
    claudeUpstream = await startOpenAiUpstream({ nonStreamBody: { input_tokens: 21 } });
    streamUpstream = await startOpenAiUpstream({ streamEvents: STREAM_EVENTS, eventDelayMs: 20 });
    // Replies with text the guardrail service cannot scan.
    outageUpstream = await startOpenAiUpstream({
      nonStreamBody: {
        ...CHAT_BODY,
        choices: [{ ...CHAT_BODY.choices[0], message: { role: "assistant", content: `${OUTAGE} ${REPLY}` } }],
      },
    });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "mandatory-mask-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "mandatory-mask-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    const streamPk = await seed.createProviderKey({
      display_name: "mandatory-mask-stream-pk",
      secret: "sk-mock",
      api_base: `${streamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "mandatory-mask-stream-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: streamPk.id,
    });
    const outagePk = await seed.createProviderKey({
      display_name: "mandatory-mask-outage-pk",
      secret: "sk-mock",
      api_base: `${outageUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "mandatory-mask-outage-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: outagePk.id,
    });
    const responsesStreamPk = await seed.createProviderKey({
      display_name: "mandatory-mask-responses-stream-pk",
      secret: "sk-mock",
      api_base: `${responsesStreamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "mandatory-mask-responses-stream-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: responsesStreamPk.id,
    });
    const claudePk = await seed.createProviderKey({
      display_name: "mandatory-mask-claude-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: claudeUpstream.baseUrl,
    });
    await seed.createModel({
      display_name: "mandatory-mask-claude",
      provider: "anthropic",
      model_name: "claude-sonnet-4-5",
      provider_key_id: claudePk.id,
    });
    // Fail-open on both hooks: without `mandatory`, an outage would let the
    // request and the response through unscanned.
    await seed.createGuardrail(
      presidioGuardrail("mandatory-presidio", presidio.baseUrl, { hook_point: "both", output_fail_open: true }),
    );
    // Caller key last: once it authenticates, every resource above is in
    // the snapshot.
    await seed.createApiKey({
      key_hash: hash(CALLER),
      allowed_models: [
        "mandatory-mask-model",
        "mandatory-mask-stream-model",
        "mandatory-mask-outage-model",
        "mandatory-mask-responses-stream-model",
        "mandatory-mask-claude",
      ],
    });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await streamUpstream?.close();
    await outageUpstream?.close();
    await responsesStreamUpstream?.close();
    await claudeUpstream?.close();
    await presidio?.close();
  });

  async function post(path: string, body: Record<string, unknown>, headers: Record<string, string> = {}) {
    const baseline = upstream!.receivedRequests.length;
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}`, ...headers },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    return { status: res.status, text, sent: upstream!.receivedRequests.slice(baseline) };
  }

  /** The prompt reached the upstream masked. */
  function expectMaskedUpstreamPrompt(sent: Array<{ body: string }>): void {
    expect(sent).toHaveLength(1);
    expect(sent[0]!.body).toContain(MASK);
    expect(sent[0]!.body).toContain("about the order");
    expect(sent[0]!.body).not.toContain(EMAIL);
  }

  const PROMPT = `contact me at ${EMAIL} about the order`;

  test("/v1/chat/completions: request and response PII are masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await post("/v1/chat/completions", {
      model: "mandatory-mask-model",
      messages: [{ role: "user", content: PROMPT }],
    });
    expect(status, text).toBe(200);
    expectMaskedUpstreamPrompt(sent);
    const reply = (JSON.parse(text) as { choices: Array<{ message: { content: string } }> }).choices[0]!.message.content;
    expect(reply).toContain(MASK);
    expect(reply).not.toContain(EMAIL);
  });

  test("/v1/chat/completions stream: a response span split across chunks is masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model: "mandatory-mask-stream-model",
        stream: true,
        messages: [{ role: "user", content: "how do I reach the customer?" }],
      }),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    const content = sseData(text)
      .map((c) => (c.choices as Array<{ delta?: { content?: string } }> | undefined)?.[0]?.delta?.content ?? "")
      .join("");
    expect(content).toContain(MASK);
    expect(content).not.toContain(EMAIL);
    expect(text).not.toContain("alice@exam");
  });

  test("/v1/messages: request and response PII are masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await post(
      "/v1/messages",
      { model: "mandatory-mask-model", max_tokens: 256, messages: [{ role: "user", content: PROMPT }] },
      { "x-api-key": CALLER, "anthropic-version": "2023-06-01" },
    );
    expect(status, text).toBe(200);
    expectMaskedUpstreamPrompt(sent);
    const reply = (JSON.parse(text) as { content: Array<{ text?: string }> }).content.map((b) => b.text ?? "").join("");
    expect(reply).toContain(MASK);
    expect(reply).not.toContain(EMAIL);
  });

  test("/v1/messages stream: a response span split across chunks is masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER, "anthropic-version": "2023-06-01" },
      body: JSON.stringify({
        model: "mandatory-mask-stream-model",
        max_tokens: 256,
        stream: true,
        messages: [{ role: "user", content: "how do I reach the customer?" }],
      }),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    const content = sseData(text)
      .map((e) => (e.delta as { text?: string } | undefined)?.text ?? "")
      .join("");
    expect(content).toContain(MASK);
    expect(text).not.toContain(EMAIL);
    expect(text).not.toContain("alice@exam");
  });

  test("/v1/responses stream: a response span split across deltas is masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await fetch(`${app.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model: "mandatory-mask-responses-stream-model",
        stream: true,
        input: "how do I reach the customer?",
      }),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    const deltas = sseData(text)
      .filter((e) => e.type === "response.output_text.delta")
      .map((e) => e.delta as string)
      .join("");
    expect(deltas).toContain(MASK);
    expect(text).not.toContain(EMAIL);
    expect(text).not.toContain("alice@exam");
  });

  test("/v1/completions: request and response PII are masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await post("/v1/completions", { model: "mandatory-mask-model", prompt: PROMPT });
    expect(status, text).toBe(200);
    expectMaskedUpstreamPrompt(sent);
    const reply = (JSON.parse(text) as { choices: Array<{ text: string }> }).choices[0]!.text;
    expect(reply).toContain(MASK);
    expect(reply).not.toContain(EMAIL);
  });

  test("/v1/messages/count_tokens: the prompt is counted masked, not refused", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const baseline = claudeUpstream!.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER, "anthropic-version": "2023-06-01" },
      body: JSON.stringify({ model: "mandatory-mask-claude", messages: [{ role: "user", content: PROMPT }] }),
    });
    const text = await res.text();
    expect(res.status, text).toBe(200);
    expect((JSON.parse(text) as { input_tokens: number }).input_tokens).toBe(21);
    const sent = claudeUpstream!.receivedRequests.slice(baseline);
    expectMaskedUpstreamPrompt(sent);
  });

  test("/v1/responses: request and response PII are masked, not blocked", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await post("/v1/responses", { model: "mandatory-mask-model", input: PROMPT });
    expect(status, text).toBe(200);
    expectMaskedUpstreamPrompt(sent);
    const output = (JSON.parse(text) as { output: Array<{ content?: Array<{ text?: string }> }> }).output;
    const reply = output.flatMap((o) => o.content ?? []).map((c) => c.text ?? "").join("");
    expect(reply).toContain(MASK);
    expect(reply).not.toContain(EMAIL);
  });

  test("an unreachable guardrail blocks: mandatory overrides fail_open", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, sent } = await post("/v1/chat/completions", {
      model: "mandatory-mask-model",
      messages: [{ role: "user", content: `${OUTAGE} ${PROMPT}` }],
    });
    expect(status).toBe(422);
    expect(sent).toHaveLength(0);
  });

  test("a guardrail that cannot scan the response blocks it: mandatory overrides output_fail_open", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text } = await post("/v1/chat/completions", {
      model: "mandatory-mask-outage-model",
      messages: [{ role: "user", content: "how do I reach the customer?" }],
    });
    expect(status, text).toBe(422);
    expect(text).not.toContain(EMAIL);
  });
});

describe("a mandatory monitor-mode masking guardrail still reports what it would have masked", () => {
  const CALLER = "sk-mandatory-monitor-caller";
  const GUARD_NAME = "mandatory-monitor-presidio";
  const PII_GUARD_NAME = "mandatory-monitor-pii";
  const CN_ID = "11010519491231002X"; // valid ISO 7064 MOD 11-2 check digit
  const CREDENTIAL_REF = "mock";
  const LOGSTORE = "mandatory-monitor-events";
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let presidio: PresidioMock | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    presidio = await startPresidioMock();
    sls = await startMockSls();
    upstream = await startOpenAiUpstream({
      nonStreamBody: { ...CHAT_BODY, choices: [{ ...CHAT_BODY.choices[0], message: { role: "assistant", content: "noted" } }] },
    });
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    await seed.createObservabilityExporter({
      name: "sls-mandatory-monitor",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });
    const pk = await seed.createProviderKey({
      display_name: "mandatory-monitor-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "mandatory-monitor-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createGuardrail(
      presidioGuardrail(GUARD_NAME, presidio.baseUrl, { hook_point: "input", enforcement_mode: "monitor" }),
    );
    // A guardrail evaluated in-process, monitored and mandatory too.
    await seed.createGuardrail({
      name: PII_GUARD_NAME,
      enabled: true,
      hook_point: "input",
      enforcement_mode: "monitor",
      mandatory: true,
      kind: "pii",
      detectors: [{ type: "china_id_card", action: "block" }],
    });
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: ["mandatory-monitor-model"] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await presidio?.close();
    await sls?.close();
  });

  const chat = (content: string) =>
    fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({ model: "mandatory-monitor-model", messages: [{ role: "user", content }] }),
    });

  test("the prompt passes unmasked and the usage event carries a would_mask hit", async (ctx) => {
    if (!etcdReachable || !app || !upstream || !sls) return ctx.skip();
    const from = sls.requests.length;
    const res = await chat(`contact me at ${EMAIL} about the order`);
    expect(res.status, await res.text()).toBe(200);
    // Monitor mode observes; it never rewrites the request.
    expect(upstream.receivedRequests.at(-1)!.body).toContain(EMAIL);

    // Wait for this request's usage event, then read what it recorded.
    await waitConfigPropagation(async () => decodedTextFor(sls!, LOGSTORE, from).length > 0);
    const text = decodedTextFor(sls, LOGSTORE, from);
    expect(text).toContain("would_mask");
    expect(text).toContain(GUARD_NAME);
    expect(text).not.toContain(EMAIL);
  });

  test("a block the in-process guardrail would have made is reported as a would_block hit", async (ctx) => {
    if (!etcdReachable || !app || !upstream || !sls) return ctx.skip();
    const from = sls.requests.length;
    const res = await chat(`my id is ${CN_ID} thanks`);
    // Monitor mode never blocks.
    expect(res.status, await res.text()).toBe(200);

    await waitConfigPropagation(async () => decodedTextFor(sls!, LOGSTORE, from).length > 0);
    const text = decodedTextFor(sls, LOGSTORE, from);
    expect(text).toContain("would_block");
    expect(text).toContain(PII_GUARD_NAME);
    expect(text).not.toContain(CN_ID);
  });

  test("an unreachable guardrail still blocks: monitor mode does not cancel mandatory", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const before = upstream.receivedRequests.length;
    const res = await chat(`${OUTAGE} contact me at ${EMAIL}`);
    expect(res.status).toBe(422);
    expect(upstream.receivedRequests.length).toBe(before);
  });
});

describe("a mandatory masking guardrail on an A2A agent's answer", () => {
  const CALLER = "sk-mandatory-a2a-caller";
  let app: SpawnedApp | undefined;
  let presidio: PresidioMock | undefined;
  let agent: Server | undefined;
  let etcdReachable = false;
  // What the agent answers with; each test sets it.
  let answer = "";

  // A minimal A2A agent (protocol 0.3): it serves an agent card and answers
  // `message/send` with a completed task whose status message is `answer`.
  async function startAgent(): Promise<string> {
    const server = createServer((req, res) => {
      let raw = "";
      req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
      req.on("end", () => {
        res.setHeader("content-type", "application/json");
        if (req.method === "GET") {
          res.end(
            JSON.stringify({
              name: "helper",
              description: "answers questions",
              protocolVersion: "0.3",
              version: "1",
              url: "http://127.0.0.1/a2a",
              skills: [],
            }),
          );
          return;
        }
        const body = JSON.parse(raw || "{}") as { id?: unknown };
        res.end(
          JSON.stringify({
            jsonrpc: "2.0",
            id: body.id ?? null,
            result: {
              kind: "task",
              id: "task-1",
              contextId: "ctx-1",
              status: { state: "completed", message: { role: "agent", parts: [{ kind: "text", text: answer }] } },
            },
          }),
        );
      });
    });
    agent = server;
    const port = await pickFreePort();
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(port, "127.0.0.1", resolve);
    });
    return `http://127.0.0.1:${port}/a2a`;
  }

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    presidio = await startPresidioMock();
    const agentUrl = await startAgent();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createGuardrail(
      presidioGuardrail("mandatory-presidio-a2a", presidio.baseUrl, { hook_point: "output", output_fail_open: true }),
    );
    await seed.update("a2a_agents", randomUUID(), {
      name: "helper",
      url: agentUrl,
      protocol_version: "0.3",
      auth_type: "none",
      enabled: true,
    });
    // Caller key last: once it authenticates, every resource above is in
    // the snapshot.
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: [], allowed_agents: ["*"] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await presidio?.close();
    await new Promise<void>((resolve) => (agent ? agent.close(() => resolve()) : resolve()));
  });

  const send = async (text: string) => {
    const res = await fetch(`${app!.proxyUrl}/a2a/helper`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: "1",
        method: "message/send",
        params: { message: { role: "user", parts: [{ kind: "text", text }], messageId: "m-1" } },
      }),
    });
    return { status: res.status, text: await res.text() };
  };

  test("PII in the answer never reaches the caller", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    answer = `you can reach the customer at ${EMAIL}`;
    const { status, text } = await send("how do I reach the customer?");
    // No masked write-back exists for an A2A answer, so it is refused.
    expect(status, text).toBe(422);
    expect(text).toContain("mandatory-presidio-a2a");
    expect(text).not.toContain(EMAIL);
  });

  test("a guardrail that cannot scan the answer blocks it", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    answer = `${OUTAGE} all good`;
    const { status, text } = await send("status?");
    expect(status, text).toBe(422);
    expect(text).not.toContain("all good");
  });
});
