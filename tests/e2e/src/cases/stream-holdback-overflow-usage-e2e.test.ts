import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  awaitWindowHeadroom,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const MODELS = {
  chat: "holdback-overflow-chat",
  completions: "holdback-overflow-completions",
  responses: "holdback-overflow-responses",
} as const;
const CALLERS = {
  chat: "sk-holdback-overflow-chat",
  completions: "sk-holdback-overflow-completions",
  responses: "sk-holdback-overflow-responses",
} as const;
const OVERFLOW_MARKER = "generated-trigger-chunk-must-be-billed ".repeat(256);
const TIMEOUT_MARKER = "generated timeout chunk must be billed";
const TIMEOUT_CALLER = "sk-held-partial-timeout";

function chatChunk(content: string): string {
  return JSON.stringify({
    id: "chatcmpl-overflow",
    object: "chat.completion.chunk",
    created: 0,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta: { content }, finish_reason: null }],
  });
}

function completionChunk(text: string): string {
  return JSON.stringify({
    id: "cmpl-overflow",
    object: "text_completion",
    created: 0,
    model: "gpt-3.5-turbo-instruct",
    choices: [{ index: 0, text, finish_reason: null }],
  });
}

async function metricCounter(
  app: SpawnedApp,
  metric: string,
  labels: Record<string, string>,
): Promise<number> {
  const response = await fetch(`${app.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  let total = 0;
  for (const line of (await response.text()).split("\n")) {
    if (!line.startsWith(`${metric}{`)) continue;
    if (!Object.entries(labels).every(([key, value]) => line.includes(`${key}="${value}"`))) {
      continue;
    }
    const value = Number(line.trim().split(/\s+/).at(-1));
    if (Number.isFinite(value)) total += value;
  }
  return total;
}

describe("fail-closed stream hold-back overflow usage", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let chatUpstream: OpenAiUpstream | undefined;
  let completionsUpstream: OpenAiUpstream | undefined;
  let responsesUpstream: OpenAiUpstream | undefined;
  let completionsTimeoutUpstream: OpenAiUpstream | undefined;
  let responsesTimeoutUpstream: OpenAiUpstream | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    chatUpstream = await startOpenAiUpstream({
      streamEvents: [chatChunk(OVERFLOW_MARKER), "[DONE]"],
    });
    completionsUpstream = await startOpenAiUpstream({
      streamEvents: [completionChunk(OVERFLOW_MARKER), "[DONE]"],
    });
    responsesUpstream = await startOpenAiUpstream({
      streamEvents: [
        JSON.stringify({
          type: "response.output_text.delta",
          delta: OVERFLOW_MARKER,
        }),
        "[DONE]",
      ],
    });
    completionsTimeoutUpstream = await startOpenAiUpstream({
      scriptedResponses: [
        {
          streamEvents: [
            completionChunk(TIMEOUT_MARKER),
            completionChunk("late"),
            "[DONE]",
          ],
          eventDelayMs: 3_000,
        },
        { streamEvents: [completionChunk("retry must not run"), "[DONE]"] },
      ],
    });
    responsesTimeoutUpstream = await startOpenAiUpstream({
      scriptedResponses: [
        {
          streamEvents: [
            JSON.stringify({ type: "response.output_text.delta", delta: TIMEOUT_MARKER }),
            JSON.stringify({
              type: "response.completed",
              response: { id: "resp-late", status: "completed" },
            }),
            "[DONE]",
          ],
          eventDelayMs: 3_000,
        },
        {
          streamEvents: [
            JSON.stringify({
              type: "response.completed",
              response: { id: "resp-retry", status: "completed" },
            }),
            "[DONE]",
          ],
        },
      ],
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const upstreams = {
      chat: chatUpstream,
      completions: completionsUpstream,
      responses: responsesUpstream,
    } as const;
    for (const endpoint of Object.keys(MODELS) as Array<keyof typeof MODELS>) {
      const providerKey = await seed.createProviderKey({
        display_name: `${MODELS[endpoint]}-pk`,
        secret: "sk-mock",
        api_base: `${upstreams[endpoint].baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: MODELS[endpoint],
        provider: "openai",
        model_name:
          endpoint === "completions" ? "gpt-3.5-turbo-instruct" : "gpt-4o-mini",
        provider_key_id: providerKey.id,
      });
    }
    for (const [display_name, target, model_name] of [
      ["held-timeout-completions", completionsTimeoutUpstream, "gpt-3.5-turbo-instruct"],
      ["held-timeout-responses", responsesTimeoutUpstream, "gpt-4o-mini"],
    ] as const) {
      const providerKey = await seed.createProviderKey({
        display_name: `${display_name}-pk`,
        secret: "sk-mock",
        api_base: `${target.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name,
        provider: "openai",
        model_name,
        provider_key_id: providerKey.id,
        stream_timeout: 250,
        retries: 1,
        cooldown: { default_seconds: 60 },
      });
    }
    await seed.createGuardrail({
      name: "holdback-overflow-pii",
      enabled: true,
      hook_point: "output",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
      max_buffer_bytes: 1_024,
      on_buffer_exceeded: "fail_closed",
    });
    for (const endpoint of Object.keys(MODELS) as Array<keyof typeof MODELS>) {
      await seed.createApiKey({
        key_hash: createHash("sha256").update(CALLERS[endpoint]).digest("hex"),
        allowed_models: [MODELS[endpoint]],
        rate_limit: { tpm: 10 },
      });
    }
    await seed.createApiKey({
      key_hash: createHash("sha256").update(TIMEOUT_CALLER).digest("hex"),
      allowed_models: ["held-timeout-completions", "held-timeout-responses"],
    });
    for (const endpoint of Object.keys(MODELS) as Array<keyof typeof MODELS>) {
      await waitConfigPropagation(async () => {
        const response = await fetch(`${app!.proxyUrl}/v1/models`, {
          headers: { authorization: `Bearer ${CALLERS[endpoint]}` },
        });
        if (response.status === 401) return false;
        if (response.status !== 200) {
          throw new Error(`model propagation probe returned ${response.status}`);
        }
        const body = (await response.json()) as { data?: Array<{ id?: string }> };
        return body.data?.some((model) => model.id === MODELS[endpoint]) === true;
      });
    }
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${TIMEOUT_CALLER}` },
      });
      if (response.status === 401) return false;
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((model) => model.id));
      return ids.has("held-timeout-completions") && ids.has("held-timeout-responses");
    });
  });

  afterAll(async () => {
    await app?.exit();
    await chatUpstream?.close();
    await completionsUpstream?.close();
    await responsesUpstream?.close();
    await completionsTimeoutUpstream?.close();
    await responsesTimeoutUpstream?.close();
  });

  const request = (endpoint: keyof typeof MODELS) => {
    const path = endpoint === "chat" ? "/v1/chat/completions" : `/v1/${endpoint}`;
    const body =
      endpoint === "chat"
        ? { model: MODELS.chat, stream: true, messages: [{ role: "user", content: "go" }] }
        : endpoint === "completions"
          ? { model: MODELS.completions, stream: true, prompt: "go" }
          : { model: MODELS.responses, stream: true, input: "go" };
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLERS[endpoint]}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
  };

  const cases = [
    { endpoint: "chat" as const, upstream: () => chatUpstream, status: 200 },
    {
      endpoint: "completions" as const,
      upstream: () => completionsUpstream,
      status: 422,
    },
    { endpoint: "responses" as const, upstream: () => responsesUpstream, status: 422 },
  ];

  for (const scenario of cases) {
    test(`${scenario.endpoint} bills the cap-triggering chunk and exhausts TPM`, async (ctx) => {
      const upstream = scenario.upstream();
      if (!etcdReachable || !app || !upstream) {
        ctx.skip();
        return;
      }
      const model = MODELS[scenario.endpoint];
      const upstreamBaseline = upstream.receivedRequests.length;
      const outputBaseline = await metricCounter(app, "aisix_llm_output_tokens_total", {
        model,
      });
      const usageBaseline = await metricCounter(app, "aisix_usage_events_emitted_total", {
        handler: scenario.endpoint,
        status_code: "4xx",
      });

      await awaitWindowHeadroom();
      const response = await request(scenario.endpoint);
      expect(response.status).toBe(scenario.status);
      const responseBody = await response.text();
      expect(responseBody).toContain("content_filter");
      expect(responseBody).not.toContain(OVERFLOW_MARKER);
      expect(upstream.receivedRequests).toHaveLength(upstreamBaseline + 1);

      await waitConfigPropagation(async () => {
        const usage = await metricCounter(app!, "aisix_usage_events_emitted_total", {
          handler: scenario.endpoint,
          status_code: "4xx",
        });
        return usage === usageBaseline + 1;
      });
      await waitConfigPropagation(async () => {
        const output = await metricCounter(app!, "aisix_llm_output_tokens_total", { model });
        return output > outputBaseline;
      });

      const throttled = await request(scenario.endpoint);
      expect(throttled.status).toBe(429);
      expect(upstream.receivedRequests).toHaveLength(upstreamBaseline + 1);
    }, 60_000);
  }

  for (const scenario of [
    {
      endpoint: "completions" as const,
      model: "held-timeout-completions",
      upstream: () => completionsTimeoutUpstream,
      status: 504,
    },
    {
      endpoint: "responses" as const,
      model: "held-timeout-responses",
      upstream: () => responsesTimeoutUpstream,
      status: 200,
    },
  ]) {
    test(`${scenario.endpoint} bills held output observed before a timeout without retrying`, async (ctx) => {
      const upstream = scenario.upstream();
      if (!etcdReachable || !app || !upstream) {
        ctx.skip();
        return;
      }
      const outputBaseline = await metricCounter(app, "aisix_llm_output_tokens_total", {
        model: scenario.model,
      });
      const usageBaseline = await metricCounter(app, "aisix_usage_events_emitted_total", {
        handler: scenario.endpoint,
        status_code: "5xx",
      });
      const upstreamBaseline = upstream.receivedRequests.length;
      const path = `/v1/${scenario.endpoint}`;
      const body =
        scenario.endpoint === "completions"
          ? { model: scenario.model, prompt: "go", stream: true }
          : { model: scenario.model, input: "go", stream: true };
      const response = await fetch(`${app.proxyUrl}${path}`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${TIMEOUT_CALLER}`,
          "content-type": "application/json",
        },
        body: JSON.stringify(body),
      });
      const responseBody = await response.text();
      expect(response.status, responseBody).toBe(scenario.status);
      expect(responseBody).not.toContain(TIMEOUT_MARKER);
      expect(upstream.receivedRequests).toHaveLength(upstreamBaseline + 1);
      try {
        await waitConfigPropagation(async () => {
          const usage = await metricCounter(app!, "aisix_usage_events_emitted_total", {
            handler: scenario.endpoint,
            status_code: "5xx",
          });
          return usage === usageBaseline + 1;
        });
        await waitConfigPropagation(async () => {
          const output = await metricCounter(app!, "aisix_llm_output_tokens_total", {
            model: scenario.model,
          });
          return output > outputBaseline;
        });
      } catch (error) {
        const usage = await metricCounter(app, "aisix_usage_events_emitted_total", {
          handler: scenario.endpoint,
          status_code: "5xx",
        });
        const output = await metricCounter(app, "aisix_llm_output_tokens_total", {
          model: scenario.model,
        });
        throw new Error(
          `${scenario.endpoint}: ${String(error)}; usage ${usageBaseline}->${usage}, output ${outputBaseline}->${output}`,
        );
      }
    }, 70_000);
  }
});
