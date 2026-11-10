import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startMockSls,
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
// prefix on every retry.

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
