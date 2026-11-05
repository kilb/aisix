import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  awaitWindowHeadroom,
  decodedTextFor,
  EtcdClient,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  scrapeMetrics,
  sumMetric,
  type MockSls,
  type OpenAiUpstream,
  type OpenAiUpstreamOptions,
  type SpawnedApp,
} from "../harness/index.js";

// What a streamed response owes, whatever shape it took on the way:
//
// - a frame a mebibyte or two long is one frame, and its usage is the bill;
// - a caller who leaves while the gateway is still holding an answer back for
//   its output guardrail left behind a call the provider billed;
// - every generated text a caller can read is text an output rule reads;
// - an upstream that dies mid-relay is a failure the caller can see.
const CALLER_PLAINTEXT = "sk-stream-contracts-PLAINTEXT";
const CALLER_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
/** A second caller whose token budget the abandoned streams spend. */
const TPM_CALLER_PLAINTEXT = "sk-stream-contracts-tpm-PLAINTEXT";
const CREDENTIAL_REF = "mock";
const SLS_PROJECT = "aisix-e2e-obs";
const LOGSTORE = "stream-contracts";
const GUARDED_LOGSTORE = "stream-contracts-guarded";
const FORBIDDEN = "streamcontractsentinel";

// An image generation result, the way the Responses API repeats it in its
// terminal event: about 2 MiB of base64 beside the only usage the stream reports.
const IMAGE_RESULT = "A".repeat(2 * 1024 * 1024);
const LARGE_TERMINAL_FRAMES = [
  `event: response.created\ndata: ${JSON.stringify({
    type: "response.created",
    response: { id: "resp_large", model: "upstream-dated-model" },
  })}\n\n`,
  `event: response.completed\ndata: ${JSON.stringify({
    type: "response.completed",
    response: {
      id: "resp_large",
      model: "upstream-dated-model",
      output: [{ type: "image_generation_call", id: "ig_1", result: IMAGE_RESULT }],
      usage: { input_tokens: 123_456, output_tokens: 7_890, total_tokens: 131_346 },
    },
  })}\n\n`,
];

/** A long answer: long enough for the caller to leave in the middle of it. */
const SLOW = { eventDelayMs: 400 } as const;
const ANSWER = "the provider is writing a long answer";

function chatChunk(delta: Record<string, unknown>): string {
  return JSON.stringify({
    id: "chatcmpl-contracts",
    object: "chat.completion.chunk",
    created: 0,
    model: "upstream-model",
    choices: [{ index: 0, delta, finish_reason: null }],
  });
}

const CHAT_SLOW: OpenAiUpstreamOptions = {
  ...SLOW,
  streamEvents: [1, 2, 3, 4, 5, 6, 7, 8].map(() => chatChunk({ content: ANSWER })),
};
const COMPLETIONS_SLOW: OpenAiUpstreamOptions = {
  ...SLOW,
  streamEvents: [1, 2, 3, 4, 5, 6, 7, 8].map(() =>
    JSON.stringify({
      id: "cmpl-contracts",
      object: "text_completion",
      created: 0,
      model: "upstream-model",
      choices: [{ index: 0, text: ANSWER, finish_reason: null }],
    }),
  ),
};
const RESPONSES_SLOW: OpenAiUpstreamOptions = {
  ...SLOW,
  rawStreamFrames: [
    `event: response.created\ndata: ${JSON.stringify({
      type: "response.created",
      response: { id: "resp_slow" },
    })}\n\n`,
    ...[1, 2, 3, 4, 5, 6, 7, 8].map(
      () =>
        `event: response.output_text.delta\ndata: ${JSON.stringify({
          type: "response.output_text.delta",
          delta: ANSWER,
        })}\n\n`,
    ),
  ],
};
const MESSAGES_SLOW: OpenAiUpstreamOptions = {
  ...SLOW,
  rawStreamFrames: [
    `event: message_start\ndata: ${JSON.stringify({
      type: "message_start",
      message: {
        id: "msg_slow",
        type: "message",
        role: "assistant",
        content: [],
        model: "claude-upstream",
        usage: { input_tokens: 41, output_tokens: 1 },
      },
    })}\n\n`,
    ...[1, 2, 3, 4, 5, 6, 7, 8].map(
      () =>
        `event: content_block_delta\ndata: ${JSON.stringify({
          type: "content_block_delta",
          index: 0,
          delta: { type: "text_delta", text: ANSWER },
        })}\n\n`,
    ),
  ],
};

