import { createHash, generateKeyPairSync } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { EtcdClient, ProxyClient, SeedClient, spawnApp, waitConfigPropagation, type SpawnedApp } from "../harness/index.js";

// E2E: a credential token endpoint that throttles the gateway.
//
// Entra (Azure AD) and Google's OAuth endpoint answer a token request they
// throttle with `429 Too Many Requests` and `Retry-After`. That is the
// endpoint of ONE target's credentials refusing to mint: the model's own
// upstream was never sent the request, and the next target of a routing
// group mints with credentials of its own. So a group moves on to its next
// target, on every family, whatever `retry_on_429` says; a model with no
// other target answers the endpoint's `429` and its `Retry-After`; and a
// target with a cooldown configured is cooled down for the endpoint's
// `Retry-After`, so the next request does not ask the endpoint again.

const CALLER = "sk-throttled-token-mint";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const RETRY_AFTER = "30";

function chatAnswer(res: import("node:http").ServerResponse, body: Record<string, unknown>) {
  if (body.stream) {
    res.writeHead(200, { "content-type": "text/event-stream" });
    const frame = (delta: unknown, finish: string | null) =>
      `data: ${JSON.stringify({ id: "c", model: "m", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
    res.end(`${frame({ role: "assistant", content: "served" }, null)}${frame({}, "stop")}data: [DONE]\n\n`);
    return;
  }
  res.writeHead(200, { "content-type": "application/json" });
  res.end(
    JSON.stringify({
      id: "c",
      model: "m",
      choices: [{ index: 0, message: { role: "assistant", content: "served" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    }),
  );
}

describe("a token endpoint that throttles", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let throttled: Server | undefined;
  const mintPaths: string[] = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    throttled = createServer((req, res) => {
      mintPaths.push(req.url ?? "");
      req.resume();
      req.on("end", () => {
        res.writeHead(429, { "content-type": "application/json", "retry-after": RETRY_AFTER });
        res.end(JSON.stringify({ error: "temporarily_unavailable", error_description: "throttled" }));
      });
    });
    // The healthy target's OpenAI-shaped upstream: chat and embeddings.
    upstream = createServer((req, res) => {
      const parts: Buffer[] = [];
      req.on("data", (c: Buffer) => parts.push(c));
      req.on("end", () => {
        if (req.url?.endsWith("/embeddings")) {
          res.writeHead(200, { "content-type": "application/json" });
          res.end(
            JSON.stringify({
              object: "list",
              model: "m",
              data: [{ object: "embedding", index: 0, embedding: [0.5, 0.25] }],
              usage: { prompt_tokens: 1, total_tokens: 1 },
            }),
          );
          return;
        }
        chatAnswer(res, JSON.parse(Buffer.concat(parts).toString("utf8") || "{}"));
      });
    });
    await Promise.all([
      new Promise<void>((resolve) => throttled!.listen(0, "127.0.0.1", resolve)),
      new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve)),
    ]);
    const portOf = (server: Server) => {
      const addr = server.address();
      if (addr === null || typeof addr === "string") throw new Error("no port");
      return addr.port;
    };
    const throttledBase = `http://127.0.0.1:${portOf(throttled)}`;
    const upstreamBase = `http://127.0.0.1:${portOf(upstream)}`;

    const { privateKey } = generateKeyPairSync("rsa", {
      modulusLength: 2048,
      publicKeyEncoding: { type: "spki", format: "pem" },
      privateKeyEncoding: { type: "pkcs8", format: "pem" },
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const vertexKey = await seed.createProviderKey({
      display_name: "throttled-mint-vertex-pk",
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({
        project: "p",
        region: "us-central1",
        service_account_json: {
          type: "service_account",
          client_email: "sa@p.iam.gserviceaccount.com",
          private_key: privateKey,
          token_uri: `${throttledBase}/token`,
        },
      }),
      api_base: upstreamBase,
    });
    const azureKey = await seed.createProviderKey({
      display_name: "throttled-mint-azure-pk",
      provider: "azure-openai",
      adapter: "azure-openai",
      secret: JSON.stringify({
        tenant_id: "tenant",
        client_id: "client",
        client_secret: "secret",
        authority_host: throttledBase,
      }),
      api_base: upstreamBase,
    });
    const rails = {
      vertex: { key: vertexKey.id, provider: "google", chat: "gemini-2.5-flash", embed: "text-embedding-005" },
      azure: { key: azureKey.id, provider: "azure-openai", chat: "gpt4o-prod", embed: "embed-prod" },
    } as const;
    for (const [rail, r] of Object.entries(rails)) {
      for (const [kind, model_name] of [
        ["chat", r.chat],
        ["embed", r.embed],
      ] as const) {
        await seed.createModel({
          display_name: `throttled-mint-${rail}-${kind}`,
          provider: r.provider,
          model_name,
          provider_key_id: r.key,
          retries: 0,
        });
      }
      // The same chat target, with a cooldown that honours Retry-After.
      await seed.createModel({
        display_name: `throttled-mint-${rail}-cooled`,
        provider: r.provider,
        model_name: r.chat,
        provider_key_id: r.key,
        retries: 0,
        cooldown: { enabled: true, default_seconds: 5, max_seconds: 120 },
      });
    }

    const healthyKey = await seed.createProviderKey({
      display_name: "throttled-mint-healthy-pk",
      secret: "k",
      api_base: `${upstreamBase}/v1`,
    });
    for (const [kind, model_name] of [
      ["chat", "gpt-4o-mini"],
      ["embed", "text-embedding-3-small"],
    ] as const) {
      await seed.createModel({
        display_name: `throttled-mint-healthy-${kind}`,
        provider: "openai",
        model_name,
        provider_key_id: healthyKey.id,
      });
    }
    for (const rail of Object.keys(rails)) {
      for (const kind of ["chat", "embed", "cooled"]) {
        await seed.createModel({
          display_name: `throttled-mint-${rail}-${kind}-group`,
          routing: {
            strategy: "failover",
            targets: [
              { model: `throttled-mint-${rail}-${kind}` },
              { model: `throttled-mint-healthy-${kind === "embed" ? "embed" : "chat"}` },
            ],
          },
        });
      }
    }
    // Seeded last, so this key authenticating implies the whole seed set
    // has reached the gateway's snapshot.
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["*"] });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    throttled?.closeAllConnections();
    upstream?.closeAllConnections();
    await Promise.all(
      [throttled, upstream].map((s) => new Promise<void>((resolve) => (s ? s.close(() => resolve()) : resolve()))),
    );
  });

  async function post(route: string, body: unknown) {
    const res = await fetch(`${app!.proxyUrl}${route}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, retryAfter: res.headers.get("retry-after"), raw: await res.text() };
  }

  const families = [
    ["chat", "/v1/chat/completions", (model: string) => ({ model, messages: [{ role: "user", content: "hi" }] })],
    [
      "messages",
      "/v1/messages",
      (model: string) => ({ model, max_tokens: 8, messages: [{ role: "user", content: "hi" }] }),
    ],
    ["responses", "/v1/responses", (model: string) => ({ model, input: "hi" })],
  ] as const;

  for (const rail of ["vertex", "azure"]) {
    /** Whether the throttled endpoint is the one this rail mints from. */
    const isRailMint = (path: string) => (rail === "azure" ? path.includes("/tenant/") : path === "/token");

    for (const [family, route, body] of families) {
      for (const stream of [false, true]) {
        test(`${rail} ${family}${stream ? " stream" : ""}: a group moves on to its next target`, async (ctx) => {
          if (!etcdReachable || !app) return ctx.skip();
          const minted = mintPaths.length;
          const out = await post(route, { ...body(`throttled-mint-${rail}-chat-group`), ...(stream ? { stream } : {}) });
          expect(out.status, out.raw).toBe(200);
          expect(out.raw).toContain("served");
          // The first target really was tried, and its endpoint refused.
          expect(mintPaths.slice(minted).some(isRailMint)).toBe(true);
        });
      }
    }

    test(`${rail} embeddings: a group moves on to its next target`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const minted = mintPaths.length;
      const out = await post("/v1/embeddings", { model: `throttled-mint-${rail}-embed-group`, input: "hi" });
      expect(out.status, out.raw).toBe(200);
      expect(JSON.parse(out.raw).data[0].embedding).toEqual([0.5, 0.25]);
      expect(mintPaths.slice(minted).some(isRailMint)).toBe(true);
    });

    test(`${rail}: with no other target the caller gets the endpoint's 429 and Retry-After`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const out = await post("/v1/chat/completions", {
        model: `throttled-mint-${rail}-chat`,
        messages: [{ role: "user", content: "hi" }],
      });
      expect(out.status, out.raw).toBe(429);
      expect(out.retryAfter).toBe(RETRY_AFTER);
    });

    test(`${rail}: the endpoint's Retry-After cools the target down`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const group = `throttled-mint-${rail}-cooled-group`;
      const message = { messages: [{ role: "user", content: "hi" }] };
      const first = mintPaths.length;
      const served = await post("/v1/chat/completions", { model: group, ...message });
      expect(served.status, served.raw).toBe(200);
      expect(mintPaths.slice(first).filter(isRailMint)).toHaveLength(1);

      // Cooled down for the endpoint's 30s: the next request goes straight to
      // the healthy target, without asking the endpoint again.
      const second = mintPaths.length;
      const again = await post("/v1/chat/completions", { model: group, ...message });
      expect(again.status, again.raw).toBe(200);
      expect(mintPaths.slice(second).filter(isRailMint)).toHaveLength(0);
    });
  }
});
