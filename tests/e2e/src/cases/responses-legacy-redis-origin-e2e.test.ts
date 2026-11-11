import { createHash, randomUUID } from "node:crypto";
import { connect } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  etcdEndpoint,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";
import { redisReady } from "../harness/redis.js";

const REDIS_URL = process.env.AISIX_E2E_REDIS ?? "redis://127.0.0.1:6379";
const ENV_ID = `legacy-response-${randomUUID()}`;
const ETCD_PREFIX = `/aisix-e2e-${randomUUID()}`;
const CALLER = `sk-legacy-response-${randomUUID()}`;
const MODEL = "legacy-response-route";
const PLATFORM_SECRET_ENV = "E2E_LEGACY_RESPONSE_ORIGIN_SECRET";
const PLATFORM_SECRET = "3f9a1c7e5b2d4086a1e3c5f7092b4d6e8f0a1b2c3d4e5f60718293a4b5c6d7e8";

// 只向随机环境的 Redis namespace 发命令，不清理共享数据库。
async function redisInteger(parts: string[]): Promise<number> {
  const url = new URL(REDIS_URL);
  return new Promise((resolve, reject) => {
    const socket = connect({ host: url.hostname, port: Number(url.port || 6379) });
    let reply = "";
    socket.once("connect", () => {
      socket.write(
        `*${parts.length}\r\n` +
          parts.map((part) => `$${Buffer.byteLength(part)}\r\n${part}\r\n`).join(""),
      );
    });
    socket.on("data", (chunk) => {
      reply += chunk.toString();
      if (!reply.includes("\r\n")) return;
      socket.destroy();
      const matched = /^:(-?\d+)\r\n$/.exec(reply);
      if (matched) resolve(Number(matched[1]));
      else reject(new Error(`Unexpected Redis integer reply: ${reply.slice(0, 200)}`));
    });
    socket.once("error", reject);
    socket.setTimeout(5000, () => {
      socket.destroy();
      reject(new Error("Redis fixture command timed out"));
    });
  });
}

// 把真正写入 Redis 的条目改成升级前 payload：保留原 key、body 和 TTL，只移除 origin。
// 以响应字节定位本次条目，避免误改 readiness probe 或其他场景。
async function cacheEntry(body: string, action: "legacy" | "expire" | "count"): Promise<number> {
  return redisInteger([
    "EVAL",
    `local n = 0
     for _, key in ipairs(redis.call('KEYS', ARGV[1])) do
       local value = cjson.decode(redis.call('GET', key))
       if value.body == ARGV[2] then
         if ARGV[3] == 'legacy' then
           value.origin_model_id = nil
           redis.call('SET', key, cjson.encode(value), 'KEEPTTL')
         elseif ARGV[3] == 'expire' then
           redis.call('PEXPIRE', key, 1)
         end
         n = n + 1
       end
     end
     return n`,
    "0",
    `aisix:cache:${ENV_ID}:body:*`,
    Buffer.from(body).toString("base64"),
    action,
  ]);
}

const responseBody = (id: string) => ({
  id,
  object: "response",
  created_at: 1,
  status: "completed",
  model: "gpt-4o-mini",
  output: [{
    type: "message", id: "msg_legacy_origin", role: "assistant", status: "completed",
    content: [{ type: "output_text", text: "ok", annotations: [] }],
  }],
  usage: { input_tokens: 3, output_tokens: 1, total_tokens: 4 },
});

