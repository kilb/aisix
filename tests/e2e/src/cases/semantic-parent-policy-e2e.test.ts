import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
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
import { pickFreePort } from "../harness/ports.js";

// E2E：按语义路由名写的策略，对一个调用方请求只计一次，而且在分类调用之前就生效。
//
// 语义路由为了选路，先打一发嵌入分类调用（真实上游、花真钱），再分派到选中的
// 目标。调用方寻址的是路由本身，所以写着 `model_name == <路由名>` 的策略：
//
// `rpm: 2` 放行两个请求、拒第三个 —— 分类子调用和目标不能各记一次；而第三个
// 请求被拒时，分类调用根本不该发出去（额度已经用完，没必要再花一次嵌入的钱）。

const CALLER_PLAINTEXT = "sk-semantic-parent-policy-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

const RPM_ROUTER = "spp-router";

interface EmbeddingMock {
  baseUrl: string;
  calls(): number;
  close(): Promise<void>;
}

/** 固定返回同一个向量、并记下被调用次数的 `/v1/embeddings` 上游。 */
async function startEmbeddingMock(): Promise<EmbeddingMock> {
  let calls = 0;
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      calls += 1;
      let inputs: string[] = [""];
      try {
        const body = JSON.parse(raw || "{}") as { input?: string | string[] };
        inputs = Array.isArray(body.input) ? body.input : [body.input ?? ""];
      } catch {
        // 解析失败按单条输入回，断言会在路由结果上暴露
      }
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          object: "list",
          model: "embed-mock",
          data: inputs.map((_, index) => ({
            object: "embedding",
            index,
            embedding: [0, 1, 0, 0],
          })),
          usage: { prompt_tokens: 1000, total_tokens: 1000 },
        }),
      );
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    calls: () => calls,
    async close() {
      server.closeAllConnections?.();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("a policy pinning a semantic router counts each caller request once", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;
  let upstream: OpenAiUpstream | undefined;
  let embed: EmbeddingMock | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    // 策略最先写（修订号最小）：后写的 key 能通过鉴权时，它必然已经生效。
    await seed.createRateLimitPolicy({
      name: "spp-router-rpm",
      conditions: [{ dimension: "model_name", operator: "==", value: RPM_ROUTER }],
      limits: { rpm: 2 },
    });

    embed = await startEmbeddingMock();
    const embedPk = await seed.createProviderKey({
      display_name: "spp-bge-pk",
      secret: "sk-mock",
      api_base: `${embed.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "spp-bge",
      provider: "openai",
      model_name: "embed-mock",
      provider_key_id: embedPk.id,
      embedding: { dimensions: 4 },
    });

    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-spp",
        object: "chat.completion",
        created: 0,
        model: "gpt-4o-mini",
        choices: [
          { index: 0, message: { role: "assistant", content: "served" }, finish_reason: "stop" },
        ],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      },
    });
    const chatPk = await seed.createProviderKey({
      display_name: "spp-target-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "spp-target",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: chatPk.id,
    });

    await seed.createModel({
      display_name: RPM_ROUTER,
      semantic: {
        embedding_model: "spp-bge",
        routes: [
          { name: "code", target: "spp-target", examples: ["write python code"], threshold: 0.5 },
        ],
        default: "spp-target",
        match: { threshold: 0.5 },
      },
    });

    // key 最后写：它能鉴权时，按修订号顺序前面的策略与模型都已生效。
    // 用 /v1/models 探测而不是打路由本身：探测不能消耗被测的桶。
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await embed?.close();
  });

  async function threeRequests(router: string): Promise<number[]> {
    const statuses: number[] = [];
    for (let i = 0; i < 3; i++) {
      const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${CALLER_PLAINTEXT}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model: router,
          messages: [{ role: "user", content: "write python code" }],
        }),
      });
      await res.text();
      statuses.push(res.status);
    }
    return statuses;
  }

  test("rpm: 2 on the router admits two requests, and the third never reaches the classifier", async (ctx) => {
    if (!etcdReachable || !app || !embed) {
      ctx.skip();
      return;
    }
    // 固定窗口：三次请求必须落在同一分钟里，否则第三次会拿到新额度。
    await awaitWindowHeadroom(15);
    const embedBefore = embed.calls();

    expect(await threeRequests(RPM_ROUTER)).toEqual([200, 200, 429]);
    expect(
      embed.calls() - embedBefore,
      "the exhausted router policy must stop the classifier call, not just the target",
    ).toBe(2);
  });
});
