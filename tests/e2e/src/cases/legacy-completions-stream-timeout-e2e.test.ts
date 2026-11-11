import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  scrapeMetrics,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  sumMetric,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-legacy-completions-stream-timeout";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const TIMEOUT_MS = 300;
const STALL_MS = 3_000;

function chunk(text: string): string {
  return JSON.stringify({
    id: "cmpl-timeout",
    object: "text_completion",
    model: "gpt-3.5-turbo-instruct",
    choices: [{ index: 0, text, finish_reason: null }],
  });
}

const done = JSON.stringify({
  id: "cmpl-timeout",
  object: "text_completion",
  model: "gpt-3.5-turbo-instruct",
  choices: [{ index: 0, text: "", finish_reason: "stop" }],
});

async function completion(model: string): Promise<Response> {
  return fetch(`${app!.proxyUrl}/v1/completions`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CALLER}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ model, prompt: "hello", stream: true }),
  });
}

async function usage5xxCounter(): Promise<number> {
  return sumMetric(await scrapeMetrics(app!.metricsUrl), "aisix_usage_events_emitted_total", {
    handler: "completions",
    status_code: "5xx",
  });
}

async function usage2xxCounter(): Promise<number> {
  return sumMetric(await scrapeMetrics(app!.metricsUrl), "aisix_usage_events_emitted_total", {
    handler: "completions",
    status_code: "2xx",
  });
}

