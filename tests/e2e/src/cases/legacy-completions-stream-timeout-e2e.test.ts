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

async function waitForTimeoutObservability(
  modelId: string,
  model: string,
  usageBaseline: number,
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
        row.status_reason === "request_timeout",
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
  let slowModelId = "";
  let midModelId = "";
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

    // The caller key is the final revision. Authenticated model listing is a
    // propagation barrier without exercising the timeout behavior under test.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: ["legacy-timeout-slow", "legacy-timeout-mid"],
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
      return ids.has("legacy-timeout-slow") && ids.has("legacy-timeout-mid");
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([slowFirst?.close(), midStream?.close()]);
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
});