describe("Responses: a warm Redis entry without origin is refreshed after upgrade", () => {
  let ready = false;
  let app: SpawnedApp | undefined;
  let alpha: OpenAiUpstream | undefined;
  let beta: OpenAiUpstream | undefined;
  let seed: SeedClient;
  let alphaPk: { id: string; value: Record<string, unknown> };

  const start = () => spawnApp({
    etcdPrefix: ETCD_PREFIX,
    extra: {
      etcd: { endpoints: [etcdEndpoint()], prefix: ETCD_PREFIX, env_id: ENV_ID },
      cache: { backend: "memory", redis: { url: REDIS_URL } },
    },
    extraEnv: {
      AISIX_PROXY__PLATFORM__ENABLED: "true",
      AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
      [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
    },
  });

  const respond = async (body: Record<string, unknown>) => {
    const response = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify({ model: MODEL, ...body }),
    });
    const text = await response.text();
    return { status: response.status, text, json: () => JSON.parse(text) as { id: string } };
  };

  const waitLive = async () => {
    const client = new ProxyClient(app!.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await client.listModels()).status === 200);
  };

  beforeAll(async () => {
    ready = (await new EtcdClient().ping()) && (await redisReady(REDIS_URL, "any"));
    if (!ready) return;
    alpha = await startOpenAiUpstream({
      scriptedResponses: Array.from({ length: 8 }, (_, i) => ({
        pathBodies: { "/v1/responses": responseBody(`resp_legacy_alpha_${i}`) },
      })),
    });
    beta = await startOpenAiUpstream({ pathBodies: { "/v1/responses": responseBody("resp_legacy_beta") } });
    app = await start();
    seed = new SeedClient(new EtcdClient(), `${ETCD_PREFIX}/${ENV_ID}`);
    for (const [name, upstream, group] of [
      ["legacy-alpha", alpha, "legacy-alpha-group"],
      ["legacy-beta", beta, "legacy-beta-group"],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`, secret: "sk-local-peer",
        api_base: `${upstream.baseUrl}/v1`, groups: [group],
      });
      if (name === "legacy-alpha") alphaPk = pk;
      await seed.createModel({
        display_name: name, provider: "openai", model_name: "gpt-4o-mini",
        provider_key_id: pk.id, retries: 0,
      });
    }
    await seed.createModel({
      display_name: MODEL,
      routing: { strategy: "failover", targets: [{ model: "legacy-alpha" }, { model: "legacy-beta" }] },
    });
    await seed.createCachePolicy({
      name: "legacy-response-cache", enabled: true, backend: "redis",
      ttl_seconds: 300, applies_to: `model:${MODEL}`, scope: "team",
    });
    // 最后写 caller；认证通过证明前面的全部 seed 已进入同一快照。
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      team_id: randomUUID(), allowed_models: [MODEL],
      allowed_provider_groups: ["legacy-alpha-group", "legacy-beta-group"],
    });
    await waitLive();
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await alpha?.close();
    await beta?.close();
  });

  test("an unknown legacy origin misses even while its former target is eligible; the rewritten entry hits", async (ctx) => {
    if (!ready) ctx.skip();
    const input = `legacy eligible ${randomUUID()}`;
    const before = alpha!.receivedRequests.length;
    const warm = await respond({ input });
    expect(warm.status, warm.text).toBe(200);
    expect(alpha!.receivedRequests.length - before).toBe(1);
    const hit = await respond({ input });
    expect(hit.status, hit.text).toBe(200);
    expect(alpha!.receivedRequests.length - before, "current-format entry must hit").toBe(1);
    expect(await cacheEntry(warm.text, "legacy"), "one real Redis payload was made legacy").toBe(1);

    const refreshed = await respond({ input });
    expect(refreshed.status, refreshed.text).toBe(200);
    expect(alpha!.receivedRequests.length - before, "legacy origin cannot be proven").toBe(2);
    const newHit = await respond({ input });
    expect(newHit.status, newHit.text).toBe(200);
    expect(alpha!.receivedRequests.length - before, "refreshed origin is known and eligible").toBe(2);

    // TTL 对照：真正到期的条目继续走普通 miss，新写条目仍能正常命中。
    expect(await cacheEntry(refreshed.text, "expire")).toBe(1);
    await waitConfigPropagation(async () => (await cacheEntry(refreshed.text, "count")) === 0);
    const afterExpiry = await respond({ input });
    expect(afterExpiry.status, afterExpiry.text).toBe(200);
    expect(alpha!.receivedRequests.length - before).toBe(3);
    const afterExpiryHit = await respond({ input });
    expect(afterExpiryHit.status, afterExpiryHit.text).toBe(200);
    expect(alpha!.receivedRequests.length - before).toBe(3);
  });

  test("warm legacy Redis survives restart and group change, then refreshes to a continuable answer", async (ctx) => {
    if (!ready) ctx.skip();
    const input = `legacy moved ${randomUUID()}`;
    const a0 = alpha!.receivedRequests.length;
    const warm = await respond({ input });
    expect(warm.status, warm.text).toBe(200);
    expect(alpha!.receivedRequests.length - a0).toBe(1);
    expect(await cacheEntry(warm.text, "legacy"), "exactly the warmed response was made legacy").toBe(1);

    // 不清理 Redis 或 etcd；继任进程读取旧 payload 和改组后的真实资源。
    await app!.stop();
    await seed.update("provider_keys", alphaPk.id, { ...alphaPk.value, groups: ["legacy-other"] });
    app = await start();
    await waitLive();
    const b0 = beta!.receivedRequests.length;
    const refreshed = await respond({ input });
    expect(refreshed.status, refreshed.text).toBe(200);
    expect(beta!.receivedRequests.length - b0, "legacy cached answer must not be replayed").toBe(1);
    expect(refreshed.json().id).not.toBe(warm.json().id);
    expect(alpha!.receivedRequests.length - a0).toBe(1);

    const hit = await respond({ input });
    expect(hit.status, hit.text).toBe(200);
    expect(hit.json().id).toBe(refreshed.json().id);
    expect(beta!.receivedRequests.length - b0, "known origin remains a normal hit").toBe(1);
    const continuation = await respond({ input: "continue", previous_response_id: refreshed.json().id });
    expect(continuation.status, continuation.text).toBe(200);
    expect(beta!.receivedRequests.length - b0).toBe(2);

    const denied = await respond({ input: "old target", previous_response_id: warm.json().id });
    expect(denied.status, denied.text).toBe(400);
    expect(beta!.receivedRequests.length - b0).toBe(2);
    expect(alpha!.receivedRequests.length - a0).toBe(1);
  });
});