/** A completion of 1,500 one-word tokens, mostly the frames around them. */
const LONG_COMPLETION: OpenAiUpstreamOptions = {
  streamEvents: [
    ...Array.from({ length: 1500 }, () =>
      JSON.stringify({
        id: "cmpl-7QmVI15qgYVllxK0FtxVGG6ywfzaq",
        object: "text_completion",
        created: 1_715_000_000,
        choices: [{ text: " token", index: 0, logprobs: null, finish_reason: null }],
        model: "gpt-3.5-turbo-instruct",
      }),
    ),
    "[DONE]",
  ],
};

interface Seeded {
  upstream: OpenAiUpstreamOptions;
  anthropic?: boolean;
  hold?: boolean;
}

const MODELS: Record<string, Seeded> = {
  "sc-large-responses": { upstream: { rawStreamFrames: LARGE_TERMINAL_FRAMES } },
  "sc-large-completions": {
    upstream: {
      streamEvents: [
        JSON.stringify({
          id: "cmpl-large",
          object: "text_completion",
          created: 0,
          model: "upstream-model",
          choices: [{ index: 0, text: "w".repeat(2 * 1024 * 1024), finish_reason: "stop" }],
          usage: { prompt_tokens: 321, completion_tokens: 654, total_tokens: 975 },
        }),
        "[DONE]",
      ],
    },
  },
  "sc-held-responses": { upstream: RESPONSES_SLOW, hold: true },
  "sc-tpm-held": { upstream: RESPONSES_SLOW, hold: true },
  "sc-held-completions": { upstream: COMPLETIONS_SLOW, hold: true },
  "sc-held-chat": { upstream: CHAT_SLOW, hold: true },
  "sc-held-messages": { upstream: MESSAGES_SLOW, anthropic: true, hold: true },
  "sc-long-completion": { upstream: LONG_COMPLETION, hold: true },
  "sc-transcript": {
    upstream: {
      streamEvents: [
        chatChunk({ role: "assistant", audio: { id: "a1", transcript: `say ${FORBIDDEN.slice(0, 8)}` } }),
        chatChunk({ audio: { transcript: `${FORBIDDEN.slice(8)} now` } }),
        "[DONE]",
      ],
    },
    hold: true,
  },
  "sc-custom-tool": {
    upstream: {
      streamEvents: [
        chatChunk({
          tool_calls: [
            { index: 0, id: "call_1", type: "custom", custom: { name: "run", input: `say ${FORBIDDEN.slice(0, 8)}` } },
          ],
        }),
        chatChunk({ tool_calls: [{ index: 0, custom: { input: `${FORBIDDEN.slice(8)} now` } }] }),
        "[DONE]",
      ],
    },
    hold: true,
  },
  "sc-custom-tool-buffered": {
    upstream: {
      nonStreamBody: {
        id: "chatcmpl-custom",
        object: "chat.completion",
        created: 0,
        model: "upstream-model",
        choices: [
          {
            index: 0,
            finish_reason: "tool_calls",
            message: {
              role: "assistant",
              content: null,
              tool_calls: [
                { id: "call_1", type: "custom", custom: { name: "run", input: `say ${FORBIDDEN} now` } },
              ],
            },
          },
        ],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      },
    },
    hold: true,
  },
};

const BLOCKED_ROUTE = "sc-blocked-route";
const CLEAN_ROUTE = "sc-clean-route";
const CUT_ROUTE = "sc-cut-route";
const LARGE_ROUTE = "sc-large-route";

