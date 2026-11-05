import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  pickFreePort,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForLogLine,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：Azure 的 api_base 带 userinfo（把凭据嵌进 URL）时照配置使用，但调用方读到的
// 错误和网关日志里都不能出现密码。写不成 URL 的值才被拒绝，拒绝要说明是 api_base 的
// 问题，同样不回显凭据。规范写法指向 *.openai.azure.com，这里经本地正向代理到本地
// 上游，测试不会连真实主机。

const CALLER_PLAINTEXT = "sk-azure-redaction-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const SECRET = "sk-live-e2e-0d1f7a";
const ANSWER = "azure upstream answered";

describe("an Azure api_base with embedded credentials never shows the password", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-azure-redaction",
        object: "chat.completion",
        created: 1_700_000_000,
        model: "dep",
        choices: [{ index: 0, message: { role: "assistant", content: ANSWER }, finish_reason: "stop" }],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      },
    });
    const local = upstream.baseUrl.replace("http://", `http://user:${SECRET}@`);
    const closedPort = await pickFreePort();

    app = await spawnApp({
      logLevel: "info",
      // 只有规范写法的主机不在 NO_PROXY 里，所以只有它走这个代理。
      extraEnv: { HTTP_PROXY: upstream.baseUrl, http_proxy: upstream.baseUrl },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const [shape, apiBase] of [
      ["canonical", `http://user:${SECRET}@myres.openai.azure.com`],
      ["bare", `user:${SECRET}@myres`],
      ["override", local],
      ["unreachable", `http://user:${SECRET}@127.0.0.1:${closedPort}`],
    ] as const) {
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
        cooldown: { enabled: false },
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
    await upstream?.close();
  });

  const chat = (shape: string) =>
    fetch(`${app!.proxyUrl}/v1/chat/completions`, {
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

  /** 这个请求的访问日志行写出来之后，它之前写的日志都已经在输出里了。 */
  const expectLogsHidePassword = async (res: Response): Promise<void> => {
    const requestId = res.headers.get("x-aisix-request-id")!;
    await waitForLogLine(app!, (l) => l.includes(requestId), "the request's access log line");
    expect(app!.output(), "the gateway log shows the password").not.toContain(SECRET);
  };

  for (const shape of ["canonical", "override"]) {
    test(`${shape} api_base: accepted and used, the password never shown`, async (ctx) => {
      if (!etcdReachable || !app || !upstream) {
        ctx.skip();
        return;
      }
      const baseline = upstream.receivedRequests.length;
      const res = await chat(shape);
      const body = await res.text();
      expect(res.status, body).toBe(200);
      expect(body).toContain(ANSWER);
      expect(body).not.toContain(SECRET);
      const sent = upstream.receivedRequests.slice(baseline);
      expect(sent).toHaveLength(1);
      expect(sent[0]!.path).toContain("/openai/deployments/dep/chat/completions");
      if (shape === "canonical") {
        expect(sent[0]!.path).toContain("myres.openai.azure.com");
      }
      await expectLogsHidePassword(res);
    });
  }

  test("an api_base that cannot be reached: the error never shows the password", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await chat("unreachable");
    const body = await res.text();
    expect(res.status, body).toBe(502);
    expect(body).not.toContain(SECRET);
    await expectLogsHidePassword(res);
  });

  test("bare api_base: 400 names the field, never the credential", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await chat("bare");
    const body = await res.text();
    expect(res.status, body).toBe(400);
    expect(body, "the rejection quotes the credential back").not.toContain(SECRET);
    expect(body, "the rejection should say what is wrong").toContain("api_base");
    await expectLogsHidePassword(res);
  });
});
