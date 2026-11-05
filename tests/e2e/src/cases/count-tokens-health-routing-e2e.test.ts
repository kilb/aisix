import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  metricDelta,
  pickFreePort,
  scrapeMetrics,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-count-tokens-health-routing";
const MODELS = {
  primary: "ct-health-primary",
  fallback: "ct-health-fallback",
  group: "ct-health-group",
  truncated: "ct-health-truncated",
  control: "ct-health-control",
  counted500: "ct-health-counted-500",
  counted429: "ct-health-counted-429",
} as const;

describe("count_tokens health and routing telemetry", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let primary: OpenAiUpstream | undefined;
  let fallback: OpenAiUpstream | undefined;
  let limited: OpenAiUpstream | undefined;
  let truncatedServer: Server | undefined;
  let truncatedBaseUrl = "";
  let truncatedRequests = 0;
  const modelIds: Partial<Record<keyof typeof MODELS, string>> = {};

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    primary = await startOpenAiUpstream({
      status: 500,
      errorBody: { error: { message: "count failed" } },
    });
    fallback = await startOpenAiUpstream({ nonStreamBody: { input_tokens: 42 } });
    limited = await startOpenAiUpstream({
      status: 429,
      errorBody: { type: "error", error: { type: "rate_limit_error", message: "slow down" } },
    });
    truncatedServer = createServer((req, res) => {
      req.resume();
      req.on("end", () => {
        truncatedRequests += 1;
        res.statusCode = 200;
        res.setHeader("content-type", "application/json");
        res.setHeader("content-length", "128");
        res.flushHeaders();
        res.write('{"input_tokens":', () => res.socket?.destroy());
      });
    });
    const port = await pickFreePort();
    await new Promise<void>((resolve) =>
      truncatedServer!.listen(port, "127.0.0.1", resolve),
    );
    truncatedBaseUrl = `http://127.0.0.1:${port}`;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const createDirect = async (name: string, apiBase: string) => {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        provider: "anthropic",
        adapter: "anthropic",
        secret: "sk-ant-mock",
        api_base: apiBase,
      });
      const model = await seed.createModel({
        display_name: name,
        provider: "anthropic",
        model_name: "claude",
        provider_key_id: pk.id,
        retries: 0,
        cooldown: { enabled: true, default_seconds: 60 },
      });
      return model.id;
    };
    modelIds.primary = await createDirect(MODELS.primary, primary.baseUrl);
    modelIds.fallback = await createDirect(MODELS.fallback, fallback.baseUrl);
    modelIds.truncated = await createDirect(MODELS.truncated, truncatedBaseUrl);
    modelIds.control = await createDirect(MODELS.control, primary.baseUrl);
    modelIds.counted500 = await createDirect(MODELS.counted500, primary.baseUrl);
    modelIds.counted429 = await createDirect(MODELS.counted429, limited.baseUrl);
    const group = await seed.createModel({
      display_name: MODELS.group,
      routing: {
        strategy: "failover",
        targets: [{ model: MODELS.primary }, { model: MODELS.fallback }],
        max_fallbacks: 1,
      },
    });
    modelIds.group = group.id;
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
    await primary?.close();
    await fallback?.close();
    await limited?.close();
    if (truncatedServer) {
      await new Promise<void>((resolve, reject) =>
        truncatedServer!.close((error) => (error ? reject(error) : resolve())),
      );
    }
  });

  const countTokens = (model: string) =>
    fetch(`${app!.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model,
        messages: [{ role: "user", content: `count ${model}` }],
      }),
    });

  const generate = (model: string) =>
    fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model,
        max_tokens: 8,
        messages: [{ role: "user", content: `generate ${model}` }],
      }),
    });

  const statusOf = async (id: string | undefined) => {
    const status = await fetch(`${app!.metricsUrl}/status/models`);
    if (status.status !== 200) {
      throw new Error(`model status probe returned ${status.status}`);
    }
    const rows = (await status.json()) as Array<{
      id?: string;
      status?: string;
      status_reason?: string;
    }>;
    return rows.find((row) => row.id === id);
  };

  test("records the failed deployment and successful fallback", async (ctx) => {
    if (!etcdReachable || !app || !primary || !fallback) {
      ctx.skip();
      return;
    }
    const before = await scrapeMetrics(app.metricsUrl);
    const primaryBaseline = primary.receivedRequests.length;
    const fallbackBaseline = fallback.receivedRequests.length;

    const response = await countTokens(MODELS.group);
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ input_tokens: 42 });
    expect(primary.receivedRequests).toHaveLength(primaryBaseline + 1);
    expect(fallback.receivedRequests).toHaveLength(fallbackBaseline + 1);
    expect(primary.receivedRequests.at(-1)?.path).toBe("/v1/messages/count_tokens");
    expect(fallback.receivedRequests.at(-1)?.path).toBe("/v1/messages/count_tokens");

    const after = await scrapeMetrics(app.metricsUrl);
    const delta = (name: string, labels: Record<string, string>) =>
      metricDelta(before, after, name, labels);
    expect(
      delta("aisix_deployment_failure_responses_total", {
        model: MODELS.primary,
      }),
    ).toBe(1);
    expect(
      delta("aisix_deployment_success_responses_total", {
        model: MODELS.fallback,
      }),
    ).toBe(1);
    expect(
      delta("aisix_routing_successful_fallbacks_total", {
        model: MODELS.group,
        fallback_model: MODELS.fallback,
      }),
    ).toBe(1);

    // The failure left the member available: the next call through the group
    // tries it again rather than skipping it.
    expect((await countTokens(MODELS.group)).status).toBe(200);
    expect(primary.receivedRequests).toHaveLength(primaryBaseline + 2);
  });

  // A server error is a 5xx to the caller, whichever one; a rate limit is the
  // 429 the upstream answered.
  const isServerError = (status: number) => status >= 500;
  const isRateLimit = (status: number) => status === 429;
  for (const [label, key, failedAs, coolingReason] of [
    ["5xx", "counted500", isServerError, "upstream_server_error"],
    ["429", "counted429", isRateLimit, "upstream_rate_limited"],
  ] as const) {
    test(`a ${label} on count_tokens leaves the model available, and a generation failure still cools it`, async (ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      const before = await scrapeMetrics(app.metricsUrl);
      const counted = await countTokens(MODELS[key]);
      expect(failedAs(counted.status), `count_tokens answered ${counted.status}`).toBe(true);

      // Read once the failure is on record, so this answer comes after
      // anything that failure was going to do.
      const after = await scrapeMetrics(app.metricsUrl);
      expect(
        metricDelta(before, after, "aisix_deployment_failure_responses_total", {
          model: MODELS[key],
        }),
      ).toBe(1);
      const row = await statusOf(modelIds[key]);
      expect(row?.status).toBe("healthy");
      expect(row?.status_reason).toBeUndefined();

      // The same model and the same status on generation does cool down, so
      // the read above is one that could have found a cooldown.
      const generated = await generate(MODELS[key]);
      expect(failedAs(generated.status), `generation answered ${generated.status}`).toBe(true);
      await waitConfigPropagation(async () => {
        const cooled = await statusOf(modelIds[key]);
        return cooled?.status === "cooldown" && cooled.status_reason === coolingReason;
      });
    });
  }

  test("a truncated 200 is a decode failure and leaves the target healthy", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const before = await scrapeMetrics(app.metricsUrl);
    const baseline = truncatedRequests;
    const response = await countTokens(MODELS.truncated);
    expect(response.status).toBe(502);
    expect(truncatedRequests).toBe(baseline + 1);

    // A real generation failure, sent after it, cools its own target: once
    // that shows, the read below is one that could have found a cooldown.
    const control = await generate(MODELS.control);
    expect(control.status).toBeGreaterThanOrEqual(500);
    await waitConfigPropagation(async () => {
      const row = await statusOf(modelIds.control);
      return row?.status === "cooldown" && row.status_reason === "upstream_server_error";
    });
    const row = await statusOf(modelIds.truncated);
    expect(row?.status).toBe("healthy");
    expect(row?.status_reason).toBeUndefined();

    const after = await scrapeMetrics(app.metricsUrl);
    expect(
      metricDelta(before, after, "aisix_deployment_failure_responses_total", {
        model: MODELS.truncated,
      }),
    ).toBe(1);
  });
});
