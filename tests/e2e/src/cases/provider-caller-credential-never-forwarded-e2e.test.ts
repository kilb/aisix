import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：调用方用来登录网关的那把密钥，绝不会到达任何模型供应商。
//
// 用户旅程：运维在 ProviderKey 的 forward_client_headers 里写了 authorization、
// x-api-key 这些名字（多半是抄配置时带上的）。调用方拿自己的网关密钥来调用。
// 如果这把密钥被转发出去，第三方供应商就拿到了一把能在本网关上重放的凭据，
// 同时供应商自己的密钥反而没发出去，请求还会失败。
//
// 所以每一家供应商、每一个端点，上游看到的都必须是 ProviderKey 自己的凭据，
// 任何请求头里都不能出现调用方的密钥。调用方专门给上游准备的另一把凭据不受影响：
// 它用 x-aisix-api-key 登录网关，authorization 留给上游。
//
// 上游只负责记录收到的请求头，这里的断言都看上游收到了什么，不看网关的回复：
// 泄露发生在请求发出去的那一刻，回复长什么样都改变不了它。

const CALLER = "sk-provider-cred-caller-PLAINTEXT";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const ALLOWLIST = ["authorization", "x-api-key", "api-key", "x-goog-api-key", "x-corp-*"];

interface Seen {
  method: string;
  path: string;
  headers: Record<string, string>;
  names: string[];
}

interface Recorder {
  baseUrl: string;
  seen: Seen[];
  close(): Promise<void>;
}