describe("what a stream owes, whatever shape it took", () => {
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
      name: "sls-stream-contracts",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });
    const block = await seed.createGuardrail(
      {
        name: "sc-output-block",
        enabled: true,
        hook_point: "output",
        kind: "keyword",
        patterns: [{ kind: "literal", value: FORBIDDEN }],
      },
      { attach: false },
    );
    for (const [model, { upstream: opts, anthropic, hold }] of Object.entries(MODELS)) {
      const upstream = await startOpenAiUpstream(opts);
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `${model}-pk`,
        secret: "sk-mock",
        ...(anthropic
          ? { api_base: upstream.baseUrl, provider: "anthropic", adapter: "anthropic" }
          : { api_base: `${upstream.baseUrl}/v1` }),
      });
      const created = await seed.createModel({
        display_name: model,
        provider: anthropic ? "anthropic" : "openai",
        model_name: anthropic ? "claude-3-5-haiku-20241022" : "gpt-4o-mini",
        provider_key_id: pk.id,
      });
      if (hold) await seed.attachGuardrailToModel(block.id, created.id);
    }
    const route = async (
      name: string,
      opts: OpenAiUpstreamOptions,
    ): Promise<void> => {
      const upstream = await startOpenAiUpstream(opts);
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      await seed.createPassthroughRoute({
        name,
        path_prefix: `/passthrough/${name}`,
        target_url: upstream.baseUrl,
        provider_key_id: pk.id,
      });
    };
    await route(CUT_ROUTE, {
      eventDelayMs: 50,
      streamEvents: [chatChunk({ content: "half an ans" }), chatChunk({ content: "wer" })],
      disconnectAfterEvents: 1,
    });
    await route(LARGE_ROUTE, { rawStreamFrames: LARGE_TERMINAL_FRAMES });
    await seed.createApiKey({
      key_hash: createHash("sha256").update(TPM_CALLER_PLAINTEXT).digest("hex"),
      allowed_models: ["*"],
      rate_limit: { tpm: 5 },
    });
    // Caller key last: it doubles as the propagation barrier for all of it.
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: ["*"],
      allowed_routes: ["*"],
    });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      if (res.status !== 200) return false;
      const body = (await res.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((m) => m.id));
      return Object.keys(MODELS).every((m) => ids.has(m));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  const ready = () => etcdReachable && !!app && !!sls;

  function post(
    path: string,
    body: Record<string, unknown>,
    init: { caller?: string; signal?: AbortSignal } = {},
  ): Promise<Response> {
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${init.caller ?? CALLER_PLAINTEXT}`,
        "x-api-key": init.caller ?? CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
      signal: init.signal,
    });
  }

  const chat = (model: string, stream = true) => ({
    path: "/v1/chat/completions",
    body: { model, stream, messages: [{ role: "user", content: "a question of several words" }] },
  });

  const rowOf = (pred: (log: Map<string, string>) => boolean, what: string) =>
    waitForSlsLog(sls!, LOGSTORE, pred, what, 20_000);

  test("a terminal frame past a mebibyte is billed what the provider reported", async (ctx) => {
    if (!ready()) return ctx.skip();
    const res = await post("/v1/responses", {
      model: "sc-large-responses",
      input: "draw it",
      stream: true,
    });
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    const body = await res.text();
    expect(body).toContain(IMAGE_RESULT);
    expect(body, "the caller is told the name it addressed").not.toContain("upstream-dated-model");
    const row = await rowOf((l) => l.get("request_id") === requestId, "the large-frame row");
    expect(row.get("status_code")).toBe("200");
    expect(row.get("prompt_tokens")).toBe("123456");
    expect(row.get("completion_tokens")).toBe("7890");
  });

  test("so is one on /v1/completions, from an upstream that answers in one frame", async (ctx) => {
    if (!ready()) return ctx.skip();
    const res = await post("/v1/completions", {
      model: "sc-large-completions",
      prompt: "go",
      stream: true,
      stream_options: { include_usage: true },
    });
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    await res.text();
    const row = await rowOf((l) => l.get("request_id") === requestId, "the large completion's row");
    expect(row.get("prompt_tokens")).toBe("321");
    expect(row.get("completion_tokens")).toBe("654");
  });

  test("so is one relayed through a passthrough route", async (ctx) => {
    if (!ready()) return ctx.skip();
    const res = await post(`/passthrough/${LARGE_ROUTE}/v1/responses`, {
      model: "anything",
      input: "draw it",
      stream: true,
    });
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    await res.text();
    const row = await rowOf((l) => l.get("request_id") === requestId, "the relayed large-frame row");
    expect(row.get("prompt_tokens")).toBe("123456");
    expect(row.get("completion_tokens")).toBe("7890");
  });

  for (const [family, request] of [
    [
      "/v1/responses",
      { path: "/v1/responses", body: { model: "sc-held-responses", input: "a question of several words", stream: true } },
    ],
    [
      "/v1/completions",
      { path: "/v1/completions", body: { model: "sc-held-completions", prompt: "a question of several words", stream: true } },
    ],
    ["/v1/chat/completions", chat("sc-held-chat")],
    [
      "/v1/messages",
      {
        path: "/v1/messages",
        body: {
          model: "sc-held-messages",
          max_tokens: 64,
          stream: true,
          messages: [{ role: "user", content: "a question of several words" }],
        },
      },
    ],
  ] as const) {
    test(`a caller leaving a held ${family} stream leaves a row that bills what was generated`, async (ctx) => {
      if (!ready()) return ctx.skip();
      const model = request.body.model;
      // The head may not exist yet, and nothing has been released either way.
      const controller = new AbortController();
      const answered = post(request.path, request.body, { signal: controller.signal }).catch(
        () => undefined,
      );
      await new Promise((resolve) => setTimeout(resolve, 1_000));
      controller.abort();
      await answered;
      const row = await rowOf(
        (l) => l.get("requested_model") === model && l.get("status_code") === "499",
        `the abandoned ${family} row`,
      );
      expect(row.get("error_class")).toBe("client_disconnected");
      expect(Number(row.get("prompt_tokens")), "the prompt reached the provider").toBeGreaterThan(0);
      expect(Number(row.get("completion_tokens")), "so did what it had written").toBeGreaterThan(0);
      expect(
        sumMetric(await scrapeMetrics(app!.metricsUrl), "aisix_llm_output_tokens_total", { model }),
        "the scrape carries what the row does",
      ).toBeGreaterThan(0);
    }, 40_000);
  }

  test("what a held stream's caller left behind spends the key's token budget", async (ctx) => {
    if (!ready()) return ctx.skip();
    await awaitWindowHeadroom(20);
    const started = Date.now();
    const controller = new AbortController();
    const answered = post(
      "/v1/responses",
      { model: "sc-tpm-held", input: "a question of several words", stream: true },
      { caller: TPM_CALLER_PLAINTEXT, signal: controller.signal },
    ).catch(() => undefined);
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    controller.abort();
    await answered;
    await rowOf(
      (l) => l.get("requested_model") === "sc-tpm-held" && l.get("status_code") === "499",
      "the abandoned row",
    );
    const next = await post(
      "/v1/responses",
      { model: "sc-tpm-held", input: "again", stream: true },
      { caller: TPM_CALLER_PLAINTEXT },
    );
    await next.text();
    expect(Date.now() - started, "both calls landed in one window").toBeLessThan(20_000);
    expect(next.status, "the key's budget of 5 tokens is spent").toBe(429);
  }, 120_000);

  test("the hold cap of /v1/completions measures the text, not the frames around it", async (ctx) => {
    if (!ready()) return ctx.skip();
    const res = await post("/v1/completions", {
      model: "sc-long-completion",
      prompt: "go",
      stream: true,
    });
    expect(res.status).toBe(200);
    const body = await res.text();
    expect(body).toContain("data: [DONE]");
    expect(body.split(" token").length - 1).toBe(1500);
  });

  test("an output rule reads a streamed audio transcript, split across deltas", async (ctx) => {
    if (!ready()) return ctx.skip();
    const { path, body } = chat("sc-transcript");
    const res = await post(path, body);
    const wire = await res.text();
    expect(wire).toContain("content_filter");
    expect(wire).not.toContain(FORBIDDEN.slice(0, 8));
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    const row = await rowOf((l) => l.get("request_id") === requestId, "the transcript row");
    expect(row.get("guardrail_blocked")).toBe("true");
  });

  test("an output rule reads a custom tool's input, streamed or not", async (ctx) => {
    if (!ready()) return ctx.skip();
    const { path, body } = chat("sc-custom-tool");
    const res = await post(path, body);
    const wire = await res.text();
    expect(wire).toContain("content_filter");
    expect(wire).not.toContain(FORBIDDEN.slice(0, 8));

    const buffered = chat("sc-custom-tool-buffered", false);
    const refused = await post(buffered.path, buffered.body);
    expect(refused.status).toBe(422);
    expect(await refused.text()).not.toContain(FORBIDDEN);
  });

  test("a relayed stream that loses its upstream fails for the caller too", async (ctx) => {
    if (!ready()) return ctx.skip();
    const res = await post(`/passthrough/${CUT_ROUTE}/v1/chat/completions`, {
      model: "anything",
      stream: true,
      messages: [{ role: "user", content: "hi" }],
    });
    expect(res.status).toBe(200);
    const reader = res.body!.getReader();
    let received = "";
    let failed = false;
    const decoder = new TextDecoder();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        received += decoder.decode(value);
      }
    } catch {
      failed = true;
    }
    expect(received).toContain("half an ans");
    expect(failed, "half an answer must not read as a whole one").toBe(true);
  });
});

// An output rule on a relay. Rules on a passthrough route attach at the
// environment, so these two get a gateway of their own.
describe("what a guarded relay exports", () => {
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
      name: "sls-guarded-relay",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: GUARDED_LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });
    await seed.createGuardrail({
      name: "sc-relay-block",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN }],
    });
    for (const [name, content] of [
      [BLOCKED_ROUTE, `say ${FORBIDDEN} now`],
      [CLEAN_ROUTE, "a perfectly clean answer"],
    ] as const) {
      const upstream = await startOpenAiUpstream({
        streamEvents: [chatChunk({ content }), "[DONE]"],
      });
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      await seed.createPassthroughRoute({
        name,
        path_prefix: `/${name}`,
        target_url: upstream.baseUrl,
        provider_key_id: pk.id,
        protocol: "openai_chat",
      });
    }
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: ["*"],
      allowed_routes: ["*"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  function relay(route: string): Promise<Response> {
    return fetch(`${app!.proxyUrl}/${route}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "anything",
        stream: true,
        messages: [{ role: "user", content: "hi" }],
      }),
    });
  }

  async function settle(route: string): Promise<Response> {
    let res: Response | undefined;
    await waitConfigPropagation(async () => {
      res = await relay(route);
      return res.status !== 401 && res.status !== 404;
    });
    return res!;
  }

  test("a stream the rule refused exports none of what it stopped", async (ctx) => {
    if (!etcdReachable || !app || !sls) return ctx.skip();
    const res = await settle(BLOCKED_ROUTE);
    const wire = await res.text();
    expect(wire).not.toContain(FORBIDDEN);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    await waitForSlsLog(
      sls,
      GUARDED_LOGSTORE,
      (l) => l.get("request_id") === requestId,
      "the refused relay's row",
      20_000,
    );
    expect(decodedTextFor(sls, GUARDED_LOGSTORE)).not.toContain(FORBIDDEN);
  });

  test("a clean one exports its answer, as content capture promises", async (ctx) => {
    if (!etcdReachable || !app || !sls) return ctx.skip();
    const res = await settle(CLEAN_ROUTE);
    await res.text();
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    await waitForSlsLog(
      sls,
      GUARDED_LOGSTORE,
      (l) => l.get("request_id") === requestId,
      "the clean relay's row",
      20_000,
    );
    expect(decodedTextFor(sls, GUARDED_LOGSTORE)).toContain("a perfectly clean answer");
  });
});
