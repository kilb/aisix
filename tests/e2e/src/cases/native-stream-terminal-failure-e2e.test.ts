import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  metricDelta,
  scrapeMetrics,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-native-terminal-failure";
const MODELS = {
  messagesMalformed: "native-messages-malformed-sse",
  messagesInBand: "native-messages-in-band-error",
  responsesMalformed: "native-responses-malformed-sse",
  responsesTruncated: "native-responses-truncated-sse",
  responsesInBand: "native-responses-in-band-error",
} as const;

async function usage5xxCounter(app: SpawnedApp, handler: string): Promise<number> {
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
        candidate.includes('status_code="5xx"'),
    );
  return line ? Number(line.trim().split(/\s+/).at(-1)) : 0;
}

async function outputTokenCounter(app: SpawnedApp, model: string): Promise<number> {
  const response = await fetch(`${app.metricsUrl}/metrics`);
  if (response.status !== 200) {
    throw new Error(`metrics probe returned ${response.status}`);
  }
  let total = 0;
  for (const line of (await response.text()).split("\n")) {
    if (!line.startsWith("aisix_llm_output_tokens_total{")) continue;
    if (!line.includes(`model="${model}"`)) continue;
    const value = Number(line.trim().split(/\s+/).at(-1));
    if (Number.isFinite(value)) total += value;
  }
  return total;
}

async function waitForFailure(
  app: SpawnedApp,
  modelId: string,
  model: string,
  endpoint: "/v1/messages" | "/v1/responses",
  reason: string,
  usageBaseline: number,
  outputBaseline: number,
  expectedOutputTokens?: number,
): Promise<void> {
  let observedStatus: unknown;
  try {
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app.metricsUrl}/status/models`);
      if (response.status !== 200) {
        throw new Error(`model status probe returned ${response.status}`);
      }
      const rows = (await response.json()) as Array<{
        id?: string;
        status?: string;
        status_reason?: string;
      }>;
      observedStatus = rows.find((row) => row.id === modelId);
      return rows.some(
        (row) =>
          row.id === modelId &&
          row.status === "cooldown" &&
          row.status_reason === reason,
      );
    });
  } catch (error) {
    throw new Error(
      `${String(error)}; observed model status: ${JSON.stringify(observedStatus)}`,
    );
  }

  try {
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app.metricsUrl}/metrics`);
      if (response.status !== 200) {
        throw new Error(`metrics probe returned ${response.status}`);
      }
      const metrics = await response.text();
      return metrics.split("\n").some(
        (line) =>
          line.startsWith("aisix_request_e2e_latency_seconds_count{") &&
          line.includes(`endpoint="${endpoint}"`) &&
          line.includes(`model="${model}"`) &&
          line.includes('status_class="5xx"') &&
          line.includes('streaming="true"'),
      );
    });
  } catch (error) {
    throw new Error(`e2e metric: ${String(error)}`);
  }

  const handler = endpoint.slice(4);
  try {
    await waitConfigPropagation(
      async () => (await usage5xxCounter(app, handler)) === usageBaseline + 1,
    );
  } catch (error) {
    throw new Error(
      `usage event: ${String(error)}; ${usageBaseline}->${await usage5xxCounter(app, handler)}`,
    );
  }

  try {
    await waitConfigPropagation(async () => {
      const delta = (await outputTokenCounter(app, model)) - outputBaseline;
      return expectedOutputTokens === undefined
        ? delta > 0
        : delta === expectedOutputTokens;
    });
  } catch (error) {
    throw new Error(
      `output tokens: ${String(error)}; ${outputBaseline}->${await outputTokenCounter(app, model)}`,
    );
  }
}

