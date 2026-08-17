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

const CALLER = "sk-override-dlp-boundary";
const STRUCTURAL_SECRET = "sk-abcdefghijklmnopqrstuv";
const hash = (value: string) => createHash("sha256").update(value).digest("hex");

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

describe("request override and legacy completions DLP boundaries", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-clean",
        object: "text_completion",
        created: Math.floor(Date.now() / 1000),
        model: "gpt-3.5-turbo-instruct",
        choices: [{ index: 0, text: "clean completion", finish_reason: "stop" }],
        usage: { prompt_tokens: 2, completion_tokens: 2, total_tokens: 4 },
      },
    });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const unsafePk = await seed.createProviderKey({
      display_name: "unsafe-request-override-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
      request: { param_renames: { smuggle: "instructions" } },
    });
    const safePk = await seed.createProviderKey({
      display_name: "legacy-completions-dlp-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });

    const unsafeModels = [
      "override-chat",
      "override-completions",
      "override-responses",
      "override-messages",
      "override-embeddings",
      "override-images",
      "override-rerank",
      "override-speech",
    ];
    for (const display_name of unsafeModels) {
      await seed.createModel({
        display_name,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: unsafePk.id,
      });
    }
    await seed.createModel({
      display_name: "legacy-completions-dlp",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: safePk.id,
    });
    await seed.createGuardrail({
      name: "legacy-completions-input-dlp",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: "FORBIDDEN" }],
    });
    await seed.createGuardrail({
      name: "legacy-completions-structural-dlp",
      enabled: true,
      hook_point: "input",
      kind: "pii",
      detectors: [{ type: "api_key", action: "mask" }],
    });

    const models = [...unsafeModels, "legacy-completions-dlp"];
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: models });
    await waitForModels(app, models);
  }, 60_000);

  afterAll(async () => {
    await upstream?.close();
    await app?.stop();
  });

  const post = (path: string, body: unknown) =>
    fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });

  test("prompt-bearing request overrides are rejected on every JSON handler before egress", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const cases = [
      ["/v1/chat/completions", { model: "override-chat", messages: [{ role: "user", content: "clean" }], smuggle: "FORBIDDEN" }],
      ["/v1/completions", { model: "override-completions", prompt: "clean", smuggle: "FORBIDDEN" }],
      ["/v1/responses", { model: "override-responses", input: "clean", smuggle: "FORBIDDEN" }],
      ["/v1/messages", { model: "override-messages", max_tokens: 8, messages: [{ role: "user", content: "clean" }], smuggle: "FORBIDDEN" }],
      ["/v1/embeddings", { model: "override-embeddings", input: "clean", smuggle: "FORBIDDEN" }],
      ["/v1/images/generations", { model: "override-images", prompt: "clean", smuggle: "FORBIDDEN" }],
      ["/v1/rerank", { model: "override-rerank", query: "clean", documents: ["safe"], smuggle: "FORBIDDEN" }],
      ["/v1/audio/speech", { model: "override-speech", input: "clean", voice: "alloy", smuggle: "FORBIDDEN" }],
    ] as const;

    const baseline = upstream.receivedRequests.length;
    for (const [path, body] of cases) {
      const response = await post(path, body);
      const text = await response.text();
      expect(response.status, `${path}: ${text}`).toBe(400);
      expect(text).not.toContain("FORBIDDEN");
      expect(upstream.receivedRequests).toHaveLength(baseline);
    }
  });

  test("legacy completions blocks opaque token prompts and all prompt-bearing fields", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const blockedBodies = [
      { model: "legacy-completions-dlp", prompt: [1, 2, 3] },
      { model: "legacy-completions-dlp", prompt: [[1, 2], [3, 4]] },
      { model: "legacy-completions-dlp", prompt: "clean", suffix: "FORBIDDEN" },
      { model: "legacy-completions-dlp", prompt: "clean", user: STRUCTURAL_SECRET },
    ];
    const baseline = upstream.receivedRequests.length;
    for (const [index, body] of blockedBodies.entries()) {
      const response = await post("/v1/completions", body);
      const text = await response.text();
      expect(response.status, `case ${index}: ${text}`).toBe(422);
      expect(text).not.toContain("FORBIDDEN");
      expect(text).not.toContain(STRUCTURAL_SECRET);
      expect(upstream.receivedRequests).toHaveLength(baseline);
    }

    const clean = await post("/v1/completions", {
      model: "legacy-completions-dlp",
      prompt: "clean string prompt",
    });
    expect(clean.status, await clean.clone().text()).toBe(200);
    expect(upstream.receivedRequests).toHaveLength(baseline + 1);
    expect(upstream.receivedRequests.at(-1)!.path).toBe("/v1/completions");
    expect(upstream.receivedRequests.at(-1)!.body).toContain("clean string prompt");
  });
});
