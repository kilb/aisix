import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：Azure 的 api_base 写错（把凭据嵌进 URL）时，调用方收到的 400 必须说明
// 是 api_base 的问题，但绝不能把嵌在里面的凭据回显出来。运维的配置错误不该变成
// 任何一个调用方都读得到的凭据泄露。每种常见写法各走一遍。

const CALLER_PLAINTEXT = "sk-azure-redaction-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const SECRET = "sk-live-e2e-0d1f7a";

const SHAPES: Array<[string, string]> = [
  ["canonical", `https://user:${SECRET}@myres.openai.azure.com`],
  ["bare", `user:${SECRET}@myres`],
  ["override", `https://user:${SECRET}@azure.example`],
];

describe("an Azure api_base with embedded credentials never reaches the caller", () => {
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [shape, apiBase] of SHAPES) {
      const pk = await seed.createProviderKey({
        display_name: `azr-${shape}-pk`,
        provider: "azure-openai",
        adapter: "azure-openai",
        secret: "azure-key",
        api_base: apiBase,
      });
      await seed.createModel({
        display_name: `azr-${shape}`,
        provider: "azure-openai",
        model_name: "dep",
        provider_key_id: pk.id,
      });
    }
    // key 最后写：它能鉴权时，按修订号顺序前面的模型都已生效。
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
  });

  for (const [shape] of SHAPES) {
    test(`${shape} api_base: 400 names the field, never the credential`, async (ctx) => {
      if (!etcdReachable || !app) {
        ctx.skip();
        return;
      }
      const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${CALLER_PLAINTEXT}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model: `azr-${shape}`,
          messages: [{ role: "user", content: "hi" }],
        }),
      });
      const body = await res.text();
      expect(res.status, body).toBe(400);
      expect(body, "the rejection quotes the credential back").not.toContain(SECRET);
      expect(body, "the rejection should say what is wrong").toContain("api_base");
    });
  }
});