describe("native stream protocol failures are not healthy completions", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  const upstreams: Partial<Record<keyof typeof MODELS, OpenAiUpstream>> = {};
  const modelIds: Partial<Record<keyof typeof MODELS, string>> = {};

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstreams.messagesMalformed = await startOpenAiUpstream({
      rawSseChunks: [
        'event: message_start\ndata: {"type":"message_start","message":{"id":"msg-malformed","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":3,"output_tokens":1}}}\n\n',
        'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"billed malformed output"}}\n\n',
        "event: content_block_delta\ndata: not-json\n\n",
      ],
    });
    upstreams.messagesInBand = await startOpenAiUpstream({
      rawSseChunks: [
        'event: message_start\ndata: {"type":"message_start","message":{"id":"msg-error","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":3,"output_tokens":1}}}\n\n',
        'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial billed output"}}\n\n',
        'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":4}}\n\n',
        'event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"provider overloaded"}}\n\n',
      ],
    });
    upstreams.responsesMalformed = await startOpenAiUpstream({
      rawSseChunks: [
        'data: {"type":"response.created","response":{"id":"resp-malformed","status":"in_progress"}}\n\n',
        'data: {"type":"response.output_text.delta","delta":"billed malformed output"}\n\n',
        "data: not-json\n\n",
      ],
    });
    upstreams.responsesTruncated = await startOpenAiUpstream({
      rawSseChunks: [
        'data: {"type":"response.created","response":{"id":"resp-truncated","status":"in_progress"}}\n\n',
        'data: {"type":"response.output_text.delta","delta":"billed truncated output"}\n\n',
      ],
    });
    upstreams.responsesInBand = await startOpenAiUpstream({
      rawSseChunks: [
        'data: {"type":"response.created","response":{"id":"resp-error","status":"in_progress"}}\n\n',
        'data: {"type":"response.output_text.delta","delta":"partial billed output"}\n\n',
        'data: {"type":"response.failed","response":{"id":"resp-error","status":"failed","output":[],"error":{"code":"server_error","message":"provider failed"},"usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7}}}\n\n',
        "data: [DONE]\n\n",
      ],
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const key of Object.keys(MODELS) as Array<keyof typeof MODELS>) {
      const messages = key.startsWith("messages");
      const providerKey = await seed.createProviderKey({
        display_name: `native-terminal-${key}-pk`,
        provider: messages ? "anthropic" : "openai",
        adapter: messages ? "anthropic" : "openai",
        secret: messages ? "sk-ant-mock" : "sk-mock",
        api_base: messages
          ? upstreams[key]!.baseUrl
          : `${upstreams[key]!.baseUrl}/v1`,
      });
      const model = await seed.createModel({
        display_name: MODELS[key],
        provider: messages ? "anthropic" : "openai",
        model_name: messages ? "claude" : "gpt-4o-mini",
        provider_key_id: providerKey.id,
        cooldown: messages
          ? { default_seconds: 60, trigger_statuses: [529] }
          : { default_seconds: 60 },
      });
      modelIds[key] = model.id;
    }

    const guardrail = await seed.createGuardrail({
      name: "native-terminal-holdback",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: "never-matches-terminal-fixture" }],
    });
    for (const key of [
      "messagesMalformed",
      "responsesMalformed",
      "responsesTruncated",
    ] as const) {
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: guardrail.id,
        env_id: randomUUID(),
        scope_type: "model",
        scope_id: modelIds[key],
        priority: 0,
        enabled: true,
      });
    }
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      allowed_models: Object.values(MODELS),
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
      return Object.values(MODELS).every((model) => ids.has(model));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(Object.values(upstreams).map((upstream) => upstream.close()));
  });

  const post = (endpoint: "/v1/messages" | "/v1/responses", model: string) =>
    fetch(`${app!.proxyUrl}${endpoint}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify(
        endpoint === "/v1/messages"
          ? {
              model,
              max_tokens: 32,
              stream: true,
              messages: [{ role: "user", content: `request ${model}` }],
            }
          : { model, input: `request ${model}`, stream: true },
      ),
    });

  test("malformed or truncated held SSE is suppressed and counted as upstream failure", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const cases = [
      {
        endpoint: "/v1/messages" as const,
        modelKey: "messagesMalformed" as const,
        upstream: upstreams.messagesMalformed!,
        reason: "upstream_decode_error",
        rawMarker: "billed malformed output",
      },
      {
        endpoint: "/v1/responses" as const,
        modelKey: "responsesMalformed" as const,
        upstream: upstreams.responsesMalformed!,
        reason: "upstream_decode_error",
        rawMarker: "billed malformed output",
      },
      {
        endpoint: "/v1/responses" as const,
        modelKey: "responsesTruncated" as const,
        upstream: upstreams.responsesTruncated!,
        reason: "transport_error",
        rawMarker: "billed truncated output",
      },
    ];
    for (const scenario of cases) {
      const baseline = scenario.upstream.receivedRequests.length;
      const usageBaseline = await usage5xxCounter(app, scenario.endpoint.slice(4));
      const outputBaseline = await outputTokenCounter(
        app,
        MODELS[scenario.modelKey],
      );
      const deploymentBaseline = await scrapeMetrics(app.metricsUrl);
      const response = await post(scenario.endpoint, MODELS[scenario.modelKey]);
      expect(response.status).toBe(200);
      const body = await response.text();
      expect(body).toContain("upstream stream failed");
      expect(body).not.toContain(scenario.rawMarker);
      expect(body).not.toContain("not-json");
      expect(scenario.upstream.receivedRequests).toHaveLength(baseline + 1);
      try {
        await waitForFailure(
          app,
          modelIds[scenario.modelKey]!,
          MODELS[scenario.modelKey],
          scenario.endpoint,
          scenario.reason,
          usageBaseline,
          outputBaseline,
        );
      } catch (error) {
        throw new Error(`${scenario.modelKey}: ${String(error)}`);
      }
      const deploymentAfter = await scrapeMetrics(app.metricsUrl);
      expect(
        metricDelta(
          deploymentBaseline,
          deploymentAfter,
          "aisix_deployment_failure_responses_total",
          { model: MODELS[scenario.modelKey] },
        ),
      ).toBe(1);
      expect(
        metricDelta(
          deploymentBaseline,
          deploymentAfter,
          "aisix_deployment_success_responses_total",
          { model: MODELS[scenario.modelKey] },
        ),
      ).toBe(0);
    }
  });

  const inBandCases = [
    {
      endpoint: "/v1/messages" as const,
      modelKey: "messagesInBand" as const,
      upstream: () => upstreams.messagesInBand,
      reason: "upstream_server_error",
      wireMarker: "overloaded_error",
    },
    {
      endpoint: "/v1/responses" as const,
      modelKey: "responsesInBand" as const,
      upstream: () => upstreams.responsesInBand,
      reason: "upstream_in_band_error",
      wireMarker: "response.failed",
    },
  ];
  for (const scenario of inBandCases) {
    test(`${scenario.endpoint} provider-declared failure preserves usage and marks the target failed`, async (ctx) => {
      const upstream = scenario.upstream();
      if (!etcdReachable || !app || !upstream) {
        ctx.skip();
        return;
      }
      const baseline = upstream.receivedRequests.length;
      const usageBaseline = await usage5xxCounter(app, scenario.endpoint.slice(4));
      const outputBaseline = await outputTokenCounter(
        app,
        MODELS[scenario.modelKey],
      );
      const response = await post(scenario.endpoint, MODELS[scenario.modelKey]);
      expect(response.status).toBe(200);
      const body = await response.text();
      expect(body).toContain(scenario.wireMarker);
      expect(body).toContain("partial billed output");
      expect(upstream.receivedRequests).toHaveLength(baseline + 1);
      await waitForFailure(
        app,
        modelIds[scenario.modelKey]!,
        MODELS[scenario.modelKey],
        scenario.endpoint,
        scenario.reason,
        usageBaseline,
        outputBaseline,
        4,
      );
    });
  }
});
