import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient, SeedClient, spawnApp, waitConfigPropagation, type SpawnedApp,
} from "../harness/index.js";
import { harnessRequest } from "../harness/http.js";

const CREDENTIAL = "sk-r111-synthetic-route-key";
const TRACE_IDS = Array.from({ length: 16 }, (_, index) => `a${index.toString(16).padStart(31, "0")}`);
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

interface SafeReceiver {
  url: string;
  observed: boolean[];
  credentialSeen: boolean[];
  invalidPayload: boolean;
  close(): Promise<void>;
}

// 只保留脱敏布尔值；失败断言与超时也不打印 credential 或原始网关日志。
async function receiver(): Promise<SafeReceiver> {
  const state = {
    url: "", observed: TRACE_IDS.map(() => false), credentialSeen: TRACE_IDS.map(() => false),
    invalidPayload: false,
    close: async () => {},
  };
  const server: Server = createServer((request, response) => {
    let body = "";
    request.on("data", (chunk: Buffer) => { body += chunk.toString("utf8"); });
    request.on("end", () => {
      try {
        const payload = JSON.parse(body) as {
          resourceSpans?: { scopeSpans?: { spans?: { traceId?: string }[] }[] }[];
        };
        for (const resource of payload.resourceSpans ?? []) {
          for (const scope of resource.scopeSpans ?? []) {
            for (const span of scope.spans ?? []) {
              const index = TRACE_IDS.indexOf(span.traceId ?? "");
              if (index < 0) continue;
              state.observed[index] = true;
              state.credentialSeen[index] ||= JSON.stringify(span).includes(CREDENTIAL);
            }
          }
        }
      } catch {
        state.invalidPayload = true;
      }
      response.writeHead(200, { "content-type": "application/json" });
      response.end("{}");
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("receiver has no TCP address");
  state.url = `http://127.0.0.1:${address.port}/v1/traces`;
  state.close = () => new Promise<void>((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });
  return state;
}

describe("R111 request ID respects the selected passthrough credential slot", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let exporter: SafeReceiver | undefined;
  let ready = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    if (!(await etcd.ping())) return;
    upstream = createServer((request, response) => {
      request.resume();
      request.on("end", () => {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(JSON.stringify({
          id: "chatcmpl-r111", object: "chat.completion", created: 0, model: "r111-model",
          choices: [{ index: 0, message: { role: "assistant", content: "r111 served" }, finish_reason: "stop" }],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          upstream_request_id: request.headers["x-aisix-request-id"],
          credential_seen: JSON.stringify(request.headers).includes(CREDENTIAL),
        }));
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const address = upstream.address();
    if (address === null || typeof address === "string") throw new Error("upstream has no TCP address");
    const target = `http://127.0.0.1:${address.port}/v1`;
    exporter = await receiver();
    app = await spawnApp({
      logLevel: "info",
      requestId: { accept_headers: ["x-aisix-request-id", "x-r111-correlation"] },
      urlRewrites: [{ match: "^/r111-rewritten", rewrite: "/r111/downstream" }],
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const provider = await seed.createProviderKey({
      display_name: "r111-provider", api_key: "sk-r111-upstream-key", api_base: target,
    });
    await seed.createModel({
      display_name: "r111-model", provider: "openai", model_name: "r111-model", provider_key_id: provider.id,
    });
    const route = (name: string, slot: string, dimensions: Record<string, unknown>) =>
      seed.createPassthroughRoute({
        name, enabled: true, target_url: target, provider_key_id: provider.id,
        auth_mode: "header_key", auth_header_name: slot, ...dimensions,
      });
    await route("r111-downstream", "x-request-id", { path_prefix: "/r111/downstream" });
    await route("r111-accepted", "x-aisix-request-id", { path_prefix: "/r111/accepted" });
    await route("r111-custom", "x-r111-correlation", { path_prefix: "/r111/custom" });
    await route("r111-host", "x-r111-correlation", { hosts: ["r111-auth.example"] });
    await route("r111-host-and-path", "x-aisix-request-id", {
      hosts: ["r111-both.example"], path_prefix: "/r111/both",
    });
    await route("r111-shadowed", "x-r111-correlation", { path_prefix: "/v1" });
    await route("r111-shorter", "x-r111-correlation", { path_prefix: "/r111/ranked" });
    await route("r111-longer", "x-request-id", { path_prefix: "/r111/ranked/selected" });
    await route("r111-disabled", "x-r111-correlation", { path_prefix: "/r111/disabled", enabled: false });
    await seed.createObservabilityExporter({
      name: "r111-exporter", enabled: true, kind: "otlp_http", endpoint: exporter.url,
      request_headers: ["x-request-id", "x-aisix-request-id", "x-r111-correlation"],
    });
    // key 最后写入，用不触发被测路由行为的模型列表认证作为传播屏障。
    await seed.createApiKey({
      key_hash: createHash("sha256").update(CREDENTIAL).digest("hex"),
      allowed_models: ["*"], allowed_routes: ["*"],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CREDENTIAL}` },
      });
      await response.arrayBuffer();
      return response.status === 200;
    });
    ready = true;
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([
      exporter?.close(),
      upstream && new Promise<void>((resolve, reject) => upstream!.close((error) => error ? reject(error) : resolve())),
    ]);
  });

  const collisions = [
    { label: "default downstream header", path: "/r111/downstream/chat/completions", slot: "x-request-id" },
    { label: "default accepted header", path: "/r111/accepted/chat/completions", slot: "x-aisix-request-id" },
    { label: "custom accepted header", path: "/r111/custom/chat/completions", slot: "x-r111-correlation" },
    { label: "host route on a typed path", path: "/v1/chat/completions", slot: "x-r111-correlation", host: "r111-auth.example" },
    { label: "host and path route", path: "/r111/both/chat/completions", slot: "x-aisix-request-id", host: "r111-both.example" },
    { label: "rewritten path", path: "/r111-rewritten/chat/completions", slot: "x-request-id" },
  ];
  for (const [index, scenario] of collisions.entries()) {
    test(`never copies credential from ${scenario.label} into correlation`, async (ctx) => {
      if (!ready) { ctx.skip(); return; }
      const response = await harnessRequest(`${app!.proxyUrl}${scenario.path}`, {
        method: "POST",
        headers: {
          "content-type": "application/json", [scenario.slot]: CREDENTIAL,
          ...(scenario.host ? { host: scenario.host } : {}),
          traceparent: `00-${TRACE_IDS[index]}-bbbbbbbbbbbbbbbb-01`,
        },
        body: JSON.stringify({ model: "r111-model", messages: [{ role: "user", content: "hello" }] }),
      });
      const body = await response.body.text();
      const idHeader = response.headers["x-aisix-request-id"];
      const requestId = typeof idHeader === "string" ? idHeader : null;
      expect(response.statusCode).toBe(200);
      expect(requestId !== null && UUID.test(requestId)).toBe(true);
      expect(JSON.stringify(response.headers).includes(CREDENTIAL)).toBe(false);
      expect(body.includes(CREDENTIAL)).toBe(false);
      const observed = JSON.parse(body) as { upstream_request_id?: string; credential_seen?: boolean };
      expect(observed.upstream_request_id === requestId).toBe(true);
      expect(observed.credential_seen).toBe(false);
      await expect.poll(() => app!.output().split("\n").slice(0, -1).some((line) =>
        line.includes("proxy request completed") && line.includes(requestId!)),
      { timeout: 5_000 }).toBe(true);
      expect(app!.output().split("\n").slice(0, -1)
        .some((line) => line.includes(requestId!) && line.includes(CREDENTIAL))).toBe(false);
      await expect.poll(() => exporter!.observed[index], { timeout: 10_000 }).toBe(true);
      expect(exporter!.credentialSeen[index]).toBe(false);
      expect(exporter!.invalidPayload).toBe(false);
    });
  }

  const ordinary = [
    { label: "another path", path: "/unclaimed-r111-path", expectedStatus: 404, slot: "x-r111-correlation" },
    { label: "a different host", path: "/r111/both/chat/completions", expectedStatus: 404, slot: "x-aisix-request-id", host: "r111-unmatched.example" },
    { label: "a typed endpoint before a path-only route", path: "/v1/chat/completions", expectedStatus: 200, slot: "x-r111-correlation" },
    { label: "a typed method miss before a path-only route", path: "/v1/chat/completions", expectedStatus: 405, slot: "x-r111-correlation", method: "GET" },
    { label: "a disabled route", path: "/r111/disabled/chat/completions", expectedStatus: 404, slot: "x-r111-correlation" },
    { label: "the more specific selected route", path: "/r111/ranked/selected/chat/completions", expectedStatus: 200, slot: "x-r111-correlation", authSlot: "x-request-id" },
  ];
  for (const [offset, scenario] of ordinary.entries()) {
    test(`keeps a normal request ID on ${scenario.label}`, async (ctx) => {
      if (!ready) { ctx.skip(); return; }
      const requestId = `r111-ordinary-${offset}`;
      const response = await harnessRequest(`${app!.proxyUrl}${scenario.path}`, {
        method: scenario.method ?? "POST",
        headers: {
          authorization: `Bearer ${CREDENTIAL}`, "content-type": "application/json",
          [scenario.slot]: requestId,
          ...(scenario.host ? { host: scenario.host } : {}),
          ...(scenario.authSlot ? { [scenario.authSlot]: CREDENTIAL } : {}),
        },
        body: scenario.method ? undefined : JSON.stringify({ model: "r111-model", messages: [{ role: "user", content: "hello" }] }),
      });
      await response.body.arrayBuffer();
      expect(response.statusCode).toBe(scenario.expectedStatus);
      expect(response.headers["x-aisix-request-id"] === requestId).toBe(true);
    });
  }

  test("still records an ordinary downstream request ID", async (ctx) => {
    if (!ready) { ctx.skip(); return; }
    const marker = "r111-ordinary-downstream";
    const response = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CREDENTIAL}`, "content-type": "application/json", "x-request-id": marker,
      },
      body: JSON.stringify({ model: "r111-model", messages: [{ role: "user", content: "hello" }] }),
    });
    await response.arrayBuffer();
    expect(response.status).toBe(200);
    await expect.poll(() => app!.output().split("\n").slice(0, -1).some((line) =>
      line.includes("proxy request completed") && line.includes(`downstream_request_id=\"${marker}\"`)),
    { timeout: 5_000 }).toBe(true);
  });
});
