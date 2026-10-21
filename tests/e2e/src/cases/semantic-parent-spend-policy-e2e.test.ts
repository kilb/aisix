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

// E2E：语义路由自己的花费上限，一次分类调用的花费只记一次。
//
// 语义路由先打一发嵌入分类调用（按嵌入模型定价，花真钱），再分派到选中的
// 目标。路由自己那一行上的花费上限要把两笔都算进去 —— 但分类调用那一笔只能
// 算一次；算两次，额度会提前用完，调用方被提前拒绝。
//
// 花费上限只有 classic 形态（`scope: model` 指向路由那一行）：条件形态带不了
// `max_spend_micro_usd`，写入时就被拒。这条用例钉住的是能配出来的那一种。
//
// 定价：分类调用 1000 个嵌入 token × $0.01/1k = $0.01（10 000 micro-USD），
// 目标调用 $0.000002。上限每分钟 15 000 micro-USD：按实际花费，两个请求之后
// 才越过上限（第三个被拒）；分类花费记两次的话，第一个请求之后就越过了。
// 一个非流式、一个流式：两条提交路径各走一次。
//
// 反过来，目标模型自己那一行上的花费上限只管它自己的花费：分类调用是另一个
// 模型（嵌入模型）的花费，记在嵌入模型自己的账上；再记进目标的上限，同一笔
// 钱就算了两次，目标被别人的花费拒掉。

const CALLER_PLAINTEXT = "sk-semantic-parent-spend-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

const ROUTER = "sps-router";
const ROUTER_B = "sps-router-b";

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

describe("a spend ceiling pinning a semantic router counts the classifier call once", () => {
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

    embed = await startEmbeddingMock();
    const embedPk = await seed.createProviderKey({
      display_name: "sps-bge-pk",
      secret: "sk-mock",
      api_base: `${embed.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sps-bge",
      provider: "openai",
      model_name: "embed-mock",
      provider_key_id: embedPk.id,
      embedding: { dimensions: 4 },
      cost: { input_per_1k: 0.01, output_per_1k: 0 },
    });

    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-sps",
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
      display_name: "sps-target-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    const targetB = await seed.createModel({
      display_name: "sps-target-b",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: chatPk.id,
      cost: { input_per_1k: 0.001, output_per_1k: 0.001 },
    });
    await seed.createModel({
      display_name: ROUTER_B,
      semantic: {
        embedding_model: "sps-bge",
        routes: [
          { name: "code", target: "sps-target-b", examples: ["write python code"], threshold: 0.5 },
        ],
        default: "sps-target-b",
        match: { threshold: 0.5 },
      },
    });
    await seed.createModel({
      display_name: "sps-target",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: chatPk.id,
      cost: { input_per_1k: 0.001, output_per_1k: 0.001 },
    });

    const router = await seed.createModel({
      display_name: ROUTER,
      semantic: {
        embedding_model: "sps-bge",
        routes: [
          { name: "code", target: "sps-target", examples: ["write python code"], threshold: 0.5 },
        ],
        default: "sps-target",
        match: { threshold: 0.5 },
      },
    });

    // 花费上限按路由自己的行写（花费上限只有 classic 形态）。
    await seed.createRateLimitPolicy({
      name: "sps-router-spend",
      scope: "model",
      scope_ref: router.id,
      window: "minute",
      max_spend_micro_usd: 15_000,
    });
    await seed.createRateLimitPolicy({
      name: "sps-target-b-spend",
      scope: "model",
      scope_ref: targetB.id,
      window: "minute",
      max_spend_micro_usd: 15_000,
    });

    // key 最后写；用 /v1/models 探测，不消耗被测的桶。
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

  async function send(stream: boolean, model = ROUTER): Promise<number> {
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify({
        model,
        stream,
        messages: [{ role: "user", content: "write python code" }],
      }),
    });
    await res.text();
    return res.status;
  }

  test("two requests fit under the ceiling and the third is refused", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    // 固定窗口：三个请求必须落在同一分钟里。
    await awaitWindowHeadroom(20);
    const statuses: number[] = [];
    // 一个非流式、一个流式：两条提交路径各走一次。
    statuses.push(await send(false));
    statuses.push(await send(true));
    // 流式的花费在流结束后才记进去；给它一点时间落账。
    await new Promise((r) => setTimeout(r, 300));
    statuses.push(await send(false));
    expect(statuses).toEqual([200, 200, 429]);
  });

  test("a ceiling on the target model counts only the target's own spend", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(20);
    const statuses: number[] = [];
    statuses.push(await send(false, ROUTER_B));
    statuses.push(await send(true, ROUTER_B));
    await new Promise((r) => setTimeout(r, 300));
    statuses.push(await send(false, ROUTER_B));
    // 目标三次一共花了 $0.000006，离上限很远。
    expect(statuses).toEqual([200, 200, 200]);
  });
});
