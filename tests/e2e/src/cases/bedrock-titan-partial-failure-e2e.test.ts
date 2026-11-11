import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  awaitWindowHeadroom,
  EtcdClient,
  SeedClient,
  scrapeMetrics,
  spawnApp,
  startMockSls,
  sumMetric,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an embeddings request for a Titan model on Bedrock, whose second
// text fails after the first was answered.
//
// Titan embeds one text per InvokeModel call
// (https://docs.aws.amazon.com/bedrock/latest/userguide/model-parameters-titan-embed-text.html),
// so the first text was served, and billed by AWS, before the second
// failed. The request's usage row bills the input the first call reported,
// and the gateway does not send the answered text again: the failure ends
// the request even with retries configured, rather than replaying the
// prefix on every retry. A caller that hangs up while a later text is still
// in flight leaves a row that bills the texts already answered, as the
// failure would have. A routing group's member is charged them against its
// own token limit.

const CALLER_PLAINTEXT = "sk-bedrock-titan-partial";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

const CREDENTIAL_REF = "mock";
const SLS_PROJECT = "aisix-e2e-obs";
const LOGSTORE = "titan-partial-events";

describe("Bedrock Titan: a later text fails after an earlier one was served", () => {
  let app: SpawnedApp | undefined;
  let bedrock: Server | undefined;
  let sls: MockSls | undefined;
  const received: string[] = [];
  // Answers to "hangs", held open until the suite ends.
  const held: import("node:http").ServerResponse[] = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Every text but "fails" is answered with 4 input tokens; "fails" is
    // throttled, which a retry budget would normally spend itself on.
    bedrock = createServer((req, res) => {
      let raw = "";
      req.on("data", (c) => (raw += c));
      req.on("end", () => {
        const text = JSON.parse(raw || "{}").inputText as string;
        received.push(text);
        if (text === "hangs") {
          held.push(res);
          return;
        }
        if (text === "fails") {
          res.writeHead(503, {
            "content-type": "application/json",
            "x-amzn-errortype": "ServiceUnavailableException",
          });
          res.end(JSON.stringify({ message: "Service unavailable." }));
          return;
        }
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ embedding: [0.25, 0.5], inputTextTokenCount: 4 }));
      });
    });
    await new Promise<void>((resolve) => bedrock!.listen(0, "127.0.0.1", resolve));
    const addr = bedrock.address();
    if (addr === null || typeof addr === "string") throw new Error("no port");

    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-titan-partial",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });
    const pk = await seed.createProviderKey({
      display_name: "titan-partial-pk",
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({
        access_key_id: "AKIA-titan-partial",
        secret_access_key: "sk-titan-partial",
        region: "us-east-1",
      }),
      api_base: `http://127.0.0.1:${addr.port}`,
    });
    await seed.createModel({
      display_name: "titan-partial",
      provider: "bedrock",
      model_name: "amazon.titan-embed-text-v1",
      provider_key_id: pk.id,
      retries: 2,
    });
    await seed.createModel({
      display_name: "titan-cancel",
      provider: "bedrock",
      model_name: "amazon.titan-embed-text-v1",
      provider_key_id: pk.id,
      cost: { input_per_1k: 1, output_per_1k: 1 },
    });
    // A group member with a token budget of its own: two answered texts
    // spend all of it.
    await seed.createModel({
      display_name: "titan-member",
      provider: "bedrock",
      model_name: "amazon.titan-embed-text-v1",
      provider_key_id: pk.id,
      rate_limit: { tpm: 8 },
    });
    await seed.createModel({
      display_name: "titan-member-group",
      routing: { strategy: "failover", targets: [{ model: "titan-member" }] },
    });
    // R-109: a direct model and a group member, each capped at the eight
    // tokens two answered texts cost, so a cancellation that charges them
    // leaves the counter at its cap.
    await seed.createModel({
      display_name: "titan-quota-direct",
      provider: "bedrock",
      model_name: "amazon.titan-embed-text-v1",
      provider_key_id: pk.id,
      rate_limit: { tpm: 8, concurrency: 1 },
    });
    await seed.createModel({
      display_name: "titan-quota-member",
      provider: "bedrock",
      model_name: "amazon.titan-embed-text-v1",
      provider_key_id: pk.id,
      rate_limit: { tpm: 8, concurrency: 1 },
    });
    await seed.createModel({
      display_name: "titan-quota-group",
      routing: { strategy: "failover", targets: [{ model: "titan-quota-member" }] },
    });
    // Seeded last, so this key authenticating implies the whole seed set
    // has reached the gateway's snapshot.
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      await res.text();
      return res.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    for (const res of held) res.destroy();
    await sls?.close();
    await new Promise<void>((resolve) => (bedrock ? bedrock.close(() => resolve()) : resolve()));
  });

  test("the served text is billed and not sent again", async (ctx) => {
    if (!etcdReachable || !app || !sls) return ctx.skip();
    const before = received.length;
    const res = await fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify({ model: "titan-partial", input: ["served", "fails"] }),
    });
    const text = await res.text();
    expect(res.status, text).toBeGreaterThanOrEqual(500);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    expect(requestId).not.toBe("");

    // One call per text, once each.
    expect(received.slice(before)).toEqual(["served", "fails"]);

    const row = await waitForSlsLog(
      sls,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      `the usage row of ${requestId}`,
    );
    expect(Number(row.get("status_code"))).toBe(res.status);
    expect(row.get("prompt_tokens")).toBe("4");
  });

  test("a caller that hangs up mid-array is billed the texts already served", async (ctx) => {
    if (!etcdReachable || !app || !sls) return ctx.skip();
    const before = received.length;
    const inputTokens = async () =>
      sumMetric(await scrapeMetrics(app!.metricsUrl), "aisix_llm_input_tokens_total", {
        endpoint: "/v1/embeddings",
        model: "titan-cancel",
      });
    const tokensBefore = await inputTokens();
    const controller = new AbortController();
    const abandoned = fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify({ model: "titan-cancel", input: ["served", "served", "hangs"] }),
      signal: controller.signal,
    }).then((res) => res.text());
    for (let i = 0; i < 400 && !received.slice(before).includes("hangs"); i++) {
      await new Promise((r) => setTimeout(r, 25));
    }
    // Two texts answered, the third is in flight when the caller leaves.
    expect(received.slice(before)).toEqual(["served", "served", "hangs"]);
    controller.abort();
    await expect(abandoned).rejects.toThrow();

    const row = await waitForSlsLog(
      sls,
      LOGSTORE,
      (log) => log.get("requested_model") === "titan-cancel" && log.get("status_code") === "499",
      "the 499 usage row of the abandoned request",
    );
    expect(row.get("error_class")).toBe("client_disconnected");
    // AWS billed the two answered texts, 4 input tokens each.
    expect(row.get("prompt_tokens")).toBe("8");
    expect(Number(row.get("cost_usd") ?? "0")).toBeGreaterThan(0);
    // R-112: and the tokens the row bills reach the Prometheus counters too —
    // the handler's own emit, which would have written them, never ran.
    await waitConfigPropagation(async () => (await inputTokens()) === tokensBefore + 8);
    // One row, one charge: the cancel path runs once per request.
    await new Promise((r) => setTimeout(r, 300));
    expect(await inputTokens()).toBe(tokensBefore + 8);
  });

  // R-112, the negative half: a request cancelled before any upstream call
  // answered served nothing, so it must add no tokens to the counters.
  test("a cancellation before anything was served adds no tokens", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const inputTokens = async () =>
      sumMetric(await scrapeMetrics(app!.metricsUrl), "aisix_llm_input_tokens_total", {
        endpoint: "/v1/embeddings",
        model: "titan-cancel",
      });
    const before = await inputTokens();
    const controller = new AbortController();
    const abandoned = fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify({ model: "titan-cancel", input: ["hangs"] }),
      signal: controller.signal,
    }).then((res) => res.text());
    await new Promise((r) => setTimeout(r, 300));
    controller.abort();
    await expect(abandoned).rejects.toThrow();
    await waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("status_code") === "499" && log.get("prompt_tokens") === "0",
      "the 499 usage row of the request that served nothing",
    );
    expect(await inputTokens()).toBe(before);
  }, 30_000);

  // R-109: the tokens an abandoned request's answered texts were billed are
  // charged to the token limits the request held them against, so a key that
  // spent a whole window before its caller left does not get a second one.
  test.for<"titan-quota-direct" | "titan-quota-group">([
    "titan-quota-direct",
    "titan-quota-group",
  ])("%s: a cancellation charges what its answered texts were served", { timeout: 60_000 }, async (model, ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    await awaitWindowHeadroom();
    const before = received.length;
    const controller = new AbortController();
    const abandoned = fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify({ model, input: ["served", "served", "hangs"] }),
      signal: controller.signal,
    }).then((res) => res.text());
    for (let i = 0; i < 400 && !received.slice(before).includes("hangs"); i++) {
      await new Promise((r) => setTimeout(r, 25));
    }
    expect(received.slice(before)).toEqual(["served", "served", "hangs"]);
    controller.abort();
    await expect(abandoned).rejects.toThrow();

    // The cancellation has settled once its 499 row is written; asking before
    // that would race the charge.
    await waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("requested_model") === model && log.get("status_code") === "499",
      `the 499 usage row of the abandoned ${model} request`,
    );
    // The two answered texts spent the window's whole eight tokens, so ONE
    // follow-up request must be refused rather than served. Deliberately not
    // retried: a served request spends tokens of its own, and a retry loop
    // would exhaust the window itself and pass on the broken code.
    const next = await fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ model, input: ["served"] }),
    });
    const text = await next.text();
    expect(next.status, text).toBe(429);
  });

  test("a group member's own token limit is charged the texts it served before failing", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const embed = (input: string[]) =>
      fetch(`${app!.proxyUrl}/v1/embeddings`, {
        method: "POST",
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
        body: JSON.stringify({ model: "titan-member-group", input }),
      });
    await awaitWindowHeadroom();
    const failed = await embed(["served", "served", "fails"]);
    await failed.text();
    expect(failed.status).toBeGreaterThanOrEqual(500);
    // The member was served 8 tokens, its whole minute: the next request
    // is refused by its limit rather than served.
    const next = await embed(["served"]);
    const text = await next.text();
    expect(next.status, text).toBe(429);
  });

  test("a failure at the first text bills nothing and keeps its retries", async (ctx) => {
    if (!etcdReachable || !app || !sls) return ctx.skip();
    const before = received.length;
    const res = await fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify({ model: "titan-partial", input: ["fails", "served"] }),
    });
    await res.text();
    expect(res.status).toBeGreaterThanOrEqual(500);
    // Nothing was served, so the configured retries run: one call per try.
    expect(received.slice(before)).toEqual(["fails", "fails", "fails"]);
  });
});
