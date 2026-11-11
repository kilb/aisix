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

const PATTERN_KEY = "sk-r115-pattern-caller";
const ID_KEY = "sk-r115-public-id-caller";
const INTERNAL_NAME = "r115-public#*";
const INTERNAL_ALIAS = "r115-public#unlisted-account";
const PUBLIC_NAME = "r115-public";
const PUBLIC_WILDCARD = "r115-visible/*";

// 若 internal wildcard 进入客户端别名解析，下面的 404 与零上游断言会同时失败。
// 名称模式授权是此回归的必要前提；仅有 public ID 授权的对照不能替代它。
describe("R115 internal wildcard stays a routing target", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let ready = false;

  async function request(path: string, body: Record<string, unknown>, key = PATTERN_KEY) {
    const response = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return {
      status: response.status,
      body: await response.json() as { choices?: { message: { content: string } }[] },
    };
  }

  beforeAll(async () => {
    const etcd = new EtcdClient();
    if (!(await etcd.ping())) return;
    upstream = await startOpenAiUpstream({ nonStreamBody: {
      id: "chatcmpl-r115",
      object: "chat.completion",
      created: 0,
      model: "r115-target",
      choices: [{ index: 0, message: { role: "assistant", content: "r115 served" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    } });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const providerKey = await seed.createProviderKey({
      display_name: "r115-pk", api_key: "sk-r115-upstream", api_base: `${upstream.baseUrl}/v1`,
    });
    const internal = await seed.createModel({
      display_name: INTERNAL_NAME, provider: "openai", model_name: "r115-target",
      provider_key_id: providerKey.id, internal: true,
    });
    await seed.createModel({
      display_name: PUBLIC_WILDCARD, provider: "openai", model_name: "*",
      provider_key_id: providerKey.id,
    });
    const publicModel = await seed.createModel({
      display_name: PUBLIC_NAME,
      routing: { strategy: "failover", targets: [{ model_id: internal.id }] },
    });
    // 所有 key 最后写入；最后一个 key 认证成功才说明此前资源都已传播。
    await seed.createApiKey({
      key_hash: createHash("sha256").update(PATTERN_KEY).digest("hex"), allowed_models: ["*"],
    });
    await seed.createApiKey({
      key_hash: createHash("sha256").update(ID_KEY).digest("hex"), allowed_model_ids: [publicModel.id],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${ID_KEY}` },
      });
      await response.arrayBuffer();
      return response.status === 200;
    });
    ready = true;
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("hides the internal pattern and refuses its exact name", async (ctx) => {
    if (!ready) { ctx.skip(); return; }
    const listed = await fetch(`${app!.proxyUrl}/v1/models`, {
      headers: { authorization: `Bearer ${PATTERN_KEY}` },
    });
    expect(listed.status).toBe(200);
    const names = (await listed.json() as { data: { id: string }[] }).data.map((model) => model.id);
    expect(names).toContain(PUBLIC_NAME);
    expect(names).not.toContain(PUBLIC_WILDCARD);
    expect(names).not.toContain(INTERNAL_NAME);
    const before = upstream!.receivedRequests.length;
    const exact = await request("/v1/chat/completions", {
      model: INTERNAL_NAME, messages: [{ role: "user", content: "hello" }],
    });
    expect(exact.status).toBe(404);
    expect(upstream!.receivedRequests.length - before).toBe(0);
  });

  for (const { path, body } of [
    { path: "/v1/chat/completions", body: { messages: [{ role: "user", content: "hello" }] } },
    { path: "/v1/messages", body: { max_tokens: 8, messages: [{ role: "user", content: "hello" }] } },
    { path: "/v1/messages/count_tokens", body: { messages: [{ role: "user", content: "hello" }] } },
    { path: "/v1/responses", body: { input: "hello" } },
    { path: "/v1/embeddings", body: { input: "hello" } },
  ]) {
    test(`refuses an internal wildcard alias on ${path} before dispatch`, async (ctx) => {
      if (!ready) { ctx.skip(); return; }
      const before = upstream!.receivedRequests.length;
      const response = await request(path, { model: INTERNAL_ALIAS, ...body });
      expect(response.status).toBe(404);
      expect(upstream!.receivedRequests.length - before).toBe(0);
    });
  }

  test("keeps public wildcard expansion and routing by internal target ID", async (ctx) => {
    if (!ready) { ctx.skip(); return; }
    const before = upstream!.receivedRequests.length;
    for (const [model, key] of [["r115-visible/alpha", PATTERN_KEY], [PUBLIC_NAME, ID_KEY]]) {
      const response = await request("/v1/chat/completions", {
        model, messages: [{ role: "user", content: "hello" }],
      }, key);
      expect(response.status).toBe(200);
      expect(response.body.choices?.[0]?.message.content).toBe("r115 served");
    }
    const requests = upstream!.receivedRequests.slice(before);
    expect(requests).toHaveLength(2);
    expect(requests.map((entry) => (JSON.parse(entry.body) as { model: string }).model))
      .toEqual(["alpha", "r115-target"]);
    const denied = await request("/v1/chat/completions", {
      model: "r115-visible/beta", messages: [{ role: "user", content: "hello" }],
    }, ID_KEY);
    expect(denied.status).toBe(403);
    expect(upstream!.receivedRequests.length - before).toBe(2);
  });
});
