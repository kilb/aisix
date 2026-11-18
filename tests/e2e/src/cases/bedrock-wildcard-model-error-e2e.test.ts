import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { EtcdClient, SeedClient, spawnApp, waitConfigPropagation, type SpawnedApp } from "../harness/index.js";

// AWS 官方两个 ValidationException 场景；只有真正由客户端选择的模型名归责于请求。
// https://repost.aws/knowledge-center/bedrock-invokemodel-api-error
const CALLER = "sk-bedrock-wildcard-errors";
const INVALID = "The provided model identifier is invalid.";
const PROFILE = "Invocation of model ID anthropic.profile with on-demand throughput isn't supported. Retry your request with the ID or ARN of an inference profile.";

describe("Bedrock wildcard model errors preserve caller responsibility and channel privacy", () => {
  let app: SpawnedApp | undefined;
  let appOff: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let reachable = false;
  const received: string[] = [];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    reachable = await etcd.ping();
    if (!reachable) return;
    upstream = createServer((request, response) => {
      request.resume();
      request.on("end", () => {
        received.push(request.url ?? "");
        response.writeHead(400, {"content-type": "application/json", "x-amzn-errortype": "ValidationException"});
        response.end(JSON.stringify({message: request.url?.includes("profile") ? PROFILE : INVALID}));
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const address = upstream.address();
    if (address === null || typeof address === "string") throw new Error("missing upstream port");
    app = await spawnApp({extraEnv: {
      AISIX_PROXY__PLATFORM__ENABLED: "true",
      AISIX_PROXY__PLATFORM__ID_KEY_ENV: "E2E_WILDCARD_PLATFORM_KEY",
      E2E_WILDCARD_PLATFORM_KEY: "42".repeat(32),
    }});
    appOff = await spawnApp();
    for (const activeApp of [app, appOff]) {
    const seed = new SeedClient(etcd, activeApp.etcdPrefix);
    const provider = await seed.createProviderKey({
      display_name: "bedrock-wildcard-errors", provider: "bedrock", adapter: "bedrock",
      secret: JSON.stringify({access_key_id: "AKIA-wildcard-test", secret_access_key: "wildcard-test-secret", region: "us-east-1"}),
      api_base: `http://127.0.0.1:${address.port}`,
    });
    for (const [display_name, model_name] of [
      ["caller/*", "anthropic.*"],
      ["fixed-invalid/*", "anthropic.fixed-invalid"],
      ["fixed-profile/*", "anthropic.fixed-profile"],
      ["exact-invalid", "anthropic.fixed-invalid"],
      ["exact-profile", "anthropic.fixed-profile"],
    ]) {
      await seed.createModel({display_name, model_name, provider: "bedrock", provider_key_id: provider.id, retries: 0});
    }
    await seed.createApiKey({key_hash: createHash("sha256").update(CALLER).digest("hex"), allowed_models: ["*"]});
    await waitConfigPropagation(async () => {
      const response = await fetch(`${activeApp.proxyUrl}/v1/models`, {headers: {authorization: `Bearer ${CALLER}`}});
      return response.status === 200;
    });
    }
  });

  afterAll(async () => {
    await app?.exit();
    await appOff?.exit();
    upstream?.closeAllConnections();
    await new Promise<void>((resolve) => upstream ? upstream.close(() => resolve()) : resolve());
  });

  for (const platform of [true, false]) {
  for (const endpoint of ["chat/completions", "messages", "responses", "messages/count_tokens"]) {
    for (const stream of endpoint.endsWith("count_tokens") ? [false] : [false, true]) {
      test(`${endpoint} stream=${stream} platform=${platform}: only captured model failures are closed 400s`, async (ctx) => {
        const activeApp = platform ? app : appOff;
        if (!reachable || !activeApp) return ctx.skip();
        for (const reason of ["invalid", "profile"]) {
          for (const [model, expected] of [
            [`caller/${reason}`, 400], [`fixed-${reason}/caller-suffix`, 503], [`exact-${reason}`, 503],
          ] as const) {
            const before = received.length;
            const body = endpoint.endsWith("count_tokens")
              ? {model, messages: [{role: "user", content: "hello"}]}
              : endpoint === "responses"
              ? {model, input: "hello", stream}
              : {model, messages: [{role: "user", content: "hello"}], max_tokens: 16, stream};
            const response = await fetch(`${activeApp.proxyUrl}/v1/${endpoint}`, {
              method: "POST", headers: {authorization: `Bearer ${CALLER}`, "content-type": "application/json"},
              body: JSON.stringify(body),
            });
            const text = await response.text();
            expect(response.status, `${model}: ${text}`).toBe(platform ? expected : 400);
            const error = JSON.parse(text).error as Record<string, unknown>;
            if (platform) {
            expect(error.type).toBe(expected === 400 ? "invalid_request_error" : endpoint.startsWith("messages") ? "overloaded_error" : "server_error");
            if (!endpoint.startsWith("messages")) expect(error.code).toBe(expected === 400 ? "invalid_request" : "upstream_unavailable");
            expect(text).not.toContain("ValidationException");
            expect(text).not.toContain("anthropic.");
            expect(text).not.toContain(reason === "invalid" ? INVALID : PROFILE);
            } else {
              expect(text).toContain(reason === "invalid" ? INVALID : PROFILE);
            }
            expect(received).toHaveLength(before + 1);
            expect(received.at(-1)).toContain(model.startsWith("caller/") ? `anthropic.${reason}` : `anthropic.fixed-${reason}`);
          }
        }
      });
    }
  }
  }
});