async function startRecorder(): Promise<Recorder> {
  const seen: Seen[] = [];
  const server: Server = createServer((req, res) => {
    req.resume();
    req.on("end", () => {
      seen.push({
        method: req.method ?? "GET",
        path: req.url ?? "/",
        headers: Object.fromEntries(
          Object.entries(req.headers).map(([k, v]) => [k, Array.isArray(v) ? v.join(",") : (v ?? "")]),
        ),
        names: req.rawHeaders.filter((_, i) => i % 2 === 0).map((n) => n.toLowerCase()),
      });
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      res.end("{}");
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    seen,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

/** 一家供应商：怎么建它的 ProviderKey，以及它自己的凭据应该出现在哪。 */
interface Provider {
  model: string;
  provider: string;
  modelName: string;
  pk: Record<string, unknown>;
  expectOwnCredential(headers: Record<string, string>): void;
}

const PROVIDERS: Provider[] = [
  {
    model: "cred-openai",
    provider: "openai",
    modelName: "gpt-4o",
    pk: { provider: "openai", adapter: "openai", secret: "sk-prov-openai" },
    expectOwnCredential: (h) => expect(h.authorization).toBe("Bearer sk-prov-openai"),
  },
  {
    model: "cred-anthropic",
    provider: "anthropic",
    modelName: "claude-sonnet-4-5",
    pk: { provider: "anthropic", adapter: "anthropic", secret: "sk-prov-anthropic" },
    expectOwnCredential: (h) => expect(h["x-api-key"]).toBe("sk-prov-anthropic"),
  },
  {
    model: "cred-azure",
    provider: "azure-openai",
    modelName: "gpt-4o-mini",
    pk: { provider: "azure-openai", adapter: "azure-openai", secret: "sk-prov-azure" },
    expectOwnCredential: (h) => expect(h["api-key"]).toBe("sk-prov-azure"),
  },
  {
    model: "cred-vertex",
    provider: "google",
    modelName: "gemini-2.0-flash",
    pk: {
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({ access_token: "ya29.prov-vertex", project: "proj-cred", region: "us-central1" }),
    },
    expectOwnCredential: (h) => expect(h.authorization).toBe("Bearer ya29.prov-vertex"),
  },
  {
    model: "cred-bedrock",
    provider: "bedrock",
    modelName: "amazon.nova-lite-v1:0",
    pk: {
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({ access_key_id: "AKIA-cred-e2e", secret_access_key: "sk-cred-e2e", region: "us-west-2" }),
    },
    expectOwnCredential: (h) => expect(h.authorization).toContain("AWS4-HMAC-SHA256"),
  },
];

/** 调用方的密钥可能出现的几种方式。 */
const ARRIVALS: Array<[string, Record<string, string>]> = [
  ["in authorization", { authorization: `Bearer ${CALLER}` }],
  ["in x-api-key", { "x-api-key": CALLER }],
  [
    "in x-aisix-api-key, copied into every other slot",
    {
      "x-aisix-api-key": CALLER,
      authorization: `Bearer ${CALLER}`,
      "x-api-key": CALLER,
      "api-key": CALLER,
      "x-goog-api-key": CALLER,
      "x-corp-copy": CALLER,
    },
  ],
];

const CHAT_BODY = { messages: [{ role: "user", content: "hi" }], max_tokens: 16 };

describe("the caller's gateway key never reaches a model provider", () => {
  let app: SpawnedApp | undefined;
  let upstream: Recorder | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startRecorder();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    for (const p of PROVIDERS) {
      const pk = await seed.createProviderKey({
        display_name: `${p.model}-pk`,
        api_base: upstream.baseUrl,
        request: { forward_client_headers: ALLOWLIST },
        ...p.pk,
      });
      await seed.createModel({
        display_name: p.model,
        provider: p.provider,
        model_name: p.modelName,
        provider_key_id: pk.id,
      });
    }

    // 最后建密钥：它能通过认证，就说明上面的资源都已经到位。
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["*"] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  /** 发一个请求，返回上游因它收到的所有请求。 */
  async function drive(path: string, init: RequestInit): Promise<Seen[]> {
    const baseline = upstream!.seen.length;
    const res = await fetch(`${app!.proxyUrl}${path}`, init);
    await res.text();
    return upstream!.seen.slice(baseline);
  }

  const postInit = (auth: Record<string, string>, body: unknown): RequestInit => ({
    method: "POST",
    headers: { "content-type": "application/json", "x-corp-session": "sess-1", ...auth },
    body: JSON.stringify(body),
  });

  function expectNoLeak(sent: Seen[], own: Provider, label: string): void {
    expect(sent.length, `${label}: the upstream was never called`).toBeGreaterThan(0);
    for (const req of sent) {
      for (const [name, value] of Object.entries(req.headers)) {
        expect(value, `${label}: caller key leaked in "${name}"`).not.toContain(CALLER);
      }
      own.expectOwnCredential(req.headers);
      // 只有一份凭据：上游不该在两个值之间挑。
      expect(req.names.filter((n) => n === "authorization").length, label).toBeLessThanOrEqual(1);
      expect(req.names.filter((n) => n === "x-api-key").length, label).toBeLessThanOrEqual(1);
      // 白名单确实在生效，否则上面「没有泄露」什么也说明不了。
      expect(req.headers["x-corp-session"], label).toBe("sess-1");
    }
  }

  describe.each(PROVIDERS.map((p) => [p.model, p] as const))("%s", (_, provider) => {
    test.for(ARRIVALS)("chat, with the key %s", async ([how, auth], ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      for (const stream of [false, true]) {
        const label = `${provider.model} stream=${stream} ${how}`;
        const sent = await drive(
          "/v1/chat/completions",
          postInit(auth, { model: provider.model, stream, ...CHAT_BODY }),
        );
        expectNoLeak(sent, provider, label);
      }
    });
  });

  // 同一把 ProviderKey 背后的其他端点，走的不是同一段代码。
  const openai = PROVIDERS[0]!;
  const anthropic = PROVIDERS[1]!;
  const JSON_ENDPOINTS: Array<[string, Provider, Record<string, unknown>]> = [
    ["/v1/messages", openai, { model: "cred-openai", ...CHAT_BODY }],
    ["/v1/messages", anthropic, { model: "cred-anthropic", ...CHAT_BODY }],
    ["/v1/messages/count_tokens", anthropic, { model: "cred-anthropic", messages: CHAT_BODY.messages }],
    ["/v1/responses", openai, { model: "cred-openai", input: "hi" }],
    ["/v1/responses", anthropic, { model: "cred-anthropic", input: "hi" }],
    ["/v1/completions", openai, { model: "cred-openai", prompt: "hi" }],
    ["/v1/embeddings", openai, { model: "cred-openai", input: "hi" }],
    ["/v1/rerank", openai, { model: "cred-openai", query: "q", documents: ["a"] }],
    ["/v1/audio/speech", openai, { model: "cred-openai", input: "hi", voice: "alloy" }],
    ["/v1/images/generations", openai, { model: "cred-openai", prompt: "a cat" }],
  ];

  test.for(JSON_ENDPOINTS)("%s (row %#)", async ([path, provider, body], ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const [how, auth] of ARRIVALS) {
      const sent = await drive(path, postInit(auth, body));
      expectNoLeak(sent, provider, `${path} ${provider.model} ${how}`);
    }
  });

  test("/v1/files", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const [how, auth] of ARRIVALS) {
      const sent = await drive("/v1/files", {
        headers: { "x-aisix-model": "cred-openai", "x-corp-session": "sess-1", ...auth },
      });
      expectNoLeak(sent, openai, `/v1/files ${how}`);
    }
  });

  test("a credential the caller sent for the upstream still takes the slot, alone", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();

    const viaAuthorization = await drive(
      "/v1/chat/completions",
      postInit(
        { "x-aisix-api-key": CALLER, authorization: "Bearer callers-upstream-credential" },
        { model: "cred-openai", ...CHAT_BODY },
      ),
    );
    expect(viaAuthorization).toHaveLength(1);
    expect(viaAuthorization[0]!.headers.authorization).toBe("Bearer callers-upstream-credential");
    expect(viaAuthorization[0]!.names.filter((n) => n === "authorization")).toHaveLength(1);

    const viaApiKey = await drive(
      "/v1/chat/completions",
      postInit(
        { authorization: `Bearer ${CALLER}`, "x-api-key": "callers-upstream-api-key" },
        { model: "cred-anthropic", ...CHAT_BODY },
      ),
    );
    expect(viaApiKey).toHaveLength(1);
    expect(viaApiKey[0]!.headers["x-api-key"]).toBe("callers-upstream-api-key");
    expect(viaApiKey[0]!.names.filter((n) => n === "x-api-key")).toHaveLength(1);
    expect(viaApiKey[0]!.headers.authorization).toBeUndefined();
  });
});
