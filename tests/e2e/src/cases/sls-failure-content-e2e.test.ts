import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  slsLogsFor,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// #1013: non-200 requests must also record the (post-mask)
// request body in full-content SLS logs — previously content was attached
// only on the 200 success path, so a 4xx/5xx row showed status + error
// class but never WHAT was sent, making triage guesswork. This drives a
// real `aisix` binary + etcd against a hard-failing mock upstream and a
// keyword guardrail, and reads the delivered SLS protobuf back:
//
//   1. upstream failure (chat)        → record carries `input_messages`
//   2. guardrail input block 422      → record carries `input_messages` with
//                                       the blocked keyword masked
//   3. 403 model-forbidden            → record carries `input_messages`
//   4. /v1/messages upstream failure  → record carries `input_messages`
//   5. unrewritable sensitive tool key → record exists WITHOUT `input_messages`
//   6. malformed /v1/messages DLP shape → record exists WITHOUT `input_messages`
//   7. /v1/responses upstream failure → record carries `input_messages`
//
// A blocked request's body is exported so an auditor can read what was refused,
// and what it was refused for stays out of the logstore: a blocking keyword or
// detector's match is masked like a mask rule's, the rest of the text is kept.
//
// A metadata_only exporter runs alongside and must never see any prompt.

const CALLER_PLAINTEXT = "sk-failure-content-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

const CREDENTIAL_REF = "mock";
const MOCK_AK_ID = "LTAI_mock_ak";
const MOCK_AK_SECRET = "mock_ak_secret";
const SLS_PROJECT = "aisix-e2e-obs";
const FULL_LOGSTORE = "failure-content-full";
const META_LOGSTORE = "failure-content-meta";

const FORBIDDEN_WORD = "failurecontentforbidden";
const UPSTREAM_FAIL_SENTINEL = "upstream-fail-prompt-7c1d2e";
const GUARDRAIL_CONTEXT = "plus context 4b9f0a";
const GUARDRAIL_SENTINEL = `${FORBIDDEN_WORD} ${GUARDRAIL_CONTEXT}`;
const RESPONSES_BLOCK_CONTEXT = "responses-block-context-8d2c7e";
const FORBIDDEN_MODEL_SENTINEL = "forbidden-model-prompt-9e3a1b";
const MESSAGES_SENTINEL = "messages-fail-prompt-5d8c4f";
const RESPONSES_SENTINEL = "responses-fail-prompt-2a6e9d";
const GROUP_SENTINEL = "group-all-failed-prompt-8f2b6c";
const RECOVER_SENTINEL = "group-recovered-prompt-3c7d1a";
const EMAIL = "dana@example.com";
const CN_ID = "11010519491231002X"; // valid ISO 7064 MOD 11-2 check digit
const MASKED_BLOCK_SENTINEL = "masked-block-prompt-6a4e8b";
const SENSITIVE_TOOL_KEY = "failure-key@example.com";
const RESPONSES_GUARDRAIL_REQUEST_ID = "11111111-1111-4111-8111-111111111111";
const EMBEDDINGS_GUARDRAIL_REQUEST_ID = "22222222-2222-4222-8222-222222222222";
const TOOL_KEY_GUARDRAIL_REQUEST_ID = "33333333-3333-4333-8333-333333333333";
const MALFORMED_MESSAGES_REQUEST_ID = "44444444-4444-4444-8444-444444444444";
const MALFORMED_MESSAGES_SECRET = "malformed-messages@example.com";
const KEYWORD_BLOCK_REQUEST_ID = "55555555-5555-4555-8555-555555555555";
const PII_BLOCK_REQUEST_ID = "66666666-6666-4666-8666-666666666666";

/** Decode every log delivered to `logstore` into flat key→value maps. */
function logsFor(sls: MockSls, logstore: string): Map<string, string>[] {
  return slsLogsFor(sls, logstore);
}

/** Poll until a log in `logstore` matching `pred` arrives (or time out). */
async function waitForLog(
  sls: MockSls,
  pred: (l: Map<string, string>) => boolean,
  what: string,
  logstore = FULL_LOGSTORE,
  timeoutMs = 10_000,
): Promise<Map<string, string>> {
  return waitForSlsLog(sls, logstore, pred, what, timeoutMs);
}

// -------------------------------------------------------------------------

