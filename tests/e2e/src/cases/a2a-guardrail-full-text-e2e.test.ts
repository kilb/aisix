import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startA2aUpstream,
  waitConfigPropagation,
  type A2aUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：A2A 调用的内容护栏看的是**整段**文字，而不是前面一截。
//
// 网关把请求原样转给 agent、把 agent 的回答原样还给调用方。护栏要是只审其中
// 前 1 MiB，在被拦的词前面垫上 1 MiB 的填充就能把它带过去 —— 送出去、收回来
// 的都是完整内容。两个方向都要守住：调用方发给 agent 的，与 agent 答回来的。
//
// 对照：同样长度、不含被拦词的内容照常往来，说明拦下的确实是那个词，而不是
// 「太长」。

const KEY = "sk-a2a-guardrail-full-text";
const MARKER = "a2afulltextforbidden";
// 超过 1 MiB 的填充，被拦词放在它后面。
const PADDING = "lorem ipsum ".repeat(Math.ceil((1.1 * 1024 * 1024) / 12));

const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

const sendBody = (text: string) =>
  JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "message/send",
    params: { message: { role: "user", parts: [{ kind: "text", text }], messageId: randomUUID() } },
  });

describe("A2A content guardrails read the whole text", () => {
  let app: SpawnedApp | undefined;
  let plainAgent: A2aUpstream | undefined;
  let longRiskyAgent: A2aUpstream | undefined;
  let longCleanAgent: A2aUpstream | undefined;
  let etcdReachable = false;

  const call = async (agent: string, text: string) => {
    const res = await fetch(`${app!.proxyUrl}/a2a/${agent}`, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
      body: sendBody(text),
    });
    return { status: res.status, text: await res.text() };
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    plainAgent = await startA2aUpstream();
    longRiskyAgent = await startA2aUpstream({ unaryAnswer: `${PADDING} ${MARKER}` });
    longCleanAgent = await startA2aUpstream({ unaryAnswer: `${PADDING} all clear` });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [name, up] of [
      ["plain", plainAgent],
      ["long-risky", longRiskyAgent],
      ["long-clean", longCleanAgent],
    ] as const) {
      await seed.update("a2a_agents", randomUUID(), {
        name,
        url: up.url,
        protocol_version: "0.3",
        auth_type: "none",
        enabled: true,
      });
    }
    await seed.createGuardrail({
      name: "a2a-full-text-keyword",
      enabled: true,
      hook_point: "both",
      kind: "keyword",
      patterns: [{ kind: "literal", value: MARKER }],
    });
    await seed.createApiKey({ key_hash: sha256(KEY), allowed_models: [], allowed_agents: ["*"] });
    // 就绪：短请求里的被拦词已经被拦下。
    await waitConfigPropagation(async () => (await call("plain", `say ${MARKER}`)).status === 422);
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await plainAgent?.close();
    await longRiskyAgent?.close();
    await longCleanAgent?.close();
  });

  test("a forbidden word after 1 MiB of request text is blocked before the agent sees it", async (ctx) => {
    if (!etcdReachable || !app || !plainAgent) return ctx.skip();
    const before = plainAgent.requests.filter((r) => r.httpMethod === "POST").length;
    const res = await call("plain", `${PADDING} ${MARKER}`);
    expect(res.status).toBe(422);
    expect(plainAgent.requests.filter((r) => r.httpMethod === "POST").length).toBe(before);
  });

  test("the same amount of clean request text goes through", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await call("plain", `${PADDING} all clear`);
    expect(res.status, res.text.slice(0, 200)).toBe(200);
  });

  test("a forbidden word after 1 MiB of the agent's answer never reaches the caller", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await call("long-risky", "write me the long report");
    expect(res.status).toBe(422);
    expect(res.text).not.toContain(MARKER);
  });

  test("an equally long clean answer is returned in full", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await call("long-clean", "write me the long report");
    expect(res.status).toBe(200);
    expect(res.text).toContain("all clear");
  });
});
