import { createHash } from "node:crypto";
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
  const response = await fetch(`${app!.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  const line = (await response.text())
    .split("\n")
    .find(
      (candidate) =>
        candidate.startsWith("aisix_usage_events_emitted_total{") &&
        candidate.includes('handler="completions"') &&
        candidate.includes('status_code="5xx"'),
    );
  return line ? Number(line.trim().split(/\s+/).at(-1)) : 0;
}

async function usage2xxCounter(): Promise<number> {
  const response = await fetch(`${app!.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  const line = (await response.text())
    .split("\n")
    .find(
      (candidate) =>
        candidate.startsWith("aisix_usage_events_emitted_total{") &&
        candidate.includes('handler="completions"') &&
        candidate.includes('status_code="2xx"'),
    );
  return line ? Number(line.trim().split(/\s+/).at(-1)) : 0;
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
    const response = await fetch(`${app!.metricsUrl}/status/models`);
    if (response.status !== 200) {
      throw new Error(`model status probe returned ${response.status}`);
    }
    const rows = (await response.json()) as Array<{
      id?: string;
      status?: string;
      status_reason?: string;
    }>;
    return rows.some(
      (row) =>
        row.id === modelId &&
        row.status === "cooldown" &&
        row.status_reason === reason,
    );
  });

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
    const doneTrailerPk = await providerKey(
      "legacy-done-with-trailer",
      doneTrailer,
    );

    const slowModel = await seed.createModel({
      display_name: "legacy-timeout-slow",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: slowPk,
      stream_timeout: TIMEOUT_MS,
      retries: 0,
      cooldown: { default_seconds: 60 },
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
      cooldown: { default_seconds: 1 },
    });
    midModelId = midModel.id;
    const truncatedModel = await seed.createModel({
      display_name: "legacy-truncated",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: truncatedPk,
      retries: 0,
      cooldown: { default_seconds: 60 },
    });
    truncatedModelId = truncatedModel.id;
    const malformedModel = await seed.createModel({
      display_name: "legacy-malformed",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: malformedPk,
      retries: 0,
      cooldown: { default_seconds: 60 },
    });
    malformedModelId = malformedModel.id;
    const malformedDoneModel = await seed.createModel({
      display_name: "legacy-malformed-before-done",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: malformedDonePk,
      retries: 0,
      cooldown: { default_seconds: 60 },
    });
    malformedDoneModelId = malformedDoneModel.id;
    await seed.createModel({
      display_name: "legacy-done-with-trailer",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: doneTrailerPk,
      retries: 0,
      cooldown: { default_seconds: 60 },
    });

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
        "legacy-done-with-trailer",
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
        ids.has("legacy-done-with-trailer")
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
    await waitForTimeoutObservability(
      malformedModelId,
      "legacy-malformed",
      usageBaseline,
      "upstream_decode_error",
    );
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
    await waitForTimeoutObservability(
      malformedDoneModelId,
      "legacy-malformed-before-done",
      usageBaseline,
      "upstream_decode_error",
    );
    await waitConfigPropagation(
      async () =>
        (await outputTokenCounter("legacy-malformed-before-done")) >
        outputBaseline,
    );
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