describe("sls e2e: failed requests record the request body (#1013)", () => {
  let okUpstream: OpenAiUpstream | undefined;
  let failUpstream: OpenAiUpstream | undefined;
  let sls: MockSls | undefined;
  let app: SpawnedApp | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    sls = await startMockSls();
    // Healthy upstream — used only to gate config propagation.
    okUpstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-ok",
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: "gpt-4o-mini",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "fine" },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
      },
    });
    // Hard-failing upstream: every call returns 500.
    failUpstream = await startOpenAiUpstream({
      status: 500,
      errorBody: { error: { message: "mock upstream exploded", type: "server_error" } },
    });

    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: MOCK_AK_ID,
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: MOCK_AK_SECRET,
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    await seed.createObservabilityExporter({
      name: "sls-failure-full",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: FULL_LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });
    await seed.createObservabilityExporter({
      name: "sls-failure-meta",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: META_LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });

    const okPk = await seed.createProviderKey({
      display_name: "failure-content-ok-pk",
      secret: "sk-mock",
      api_base: `${okUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "failure-content-ok",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: okPk.id,
    });
    const failPk = await seed.createProviderKey({
      display_name: "failure-content-fail-pk",
      secret: "sk-mock",
      api_base: `${failUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "failure-content-fail",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: failPk.id,
      // Keep the failing target in rotation across tests — a cooldown
      // would silently shrink the routing groups below to one target.
      cooldown: { enabled: false },
    });
    // A model the caller is NOT allowed to use (403 case).
    await seed.createModel({
      display_name: "failure-content-offlimits",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: okPk.id,
    });
    // Second hard-failing target so a routing group can fail on BOTH targets.
    await seed.createModel({
      display_name: "failure-content-fail-b",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: failPk.id,
      cooldown: { enabled: false },
    });
    await seed.createModel({
      display_name: "failure-content-group",
      routing: {
        strategy: "failover",
        targets: [
          { model: "failure-content-fail" },
          { model: "failure-content-fail-b" },
        ],
      },
    });
    // Fail-then-recover group: both the failed attempt and the winner's
    // success event carry the prompt.
    await seed.createModel({
      display_name: "failure-content-recover",
      routing: {
        strategy: "failover",
        targets: [
          { model: "failure-content-fail" },
          { model: "failure-content-ok" },
        ],
      },
    });

    // Input-side keyword guardrail (block mode) for the 422 case.
    await seed.createGuardrail({
      name: "failure-content-guard",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN_WORD }],
    });
    // PII guardrail: email masks, china_id_card blocks. A blocked request
    // carrying BOTH must capture the post-mask body (the email placeholder,
    // never the raw address).
    await seed.createGuardrail({
      name: "failure-content-pii",
      enabled: true,
      hook_point: "input",
      kind: "pii",
      detectors: [
        { type: "email", action: "mask" },
        { type: "china_id_card", action: "block" },
      ],
    });

    // The caller key is the final revision, so authenticated model listing is
    // a barrier for every model, exporter, and guardrail written above without
    // exercising any behavior asserted by the test cases.
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: [
        "failure-content-ok",
        "failure-content-fail",
        "failure-content-group",
        "failure-content-recover",
      ],
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      if (response.status === 401) return false;
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((model) => model.id));
      return [
        "failure-content-ok",
        "failure-content-fail",
        "failure-content-group",
        "failure-content-recover",
      ].every((model) => ids.has(model));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await okUpstream?.close();
    await failUpstream?.close();
    await sls?.close();
  });

  async function chat(model: string, content: string, requestId?: string): Promise<Response> {
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
        ...(requestId ? { "x-aisix-request-id": requestId } : {}),
      },
      body: JSON.stringify({ model, messages: [{ role: "user", content }] }),
    });
    await res.text();
    return res;
  }

  test("upstream failure: the failed chat request's record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-fail", UPSTREAM_FAIL_SENTINEL);
    expect(res.status).toBeGreaterThanOrEqual(500);

    const log = await waitForLog(
      sls,
      (l) => (l.get("input_messages") ?? "").includes(UPSTREAM_FAIL_SENTINEL),
      "failed-upstream chat record with prompt",
    );
    // It is the FAILED request's record: non-2xx status, the upstream's answer.
    expect(Number(log.get("status_code"))).toBeGreaterThanOrEqual(400);
    expect(log.get("response")).toContain("mock upstream exploded");
    expect(log.has("output_messages")).toBe(false);
    // The request's messages, in the GenAI conventions' shape.
    const input = JSON.parse(log.get("input_messages")!) as Array<{
      role: string;
      parts: Array<{ type: string; content: string }>;
    }>;
    expect(input[0]!.role).toBe("user");
    expect(input[0]!.parts[0]!.content).toContain(UPSTREAM_FAIL_SENTINEL);
  });

  test("guardrail input block (422): the record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-ok", GUARDRAIL_SENTINEL);
    expect(res.status).toBe(422);

    const log = await waitForLog(
      sls,
      (l) => (l.get("input_messages") ?? "").includes(GUARDRAIL_CONTEXT),
      "guardrail-blocked record with prompt",
    );
    expect(log.get("status_code")).toBe("422");
    expect(log.get("guardrail_blocked")).toBe("true");
    expect([...log.values()].join("\n")).not.toContain(FORBIDDEN_WORD);
  });

  test("guardrail input block (422): the record masks the keyword and keeps the rest", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat(
      "failure-content-ok",
      GUARDRAIL_SENTINEL,
      KEYWORD_BLOCK_REQUEST_ID,
    );
    expect(res.status).toBe(422);

    const log = await waitForLog(
      sls,
      (l) => l.get("request_id") === KEYWORD_BLOCK_REQUEST_ID,
      "guardrail-blocked record",
    );
    expect(log.get("status_code")).toBe("422");
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(log.get("input_messages")).toContain(GUARDRAIL_CONTEXT);
    expect([...log.values()].join("\n")).not.toContain(FORBIDDEN_WORD);
  });

  test("/v1/responses exports blocked and applied guardrail metadata", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const response = await fetch(`${app.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
        "x-aisix-request-id": RESPONSES_GUARDRAIL_REQUEST_ID,
      },
      body: JSON.stringify({
        model: "failure-content-ok",
        input: `${RESPONSES_BLOCK_CONTEXT} ${FORBIDDEN_WORD}`,
      }),
    });
    await response.text();
    expect(response.status).toBe(422);

    const log = await waitForLog(
      sls,
      (entry) => entry.get("request_id") === RESPONSES_GUARDRAIL_REQUEST_ID,
      "blocked /v1/responses guardrail metadata",
    );
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(log.get("input_messages")).toContain(RESPONSES_BLOCK_CONTEXT);
    for (const value of log.values()) {
      expect(value).not.toContain(FORBIDDEN_WORD);
    }
    expect(JSON.parse(log.get("applied_guardrails") ?? "[]")).toContainEqual({
      kind: "keyword",
      hook: "input",
    });
  });

  test("/v1/embeddings exports blocked and applied guardrail metadata", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const response = await fetch(`${app.proxyUrl}/v1/embeddings`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
        "x-aisix-request-id": EMBEDDINGS_GUARDRAIL_REQUEST_ID,
      },
      body: JSON.stringify({
        model: "failure-content-ok",
        input: `embeddings ${FORBIDDEN_WORD}`,
      }),
    });
    await response.text();
    expect(response.status).toBe(422);

    const log = await waitForLog(
      sls,
      (entry) => entry.get("request_id") === EMBEDDINGS_GUARDRAIL_REQUEST_ID,
      "blocked /v1/embeddings guardrail metadata",
    );
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(JSON.parse(log.get("applied_guardrails") ?? "[]")).toContainEqual({
      kind: "keyword",
      hook: "input",
    });
  });

  test("403 model-forbidden: the record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-offlimits", FORBIDDEN_MODEL_SENTINEL);
    expect(res.status).toBe(403);

    const log = await waitForLog(
      sls,
      (l) =>
        l.get("status_code") === "403" &&
        (l.get("requested_model") ?? "") === "failure-content-offlimits",
      "403 record",
    );
    expect(log.get("input_messages")).toContain(FORBIDDEN_MODEL_SENTINEL);
  });

  test("/v1/messages upstream failure: the record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "failure-content-fail",
        max_tokens: 32,
        messages: [{ role: "user", content: MESSAGES_SENTINEL }],
      }),
    });
    await res.text();
    expect(res.status).toBeGreaterThanOrEqual(400);

    const log = await waitForLog(
      sls,
      (l) => (l.get("input_messages") ?? "").includes(MESSAGES_SENTINEL),
      "failed /v1/messages record with prompt",
    );
    expect(Number(log.get("status_code"))).toBeGreaterThanOrEqual(400);
  });

  test("/v1/messages prior block plus sensitive tool key never reaches upstream or logs", async (ctx) => {
    if (!etcdReachable || !app || !sls || !okUpstream) {
      ctx.skip();
      return;
    }
    const upstreamBaseline = okUpstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
        "x-aisix-request-id": TOOL_KEY_GUARDRAIL_REQUEST_ID,
      },
      body: JSON.stringify({
        model: "failure-content-ok",
        max_tokens: 32,
        messages: [
          {
            role: "user",
            content: `${FORBIDDEN_WORD} must block before structured masking`,
          },
          {
            role: "assistant",
            content: [
              {
                type: "tool_use",
                id: "toolu_failure_log",
                name: "lookup",
                input: { [SENSITIVE_TOOL_KEY]: "safe" },
              },
            ],
          },
        ],
      }),
    });
    const responseBody = await res.text();
    expect(res.status).toBe(422);
    expect(responseBody).not.toContain(SENSITIVE_TOOL_KEY);
    expect(okUpstream.receivedRequests).toHaveLength(upstreamBaseline);

    const log = await waitForLog(
      sls,
      (entry) => entry.get("request_id") === TOOL_KEY_GUARDRAIL_REQUEST_ID,
      "sensitive tool-key guardrail failure",
    );
    expect(log.get("status_code")).toBe("422");
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(log.get("prompt")).toBeUndefined();
    expect(log.get("input_messages")).toBeUndefined();
    for (const entry of logsFor(sls, FULL_LOGSTORE)) {
      for (const value of entry.values()) {
        expect(value).not.toContain(SENSITIVE_TOOL_KEY);
      }
    }
    const metadataLog = await waitForLog(
      sls,
      (entry) => entry.get("request_id") === TOOL_KEY_GUARDRAIL_REQUEST_ID,
      "metadata-only sensitive tool-key guardrail failure",
      META_LOGSTORE,
    );
    expect(metadataLog.get("status_code")).toBe("422");
    expect(metadataLog.get("guardrail_blocked")).toBe("true");
    expect(metadataLog.get("prompt")).toBeUndefined();
    expect(metadataLog.get("input_messages")).toBeUndefined();
    for (const value of metadataLog.values()) {
      expect(value).not.toContain(SENSITIVE_TOOL_KEY);
    }
  });

  test("malformed /v1/messages DLP shape fails closed without exporting its body", async (ctx) => {
    if (!etcdReachable || !app || !sls || !okUpstream) {
      ctx.skip();
      return;
    }
    const upstreamBaseline = okUpstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
        "x-aisix-request-id": MALFORMED_MESSAGES_REQUEST_ID,
      },
      body: JSON.stringify({
        model: "failure-content-ok",
        max_tokens: 32,
        messages: { hidden_text: MALFORMED_MESSAGES_SECRET },
      }),
    });
    const responseBody = await res.text();
    expect(res.status).toBe(422);
    expect(responseBody).not.toContain(MALFORMED_MESSAGES_SECRET);
    expect(okUpstream.receivedRequests).toHaveLength(upstreamBaseline);

    for (const logstore of [FULL_LOGSTORE, META_LOGSTORE]) {
      const log = await waitForLog(
        sls,
        (entry) => entry.get("request_id") === MALFORMED_MESSAGES_REQUEST_ID,
        `malformed Messages failure in ${logstore}`,
        logstore,
      );
      expect(log.get("status_code")).toBe("422");
      expect(log.get("prompt")).toBeUndefined();
      expect(log.get("input_messages")).toBeUndefined();
      for (const value of log.values()) {
        expect(value).not.toContain(MALFORMED_MESSAGES_SECRET);
      }
    }
  });

  test("/v1/responses upstream failure: the record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "failure-content-fail",
        input: RESPONSES_SENTINEL,
      }),
    });
    await res.text();
    expect(res.status).toBeGreaterThanOrEqual(400);

    const log = await waitForLog(
      sls,
      (l) => (l.get("input_messages") ?? "").includes(RESPONSES_SENTINEL),
      "failed /v1/responses record with prompt",
    );
    expect(Number(log.get("status_code"))).toBeGreaterThanOrEqual(400);
  });

  test("blocked request carrying maskable PII captures the POST-mask body", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    // The china_id_card detector blocks; the email detector masks. The
    // captured prompt must show what the success path would have sent —
    // the masked email — never the raw address.
    const res = await chat(
      "failure-content-ok",
      `${MASKED_BLOCK_SENTINEL} write to ${EMAIL} about id ${CN_ID}`,
    );
    expect(res.status).toBe(422);

    const log = await waitForLog(
      sls,
      (l) => (l.get("input_messages") ?? "").includes(MASKED_BLOCK_SENTINEL),
      "pii-blocked record with prompt",
    );
    expect(log.get("guardrail_blocked")).toBe("true");
    const prompt = log.get("input_messages")!;
    expect(prompt).toContain("[EMAIL_REDACTED]");
    expect(prompt).not.toContain(EMAIL);
    // And the raw address never reaches the logstore in any record.
    for (const l of logsFor(sls, FULL_LOGSTORE)) {
      for (const v of l.values()) {
        expect(v).not.toContain(EMAIL);
      }
    }
  });

  test("blocked request carrying PII exports the body with the rejected value masked", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat(
      "failure-content-ok",
      `${MASKED_BLOCK_SENTINEL} write to ${EMAIL} about id ${CN_ID}`,
      PII_BLOCK_REQUEST_ID,
    );
    expect(res.status).toBe(422);

    const log = await waitForLog(
      sls,
      (l) => l.get("request_id") === PII_BLOCK_REQUEST_ID,
      "pii-blocked record",
    );
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(log.get("input_messages")).toContain(MASKED_BLOCK_SENTINEL);
    for (const l of logsFor(sls, FULL_LOGSTORE)) {
      for (const v of l.values()) {
        expect(v).not.toContain(EMAIL);
        expect(v).not.toContain(CN_ID);
      }
    }
  });

  test("all targets failed: every attempt's record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-group", GROUP_SENTINEL);
    expect(res.status).toBeGreaterThanOrEqual(400);

    const withPrompt = await waitForLog(
      sls,
      (l) => (l.get("input_messages") ?? "").includes(GROUP_SENTINEL),
      "all-targets-failed record with prompt",
    );
    const requestId = withPrompt.get("request_id")!;
    expect(requestId).toBeTruthy();

    const deadline = Date.now() + 10_000;
    const requestLogs = () =>
      logsFor(sls!, FULL_LOGSTORE).filter((l) => l.get("request_id") === requestId);
    while (requestLogs().length < 2 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
    }
    expect(requestLogs().length).toBeGreaterThanOrEqual(2);
    for (const log of requestLogs()) {
      expect(log.get("input_messages")).toContain(GROUP_SENTINEL);
    }
  });

  test("fallback recovers: the failed attempt and the success record both carry the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-recover", RECOVER_SENTINEL);
    expect(res.status).toBe(200);

    const successLog = await waitForLog(
      sls,
      (l) =>
        (l.get("input_messages") ?? "").includes(RECOVER_SENTINEL) &&
        l.get("status_code") === "200",
      "recovered request's success record with prompt",
    );
    const requestId = successLog.get("request_id")!;
    const failedAttempt = await waitForLog(
      sls,
      (l) =>
        l.get("request_id") === requestId && Number(l.get("status_code") ?? "0") >= 400,
      "failed attempt record",
    );
    expect(failedAttempt.get("input_messages")).toContain(RECOVER_SENTINEL);
    expect(failedAttempt.get("response")).toContain("mock upstream exploded");
  });

  test("metadata_only exporter never receives any failed-request prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    // Runs last: every sentinel above has already been sent and captured
    // into the FULL logstore. None may appear in the metadata logstore.
    // Vacuous-pass guard: the meta pipeline must have delivered the failed
    // requests before we assert what its records lack.
    const failedMeta = () =>
      logsFor(sls!, META_LOGSTORE).filter(
        (l) => Number(l.get("status_code") ?? "0") >= 400,
      );
    const deadline = Date.now() + 10_000;
    while (failedMeta().length < 4 && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 100));
    }
    expect(failedMeta().length).toBeGreaterThanOrEqual(4);
    const metaText = logsFor(sls, META_LOGSTORE)
      .flatMap((l) => [...l.values()])
      .join(" ");
    for (const sentinel of [
      UPSTREAM_FAIL_SENTINEL,
      GUARDRAIL_SENTINEL,
      MESSAGES_SENTINEL,
      RESPONSES_SENTINEL,
      SENSITIVE_TOOL_KEY,
      MALFORMED_MESSAGES_SECRET,
    ]) {
      expect(metaText).not.toContain(sentinel);
    }
    for (const l of logsFor(sls, META_LOGSTORE)) {
      expect(l.get("prompt")).toBeUndefined();
      expect(l.get("input_messages")).toBeUndefined();
    }
  });
});
