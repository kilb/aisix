import { createHash, generateKeyPairSync } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { EtcdClient, ProxyClient, SeedClient, spawnApp, waitConfigPropagation, type SpawnedApp } from "../harness/index.js";

// E2E: a token endpoint that accepts the connection and never answers costs a
// request no more than the target's own timeout. The attempt fails, and a
// routing group moves on to its next target, rather than the caller waiting
// on an endpoint that will not answer.

const CALLER = "sk-stalled-token-mint";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const TARGET_TIMEOUT_MS = 1_500;
// Generous over the target's timeout and well under what a hung attempt takes.
const CALLER_PATIENCE_MS = 15_000;

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

describe("a token endpoint that never answers", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let stalled: Server | undefined;
  const mintPaths: string[] = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Reads the request and never writes a byte back.
    stalled = createServer((req) => {
      mintPaths.push(req.url ?? "");
      req.resume();
    });
    upstream = createServer((req, res) => {
      const parts: Buffer[] = [];
      req.on("data", (c: Buffer) => parts.push(c));
      req.on("end", () => chatAnswer(res, JSON.parse(Buffer.concat(parts).toString("utf8") || "{}")));
    });
    await Promise.all([
      new Promise<void>((resolve) => stalled!.listen(0, "127.0.0.1", resolve)),
      new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve)),
    ]);
    const portOf = (server: Server) => {
      const addr = server.address();
      if (addr === null || typeof addr === "string") throw new Error("no port");
      return addr.port;
    };
    const stalledBase = `http://127.0.0.1:${portOf(stalled)}`;
    const upstreamBase = `http://127.0.0.1:${portOf(upstream)}`;

    const { privateKey } = generateKeyPairSync("rsa", {
      modulusLength: 2048,
      publicKeyEncoding: { type: "spki", format: "pem" },
      privateKeyEncoding: { type: "pkcs8", format: "pem" },
    });

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const limits = { timeout: TARGET_TIMEOUT_MS, stream_timeout: TARGET_TIMEOUT_MS, retries: 0 };

    const vertexKey = await seed.createProviderKey({
      display_name: "stalled-mint-vertex-pk",
      provider: "google",
      adapter: "vertex",
      secret: JSON.stringify({
        project: "p",
        region: "us-central1",
        service_account_json: {
          type: "service_account",
          client_email: "sa@p.iam.gserviceaccount.com",
          private_key: privateKey,
          token_uri: `${stalledBase}/token`,
        },
      }),
      api_base: upstreamBase,
    });
    await seed.createModel({
      display_name: "stalled-mint-vertex",
      provider: "google",
      model_name: "gemini-2.5-flash",
      provider_key_id: vertexKey.id,
      ...limits,
    });

    const azureKey = await seed.createProviderKey({
      display_name: "stalled-mint-azure-pk",
      provider: "azure-openai",
      adapter: "azure-openai",
      secret: JSON.stringify({
        tenant_id: "tenant",
        client_id: "client",
        client_secret: "secret",
        authority_host: stalledBase,
      }),
      api_base: upstreamBase,
    });
    await seed.createModel({
      display_name: "stalled-mint-azure",
      provider: "azure-openai",
      model_name: "gpt4o-prod",
      provider_key_id: azureKey.id,
      ...limits,
    });

    const healthyKey = await seed.createProviderKey({
      display_name: "stalled-mint-healthy-pk",
      secret: "k",
      api_base: `${upstreamBase}/v1`,
    });
    await seed.createModel({
      display_name: "stalled-mint-healthy",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: healthyKey.id,
    });
    for (const rail of ["vertex", "azure"]) {
      await seed.createModel({
        display_name: `stalled-mint-${rail}-group`,
        routing: {
          strategy: "failover",
          targets: [{ model: `stalled-mint-${rail}` }, { model: "stalled-mint-healthy" }],
        },
      });
    }
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["*"] });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    stalled?.closeAllConnections();
    upstream?.closeAllConnections();
    await Promise.all(
      [stalled, upstream].map((s) => new Promise<void>((resolve) => (s ? s.close(() => resolve()) : resolve()))),
    );
  });

  async function post(route: string, body: unknown) {
    const res = await fetch(`${app!.proxyUrl}${route}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(CALLER_PATIENCE_MS),
    });
    return { status: res.status, raw: await res.text() };
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
    for (const [family, route, body] of families) {
      for (const stream of [false, true]) {
        test(
          `${rail} ${family}${stream ? " stream" : ""}: a group moves on to its next target`,
          async (ctx) => {
            if (!etcdReachable || !app) return ctx.skip();
            const out = await post(route, { ...body(`stalled-mint-${rail}-group`), ...(stream ? { stream } : {}) });
            expect(out.status, out.raw).toBe(200);
            expect(out.raw).toContain("served");
          },
          CALLER_PATIENCE_MS + 5_000,
        );
      }
    }

    test(
      `${rail}: with no other target the request fails, within the target's timeout`,
      async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const started = Date.now();
        const out = await post("/v1/chat/completions", {
          model: `stalled-mint-${rail}`,
          messages: [{ role: "user", content: "hi" }],
        });
        expect(out.status, out.raw).toBeGreaterThanOrEqual(500);
        expect(Date.now() - started).toBeLessThan(CALLER_PATIENCE_MS);
        // The stalled endpoint is what this rail really waited on: the secret
        // names the tenant on one rail and the whole token URL on the other.
        expect(mintPaths.some((path) => (rail === "azure" ? path.includes("/tenant/") : path === "/token"))).toBe(true);
      },
      CALLER_PATIENCE_MS + 5_000,
    );
  }
});