async function outputTokenCounter(model: string): Promise<number> {
  const response = await fetch(`${app!.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  let total = 0;
  for (const line of (await response.text()).split("\n")) {
    if (!line.startsWith("aisix_llm_output_tokens_total{")) continue;
    if (!line.includes(`model="${model}"`)) continue;
    total += Number(line.trim().split(/\s+/).at(-1)) || 0;
  }
  return total;
}

async function readCommittedStream(response: Response): Promise<string> {
  const reader = response.body!.getReader();
  const decoder = new TextDecoder();
  let body = "";
  try {
    for (;;) {
      const next = await reader.read();
      if (next.done) break;
      body += decoder.decode(next.value, { stream: true });
    }
  } catch {
    // The typed decode failure terminates an already-committed response body.
  }
  return body + decoder.decode();
}

async function waitForTimeoutObservability(
  modelId: string,
  model: string,
  usageBaseline: number,
  reason = "request_timeout",
): Promise<void> {
  await waitConfigPropagation(async () => {
    const row = await modelStatus(modelId);
    return row?.status === "cooldown" && row.status_reason === reason;
  });

  await waitForFailureRow(model, usageBaseline);
}

/**
 * An undecodable 2xx fails the request and says nothing about the target's
 * health. Read once the failure row has landed: whatever the stream's ending
 * does to the status it has done by then, and this spec's other cases show a
 * real failure turning this same field into `cooldown`.
 */
async function waitForDecodeFailure(
  modelId: string,
  model: string,
  usageBaseline: number,
): Promise<void> {
  await waitForFailureRow(model, usageBaseline);
  const row = await modelStatus(modelId);
  expect(row?.status).toBe("healthy");
  expect(row?.status_reason).toBeUndefined();
}

async function modelStatus(
  modelId: string,
): Promise<{ id?: string; status?: string; status_reason?: string } | undefined> {
  const response = await fetch(`${app!.metricsUrl}/status/models`);
  if (response.status !== 200) {
    throw new Error(`model status probe returned ${response.status}`);
  }
  const rows = (await response.json()) as Array<{
    id?: string;
    status?: string;
    status_reason?: string;
  }>;
  return rows.find((row) => row.id === modelId);
}

async function waitForFailureRow(model: string, usageBaseline: number): Promise<void> {
  await waitConfigPropagation(async () => {
    const response = await fetch(`${app!.metricsUrl}/metrics`);
    if (response.status !== 200) {
      throw new Error(`metrics probe returned ${response.status}`);
    }
    const metrics = await response.text();
    const lines = metrics.split("\n");
    const e2e = lines.some(
      (line) =>
        line.startsWith("aisix_request_e2e_latency_seconds_count{") &&
        line.includes('endpoint="/v1/completions"') &&
        line.includes(`model="${model}"`) &&
        line.includes('status_class="5xx"') &&
        line.includes('streaming="true"'),
    );
    const request = lines.some(
      (line) =>
        line.startsWith("aisix_llm_requests_total{") &&
        line.includes('endpoint="/v1/completions"') &&
        line.includes(`model="${model}"`) &&
        line.includes('stream="true"'),
    );
    return e2e && request;
  });

  await waitConfigPropagation(async () => (await usage5xxCounter()) === usageBaseline + 1);
}

let app: SpawnedApp | undefined;

describe("legacy completions stream timeout", () => {
  let slowFirst: OpenAiUpstream | undefined;
  let midStream: OpenAiUpstream | undefined;
  let truncated: OpenAiUpstream | undefined;
  let malformed: OpenAiUpstream | undefined;
  let malformedDone: OpenAiUpstream | undefined;
  let doneTrailer: OpenAiUpstream | undefined;
  let malformedWithDone: OpenAiUpstream | undefined;
  let inBandError: OpenAiUpstream | undefined;
  let inBandOneRead: OpenAiUpstream | undefined;
  let inBandModelId = "";
  let inBandOverCapModelId = "";
  let inBandHeldModelId = "";
  let slowModelId = "";
  let midModelId = "";
  let truncatedModelId = "";
  let malformedModelId = "";
  let malformedDoneModelId = "";
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    slowFirst = await startOpenAiUpstream({
      firstEventDelayMs: STALL_MS,
      streamEvents: [chunk("slow"), done, "[DONE]"],
    });
    midStream = await startOpenAiUpstream({
      scriptedResponses: [
        {
          eventDelayMs: STALL_MS,
          streamEvents: [chunk("first"), chunk("late"), done, "[DONE]"],
        },
        { streamEvents: [chunk("after-timeout"), done, "[DONE]"] },
      ],
    });
    truncated = await startOpenAiUpstream({
      rawSseChunks: [`data: ${chunk("truncated output")}\n\n`],
    });
    malformed = await startOpenAiUpstream({
      rawSseChunks: ["data: not-json\n\n"],
    });
    malformedDone = await startOpenAiUpstream({
      rawSseChunks: [
        `data: ${chunk("billed-before-malformed-done")}\n\n`,
        "data: not-json\n\n",
        "data: [DONE]\n\n",
      ],
    });
    doneTrailer = await startOpenAiUpstream({
      rawSseChunks: [
        `data: ${chunk("complete-before-done")}\n\ndata: [DONE]\n\ndata: not-json-after-done\n\n`,
      ],
    });
    // The same malformed stream, with the bad event and [DONE] written in one
    // go: how the upstream's writes are split on the way must not change what
    // the caller gets.
    malformedWithDone = await startOpenAiUpstream({
      rawSseChunks: [
        `data: ${chunk("billed-before-coalesced-done")}\n\n`,
        "data: not-json-coalesced\n\ndata: [DONE]\n\n",
      ],
    });

    // How vLLM's completions server ends a generation that failed after the
    // head went out: the error envelope as a data event, then `[DONE]`
    // (`vllm/entrypoints/openai/completion/serving.py`, the stream
    // generator's `except` arms; the envelope is its `ErrorResponse`).
    inBandError = await startOpenAiUpstream({
      rawSseChunks: [
        `data: ${chunk("written-before-the-error")}\n\n`,
        `data: ${JSON.stringify({
          error: {
            message: "engine failed mid-generation",
            type: "InternalServerError",
            param: null,
            code: 500,
          },
        })}\n\n`,
        "data: [DONE]\n\n",
      ],
    });
    // The same failure written in ONE read, with more text ahead of the error
    // than a small output hold may keep: the read that crosses the hold's cap
    // is the one that carries the upstream's report.
    inBandOneRead = await startOpenAiUpstream({
      rawSseChunks: [
        `data: ${chunk("X".repeat(128))}\n\n` +
          `data: ${JSON.stringify({
            error: {
              message: "engine failed mid-generation",
              type: "InternalServerError",
              param: null,
              code: 500,
            },
          })}\n\n` +
          "data: [DONE]\n\n",
      ],
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const providerKey = async (name: string, upstream: OpenAiUpstream) =>
      (
        await seed.createProviderKey({
          display_name: `${name}-pk`,
          secret: "sk-mock",
          api_base: `${upstream.baseUrl}/v1`,
        })
      ).id;

    const slowPk = await providerKey("legacy-timeout-slow", slowFirst);
    const midPk = await providerKey("legacy-timeout-mid", midStream);
    const truncatedPk = await providerKey("legacy-truncated", truncated);
    const malformedPk = await providerKey("legacy-malformed", malformed);
    const malformedDonePk = await providerKey(
      "legacy-malformed-before-done",
      malformedDone,
    );
    const malformedWithDonePk = await providerKey(
      "legacy-malformed-with-done",
      malformedWithDone,
    );
    const doneTrailerPk = await providerKey(
      "legacy-done-with-trailer",
      doneTrailer,
    );
    const inBandPk = await providerKey("legacy-in-band-error", inBandError);
    const inBandOneReadPk = await providerKey("legacy-in-band-one-read", inBandOneRead);

    const slowModel = await seed.createModel({
      display_name: "legacy-timeout-slow",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: slowPk,
      stream_timeout: TIMEOUT_MS,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    slowModelId = slowModel.id;
    const midModel = await seed.createModel({
      display_name: "legacy-timeout-mid",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: midPk,
      stream_timeout: TIMEOUT_MS,
      retries: 0,
      rate_limit: { concurrency: 1 },
      cooldown: { enabled: true, default_seconds: 1 },
    });
    midModelId = midModel.id;
    const truncatedModel = await seed.createModel({
      display_name: "legacy-truncated",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: truncatedPk,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    truncatedModelId = truncatedModel.id;
    const malformedModel = await seed.createModel({
      display_name: "legacy-malformed",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: malformedPk,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    malformedModelId = malformedModel.id;
    const malformedDoneModel = await seed.createModel({
      display_name: "legacy-malformed-before-done",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: malformedDonePk,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    malformedDoneModelId = malformedDoneModel.id;
    await seed.createModel({
      display_name: "legacy-malformed-with-done",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: malformedWithDonePk,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    await seed.createModel({
      display_name: "legacy-done-with-trailer",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: doneTrailerPk,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    const inBandModel = await seed.createModel({
      display_name: "legacy-in-band-error",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: inBandPk,
      retries: 0,
      cooldown: { enabled: true, default_seconds: 60 },
    });
    inBandModelId = inBandModel.id;
    // Two holds over the same upstream: one whose cap the read crosses and
    // that refuses past it, and one that holds the whole answer.
    for (const [name, maxBufferBytes] of [
      ["legacy-in-band-over-cap", 32],
      ["legacy-in-band-held", 4096],
    ] as const) {
      const model = await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-3.5-turbo-instruct",
        provider_key_id: inBandOneReadPk,
        retries: 0,
        cooldown: { enabled: true, default_seconds: 60 },
      });
      const guard = await seed.createGuardrail(
        {
          name: `${name}-guard`,
          enabled: true,
          kind: "pii",
          hook_point: "output",
          detectors: [{ type: "email", action: "mask" }],
          max_buffer_bytes: maxBufferBytes,
          on_buffer_exceeded: "fail_closed",
        },
        { attach: false },
      );
      await seed.attachGuardrailToModel(guard.id, model.id);
      if (name === "legacy-in-band-over-cap") inBandOverCapModelId = model.id;
      else inBandHeldModelId = model.id;
    }

    // The caller key is the final revision. Authenticated model listing is a
    // propagation barrier without exercising the timeout behavior under test.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: [
        "legacy-timeout-slow",
        "legacy-timeout-mid",
        "legacy-truncated",
        "legacy-malformed",
        "legacy-malformed-before-done",
        "legacy-malformed-with-done",
        "legacy-done-with-trailer",
        "legacy-in-band-error",
        "legacy-in-band-over-cap",
        "legacy-in-band-held",
      ],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      if (response.status === 401) return false;
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((model) => model.id));
      return (
        ids.has("legacy-timeout-slow") &&
        ids.has("legacy-timeout-mid") &&
        ids.has("legacy-truncated") &&
        ids.has("legacy-malformed") &&
        ids.has("legacy-malformed-before-done") &&
        ids.has("legacy-malformed-with-done") &&
        ids.has("legacy-done-with-trailer") &&
        ids.has("legacy-in-band-error") &&
        ids.has("legacy-in-band-over-cap") &&
        ids.has("legacy-in-band-held")
      );
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([
      slowFirst?.close(),
      midStream?.close(),
      truncated?.close(),
      malformed?.close(),
      malformedDone?.close(),
      doneTrailer?.close(),
      malformedWithDone?.close(),
      inBandError?.close(),
      inBandOneRead?.close(),
    ]);
  });

  test("a stalled first event times out before committing 200", async (ctx) => {
    if (!etcdReachable || !app || !slowFirst) {
      ctx.skip();
      return;
    }
    const slowBase = slowFirst.receivedRequests.length;
    const usageBaseline = await usage5xxCounter();
    const started = Date.now();

    const response = await completion("legacy-timeout-slow");

    expect(response.status).toBe(504);
    expect(Date.now() - started).toBeLessThan(STALL_MS / 2);
    expect(slowFirst.receivedRequests.length - slowBase).toBe(1);
    await waitForTimeoutObservability(
      slowModelId,
      "legacy-timeout-slow",
      usageBaseline,
    );
  });

  test("a mid-stream stall terminates promptly and releases concurrency", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const usageBaseline = await usage5xxCounter();
    const started = Date.now();
    const response = await completion("legacy-timeout-mid");
    expect(response.status).toBe(200);
    const reader = response.body!.getReader();
    let received = "";
    try {
      while (true) {
        const next = await reader.read();
        if (next.done) break;
        received += new TextDecoder().decode(next.value);
      }
    } catch {
      // A typed upstream stream error closes the HTTP body after the first
      // event. Whether fetch reports the truncated body as EOF or an error is
      // transport-dependent; the observable contract is bounded termination.
    }

    expect(Date.now() - started).toBeLessThan(STALL_MS / 2);
    expect(received).toContain("first");
    expect(received).not.toContain("late");
    await waitForTimeoutObservability(
      midModelId,
      "legacy-timeout-mid",
      usageBaseline,
    );

    await waitConfigPropagation(async () => {
      const status = await fetch(`${app!.metricsUrl}/status/models`);
      if (status.status !== 200) {
        throw new Error(`model status probe returned ${status.status}`);
      }
      const rows = (await status.json()) as Array<{ id?: string; status?: string }>;
      return rows.some((row) => row.id === midModelId && row.status !== "cooldown");
    });

    // The first stream's reservation must be gone before the next request.
    // With concurrency=1, a leaked hold would produce 429 here.
    const next = await completion("legacy-timeout-mid");
    expect(next.status).toBe(200);
    expect(await next.text()).toContain("after-timeout");
  }, 30_000);

  test("EOF without DONE is recorded as an upstream stream failure", async (ctx) => {
    if (!etcdReachable || !app || !truncated) {
      ctx.skip();
      return;
    }
    const upstreamBaseline = truncated.receivedRequests.length;
    const usageBaseline = await usage5xxCounter();

    const response = await completion("legacy-truncated");

    expect(response.status).toBe(200);
    expect(await response.text()).toContain("truncated output");
    expect(truncated.receivedRequests).toHaveLength(upstreamBaseline + 1);
    await waitForTimeoutObservability(
      truncatedModelId,
      "legacy-truncated",
      usageBaseline,
      "transport_error",
    );
  });

  test("malformed SSE is recorded as an upstream decode failure", async (ctx) => {
    if (!etcdReachable || !app || !malformed) {
      ctx.skip();
      return;
    }
    const upstreamBaseline = malformed.receivedRequests.length;
    const usageBaseline = await usage5xxCounter();

    const response = await completion("legacy-malformed");

    expect(response.status).toBe(200);
    expect(await response.text()).toContain("not-json");
    expect(malformed.receivedRequests).toHaveLength(upstreamBaseline + 1);
    await waitForDecodeFailure(malformedModelId, "legacy-malformed", usageBaseline);
  });

  test("malformed SSE before DONE is a billed decode failure without a success terminal", async (ctx) => {
    if (!etcdReachable || !app || !malformedDone) {
      ctx.skip();
      return;
    }
    const upstreamBaseline = malformedDone.receivedRequests.length;
    const usageBaseline = await usage5xxCounter();
    const outputBaseline = await outputTokenCounter(
      "legacy-malformed-before-done",
    );

    const response = await completion("legacy-malformed-before-done");
    expect(response.status).toBe(200);
    const body = await readCommittedStream(response);
    expect(body).toContain("billed-before-malformed-done");
    expect(body).toContain("not-json");
    expect(body).not.toContain("[DONE]");
    expect(malformedDone.receivedRequests).toHaveLength(upstreamBaseline + 1);
    await waitForDecodeFailure(
      malformedDoneModelId,
      "legacy-malformed-before-done",
      usageBaseline,
    );
    await waitConfigPropagation(
      async () =>
        (await outputTokenCounter("legacy-malformed-before-done")) >
        outputBaseline,
    );
  });

  test("an in-band error before DONE is recorded as the upstream's failure, not a success", async (ctx) => {
    if (!etcdReachable || !app || !inBandError) {
      ctx.skip();
      return;
    }
    const usageBaseline = await usage5xxCounter();
    const successBaseline = await usage2xxCounter();

    const response = await completion("legacy-in-band-error");

    expect(response.status).toBe(200);
    const body = await readCommittedStream(response);
    // The caller gets what the upstream sent: its text, its error, its [DONE].
    expect(body).toContain("written-before-the-error");
    expect(body).toContain("engine failed mid-generation");
    expect(body).toContain("[DONE]");
    // The request failed upstream: a 5xx row, and the target cools down.
    await waitForTimeoutObservability(
      inBandModelId,
      "legacy-in-band-error",
      usageBaseline,
      "upstream_server_error",
    );
    expect(await usage2xxCounter()).toBe(successBaseline);
  });

  test("an in-band error in the read that crosses a fail-closed hold's cap still cools the target down", async (ctx) => {
    if (!etcdReachable || !app || !inBandOneRead) {
      ctx.skip();
      return;
    }
    const upstreamBaseline = inBandOneRead.receivedRequests.length;
    const successBaseline = await usage2xxCounter();

    const response = await completion("legacy-in-band-over-cap");

    // The gateway's refusal is unchanged: one failure frame, nothing held.
    expect(response.status).toBe(200);
    const body = await readCommittedStream(response);
    expect(body).toMatch(/^event: error\ndata: /);
    expect(body).toContain("output_buffer_exceeded");
    expect(body).not.toContain("XXXX");
    expect(body).not.toContain("engine failed mid-generation");
    expect(inBandOneRead.receivedRequests).toHaveLength(upstreamBaseline + 1);
    // The refusal's row is the 200 its head committed; the upstream's own end
    // is what the target's health reads, and it reported a server error.
    await waitConfigPropagation(
      async () => (await usage2xxCounter()) === successBaseline + 1,
    );
    const row = await modelStatus(inBandOverCapModelId);
    expect(row?.status).toBe("cooldown");
    expect(row?.status_reason).toBe("upstream_server_error");
  });

  test("an in-band error under a hold that keeps the whole answer cools the target down", async (ctx) => {
    if (!etcdReachable || !app || !inBandOneRead) {
      ctx.skip();
      return;
    }
    const usageBaseline = await usage5xxCounter();

    const response = await completion("legacy-in-band-held");

    expect(response.status).toBe(200);
    const body = await readCommittedStream(response);
    expect(body).toContain("XXXX");
    expect(body).toContain("engine failed mid-generation");
    await waitForTimeoutObservability(
      inBandHeldModelId,
      "legacy-in-band-held",
      usageBaseline,
      "upstream_server_error",
    );
  });

  test("malformed SSE written together with DONE reaches the caller the same way", async (ctx) => {
    if (!etcdReachable || !app || !malformedWithDone) {
      ctx.skip();
      return;
    }
    const response = await completion("legacy-malformed-with-done");
    expect(response.status).toBe(200);
    const body = await readCommittedStream(response);
    expect(body).toContain("billed-before-coalesced-done");
    expect(body).toContain("not-json-coalesced");
    expect(body).not.toContain("[DONE]");
  });

  test("DONE cuts off malformed data coalesced into the same upstream chunk", async (ctx) => {
    if (!etcdReachable || !app || !doneTrailer) {
      ctx.skip();
      return;
    }

    const upstreamBaseline = doneTrailer.receivedRequests.length;
    const successBaseline = await usage2xxCounter();
    const failureBaseline = await usage5xxCounter();
    const response = await completion("legacy-done-with-trailer");
    expect(response.status).toBe(200);
    const body = await readCommittedStream(response);
    expect(body).toContain("complete-before-done");
    expect(body).toContain("[DONE]");
    expect(body).not.toContain("not-json-after-done");
    expect(doneTrailer.receivedRequests).toHaveLength(upstreamBaseline + 1);
    await waitConfigPropagation(
      async () => (await usage2xxCounter()) === successBaseline + 1,
    );
    expect(await usage5xxCounter()).toBe(failureBaseline);
  });
});
