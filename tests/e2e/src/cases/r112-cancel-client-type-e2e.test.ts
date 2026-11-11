import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server, type ServerResponse } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  metricDelta,
  scrapeMetrics,
  slsLogsFor,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// Titan 每个文本调用一次 InvokeModel，已完成文本各报告 4 个 input token：
// https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-titan-embed-text.html
// 取消发生在第三个文本，故取消请求计 8，后续成功请求计 4。
// 若取消路径跳过 classifier，分类标签只有 4，raw UA 标签另有 8。
const CALLER = "sk-r112-client-type-test";
const LOGSTORE = "r112-client-type";
const SCENARIOS = [
  { client: "openai-python", uas: ["OpenAI/Python 1.2.3", "OpenAI/Python 2.0.0"] },
  { client: "openai-node", uas: ["OpenAI/JS 5.1.0", "OpenAI/JS 6.0.0"] },
  { client: "anthropic-python", uas: ["Anthropic/Python 1.2.3", "Anthropic/Python 2.0.0"] },
  { client: "anthropic-typescript", uas: ["Anthropic/JS 1.2.3", "Anthropic/JS 2.0.0"] },
  { client: "enterprise-client", uas: ["R112Enterprise/alpha", "R112Enterprise/beta"] },
  { client: "other", uas: ["R112Synthetic/alpha", "R112Synthetic/beta"] },
] as const;

describe("cancelled token metrics use the same bounded client type as successful requests", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let sls: MockSls | undefined;
  let reachable = false;
  const received: string[] = [];
  const held: ServerResponse[] = [];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    reachable = await etcd.ping();
    if (!reachable) return;
    upstream = createServer((req, res) => {
      let raw = "";
      req.on("data", (chunk) => (raw += chunk));
      req.on("end", () => {
        const text = (JSON.parse(raw) as { inputText: string }).inputText;
        received.push(text);
        if (text === "hangs") {
          held.push(res);
          return;
        }
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ embedding: [0.25, 0.5], inputTextTokenCount: 4 }));
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const address = upstream.address();
    if (address === null || typeof address === "string") throw new Error("no upstream port");
    sls = await startMockSls();
    app = await spawnApp({
      clientTypeRules: [{ pattern: "^R112Enterprise/", client: "enterprise-client" }],
      extraEnv: {
        SLS_CRED_R112_AK_ID: "r112-test-akid",
        SLS_CRED_R112_AK_SECRET: "r112-test-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "r112-client-type",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "r112-test",
      logstore: LOGSTORE,
      credential_ref: "r112",
      content_mode: "metadata_only",
    });
    const pk = await seed.createProviderKey({
      display_name: "r112-test-peer",
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({
        access_key_id: "AKIA-r112-test",
        secret_access_key: "r112-test-secret",
        region: "us-east-1",
      }),
      api_base: `http://127.0.0.1:${address.port}`,
    });
    for (const client of [...SCENARIOS.map((scenario) => scenario.client), "unserved"]) {
      await seed.createModel({
        display_name: `r112-${client}`,
        provider: "bedrock",
        model_name: "amazon.titan-embed-text-v1",
        provider_key_id: pk.id,
      });
    }
    // 最后写调用者，认证成功即证明整个种子集合已传播。
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      allowed_models: ["*"],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      await response.text();
      return response.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    for (const response of held) response.destroy();
    await sls?.close();
    await new Promise<void>((resolve) => (upstream ? upstream.close(() => resolve()) : resolve()));
  });

  async function cancel(model: string, ua: string, input: string[]): Promise<string> {
    const requestId = randomUUID();
    const controller = new AbortController();
    const offset = received.length;
    const pending = fetch(`${app!.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
        "user-agent": ua,
        "x-aisix-request-id": requestId,
      },
      body: JSON.stringify({ model, input }),
      signal: controller.signal,
    }).then((response) => response.text());
    try {
      await waitConfigPropagation(async () => received.length === offset + input.length);
      expect(received.slice(offset)).toEqual(input);
    } finally {
      controller.abort();
      await expect(pending).rejects.toThrow();
    }
    return requestId;
  }

  for (const scenario of SCENARIOS) {
    test(`${scenario.client}: cancelled 8 and successful 4 aggregate across UA versions`, async (ctx) => {
      if (!reachable || !app || !sls) return ctx.skip();
      const model = `r112-${scenario.client}`;
      const before = await scrapeMetrics(app.metricsUrl);
      // 两个 UA 各取消一次；对 other 这也验证 nonce 不扩张标签集合。
      for (const [index, ua] of scenario.uas.entries()) {
        const cancelledId = await cancel(model, ua, ["served", "served", "hangs"]);
        const row = await waitForSlsLog(
          sls,
          LOGSTORE,
          (log) => log.get("request_id") === cancelledId,
          "the cancelled request's terminal row",
        );
        expect(row.get("status_code")).toBe("499");
        expect(row.get("prompt_tokens")).toBe("8");
        const response = await fetch(`${app.proxyUrl}/v1/embeddings`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${CALLER}`,
            "content-type": "application/json",
            "user-agent": ua,
          },
          body: JSON.stringify({ model, input: ["served"] }),
        });
        expect(response.status).toBe(200);
        await response.json();
        const successId = response.headers.get("x-aisix-request-id");
        expect(successId).toBeTruthy();
        // 后发成功事件充当队列屏障，之后检查取消事件恰好一次。
        await waitForSlsLog(
          sls,
          LOGSTORE,
          (log) => log.get("request_id") === successId,
          "the later successful request's row",
        );
        expect(slsLogsFor(sls, LOGSTORE).filter((log) => log.get("request_id") === cancelledId)).toHaveLength(1);
        const after = await scrapeMetrics(app.metricsUrl);
        const expected = (index + 1) * 12;
        expect(metricDelta(before, after, "aisix_llm_input_tokens_total", { model })).toBe(expected);
        for (const token_type of ["input", "total"]) {
          expect.soft(metricDelta(before, after, "aisix_llm_tokens_by_client_total", {
            client_type: scenario.client,
            model,
            token_type,
          })).toBe(expected);
        }
        const clients = after
          .filter((sample) => sample.name === "aisix_llm_tokens_by_client_total" && sample.labels.model === model)
          .map((sample) => sample.labels.client_type);
        expect.soft(new Set(clients)).toEqual(new Set([scenario.client]));
      }
    });
  }

  test("cancelling before any text is served emits no token counts", async (ctx) => {
    if (!reachable || !app || !sls) return ctx.skip();
    const model = "r112-unserved";
    const before = await scrapeMetrics(app.metricsUrl);
    const requestId = await cancel(model, "R112Synthetic/unserved", ["hangs"]);
    const row = await waitForSlsLog(
      sls,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      "the unserved cancellation row",
    );
    expect(row.get("status_code")).toBe("499");
    expect(row.has("prompt_tokens")).toBe(true);
    expect(row.get("prompt_tokens")).toBe("0");
    const after = await scrapeMetrics(app.metricsUrl);
    for (const metric of ["aisix_llm_input_tokens_total", "aisix_llm_tokens_by_client_total"]) {
      expect(metricDelta(before, after, metric, { model })).toBe(0);
    }
  });
});
