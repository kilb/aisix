import { createHash } from "node:crypto";
import http from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-native-error-body-boundary";
const TIMEOUT_MS = 250;
const STALL_MS = 3_000;
const TAIL_SECRET = "ERROR_BODY_TAIL_MUST_NOT_SURFACE";

async function waitForModels(app: SpawnedApp, models: string[]): Promise<void> {
  await waitConfigPropagation(async () => {
    const response = await fetch(`${app.proxyUrl}/v1/models`, {
      headers: { authorization: `Bearer ${CALLER}` },
    });
    if (response.status === 401) {
      await response.text();
      return false;
    }
    if (response.status !== 200) {
      throw new Error(`model propagation probe returned ${response.status}`);
    }
    const body = (await response.json()) as { data?: Array<{ id?: string }> };
    const ids = new Set(body.data?.map((model) => model.id));
    return models.every((model) => ids.has(model));
  });
}

describe("native upstream error body boundaries", () => {
  let app: SpawnedApp | undefined;
  let upstream: http.Server | undefined;
  let baseUrl = "";
  let received = 0;
  let etcdReachable = false;
  const timers = new Set<NodeJS.Timeout>();

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = http.createServer((request, response) => {
      const chunks: Buffer[] = [];
      request.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
      request.on("end", () => {
        received += 1;
        const body = Buffer.concat(chunks).toString("utf8");
        response.writeHead(500, { "content-type": "application/json" });
        if (body.includes("STALL")) {
          response.write('{"error":{"message":"partial error body');
          const timer = setTimeout(() => {
            timers.delete(timer);
            response.end('"}}');
          }, STALL_MS);
          timers.add(timer);
          return;
        }
        response.end(
          JSON.stringify({
            error: { message: `prefix-${"x".repeat(2 * 1024 * 1024)}-${TAIL_SECRET}` },
          }),
        );
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const address = upstream.address();
    if (!address || typeof address === "string") throw new Error("upstream did not bind");
    baseUrl = `http://127.0.0.1:${address.port}`;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const openaiPk = await seed.createProviderKey({
      display_name: "error-body-openai-pk",
      secret: "sk-mock",
      api_base: `${baseUrl}/v1`,
    });
    const anthropicPk = await seed.createProviderKey({
      display_name: "error-body-anthropic-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-mock",
      api_base: `${baseUrl}/v1`,
    });
    const models = [
      ["error-body-messages-stall", "anthropic", anthropicPk.id],
      ["error-body-messages-huge", "anthropic", anthropicPk.id],
      ["error-body-responses-stall", "openai", openaiPk.id],
      ["error-body-responses-huge", "openai", openaiPk.id],
      ["error-body-count-stall", "anthropic", anthropicPk.id],
      ["error-body-count-huge", "anthropic", anthropicPk.id],
    ] as const;
    for (const [display_name, provider, provider_key_id] of models) {
      await seed.createModel({
        display_name,
        provider,
        model_name: provider === "anthropic" ? "claude-test" : "gpt-test",
        provider_key_id,
        timeout: TIMEOUT_MS,
        stream_timeout: TIMEOUT_MS,
        retries: 0,
        cooldown: { default_seconds: 60 },
      });
    }
    const names = models.map(([name]) => name);
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      allowed_models: names,
    });
    await waitForModels(app, names);
  }, 60_000);

  afterAll(async () => {
    for (const timer of timers) clearTimeout(timer);
    timers.clear();
    await app?.stop();
    if (upstream) {
      upstream.closeAllConnections();
      await new Promise<void>((resolve) => upstream!.close(() => resolve()));
    }
  });

  const call = async (kind: "messages" | "responses" | "count", marker: string) => {
    const model = `error-body-${kind === "count" ? "count" : kind}-${marker.toLowerCase()}`;
    const path =
      kind === "messages"
        ? "/v1/messages"
        : kind === "responses"
          ? "/v1/responses"
          : "/v1/messages/count_tokens";
    const body =
      kind === "responses"
        ? { model, input: marker, stream: true }
        : {
            model,
            max_tokens: 8,
            messages: [{ role: "user", content: marker }],
            ...(kind === "messages" ? { stream: true } : {}),
          };
    const started = Date.now();
    const response = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
    const text = await response.text();
    return { response, text, elapsedMs: Date.now() - started };
  };

  test("headers followed by a stalled error body remain deadline-bounded", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    for (const kind of ["messages", "responses", "count"] as const) {
      const baseline = received;
      const result = await call(kind, "STALL");
      expect(result.response.status, `${kind}: ${result.text}`).toBeGreaterThanOrEqual(500);
      expect(result.elapsedMs, kind).toBeLessThan(1_500);
      expect(received).toBe(baseline + 1);
    }
  });

  test("multi-megabyte error bodies are capped before parsing or surfacing", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    for (const kind of ["messages", "responses", "count"] as const) {
      const baseline = received;
      const result = await call(kind, "HUGE");
      expect(result.response.status, `${kind}: ${result.text}`).toBeGreaterThanOrEqual(500);
      expect(result.text.length).toBeLessThan(10_000);
      expect(result.text).not.toContain(TAIL_SECRET);
      expect(received).toBe(baseline + 1);
    }
  });
});
