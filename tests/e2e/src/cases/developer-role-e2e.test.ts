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

// E2E：`role: "developer"` 的消息被接受，并以上游认得的形式送达。
//
// `developer` 是 OpenAI 在 o1 及之后的模型上给系统指令起的名字，官方 SDK 与
// 新版文档都这么写（https://platform.openai.com/docs/api-reference/chat/create）。
// 网关把它整个请求 400 掉的话，按官方推荐写法发请求的调用方一个都用不了。
//
// 送达的形式按上游区分：
//   - OpenAI 本身：原样是 developer；
//   - 只认 system 的 OpenAI 兼容上游（DeepSeek 文档里只有 system/user/assistant/
//     tool）：作为 system 送达，指令一字不少；
//   - Anthropic：进顶层 `system` 字段，与 system 消息合在一起。

const CALLER = "sk-developer-role-e2e";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const INSTRUCTION = "Answer in French.";

const ANTHROPIC_REPLY = {
  id: "msg_dev_1",
  type: "message",
  role: "assistant",
  model: "claude-sonnet-4-5",
  content: [{ type: "text", text: "Bonjour" }],
  stop_reason: "end_turn",
  usage: { input_tokens: 5, output_tokens: 2 },
};

interface SentMessage {
  role: string;
  content: unknown;
}

describe("chat requests carrying a developer message", () => {
  let app: SpawnedApp | undefined;
  let openai: OpenAiUpstream | undefined;
  let deepseek: OpenAiUpstream | undefined;
  let anthropic: OpenAiUpstream | undefined;
  let etcdReachable = false;

  const chat = (model: string, stream = false) =>
    fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model,
        stream,
        messages: [
          { role: "developer", content: INSTRUCTION },
          { role: "system", content: "Be brief." },
          { role: "user", content: "Hello" },
        ],
      }),
    });

  const lastBody = (up: OpenAiUpstream) =>
    JSON.parse(up.receivedRequests.at(-1)?.body ?? "{}") as {
      messages?: SentMessage[];
      system?: unknown;
    };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    openai = await startOpenAiUpstream();
    deepseek = await startOpenAiUpstream();
    anthropic = await startOpenAiUpstream({ nonStreamBody: ANTHROPIC_REPLY });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const models: Array<[string, string, string, OpenAiUpstream, string, string]> = [
      ["dev-openai", "openai", "openai", openai, "/v1", "gpt-4o"],
      ["dev-deepseek", "deepseek", "openai", deepseek, "/v1", "deepseek-chat"],
      ["dev-anthropic", "anthropic", "anthropic", anthropic, "", "claude-sonnet-4-5"],
    ];
    for (const [name, provider, adapter, up, suffix, upstreamModel] of models) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        provider,
        adapter,
        secret: "sk-mock",
        api_base: `${up.baseUrl}${suffix}`,
      });
      await seed.createModel({
        display_name: name,
        provider,
        model_name: upstreamModel,
        provider_key_id: pk.id,
      });
    }
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: models.map(([name]) => name),
    });
    await waitConfigPropagation(async () => {
      for (const [name] of models) {
        const r = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
          method: "POST",
          headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
          body: JSON.stringify({ model: name, messages: [{ role: "user", content: "probe" }] }),
        });
        await r.text();
        if (r.status !== 200) return false;
      }
      return true;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await openai?.close();
    await deepseek?.close();
    await anthropic?.close();
  });

  test("OpenAI receives the developer message as sent", async (ctx) => {
    if (!etcdReachable || !app || !openai) return ctx.skip();
    const res = await chat("dev-openai");
    expect(res.status, await res.text()).toBe(200);
    expect(lastBody(openai).messages?.map((m) => [m.role, m.content])).toEqual([
      ["developer", INSTRUCTION],
      ["system", "Be brief."],
      ["user", "Hello"],
    ]);
  });

  test("OpenAI receives it as sent on a streamed request too", async (ctx) => {
    if (!etcdReachable || !app || !openai) return ctx.skip();
    const res = await chat("dev-openai", true);
    expect(res.status).toBe(200);
    await res.text();
    expect(lastBody(openai).messages?.[0]).toMatchObject({ role: "developer", content: INSTRUCTION });
  });

  test("an OpenAI-compatible upstream that documents only system gets it as system", async (ctx) => {
    if (!etcdReachable || !app || !deepseek) return ctx.skip();
    const res = await chat("dev-deepseek");
    expect(res.status, await res.text()).toBe(200);
    expect(lastBody(deepseek).messages?.map((m) => [m.role, m.content])).toEqual([
      ["system", INSTRUCTION],
      ["system", "Be brief."],
      ["user", "Hello"],
    ]);
  });

  test("Anthropic gets the instruction in its system prompt", async (ctx) => {
    if (!etcdReachable || !app || !anthropic) return ctx.skip();
    const res = await chat("dev-anthropic");
    expect(res.status, await res.text()).toBe(200);
    const sent = lastBody(anthropic);
    expect(JSON.stringify(sent.system)).toContain(INSTRUCTION);
    expect(JSON.stringify(sent.system)).toContain("Be brief.");
    expect(sent.messages?.map((m) => m.role)).toEqual(["user"]);
  });

  test("/v1/responses bridged to a system-only upstream sends developer input as system", async (ctx) => {
    if (!etcdReachable || !app || !deepseek) return ctx.skip();
    const res = await fetch(`${app.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({
        model: "dev-deepseek",
        input: [
          { role: "developer", content: INSTRUCTION },
          { role: "user", content: "Hello" },
        ],
      }),
    });
    expect(res.status, await res.text()).toBe(200);
    expect(lastBody(deepseek).messages?.map((m) => [m.role, m.content])).toEqual([
      ["system", INSTRUCTION],
      ["user", "Hello"],
    ]);
  });
});
