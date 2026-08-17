import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  lz4DecompressBlock,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// AISIX-Cloud#1013: non-200 requests must also record the (post-mask)
// request body in full-content SLS logs — previously content was attached
// only on the 200 success path, so a 4xx/5xx row showed status + error
// class but never WHAT was sent, making triage guesswork. This drives a
// real `aisix` binary + etcd against a hard-failing mock upstream and a
// keyword guardrail, and reads the delivered SLS protobuf back:
//
//   1. upstream failure (chat)        → record carries `prompt`
//   2. guardrail input block 422      → record exists WITHOUT `prompt`
//   3. 403 model-forbidden            → record exists WITHOUT `prompt`
//      (auth-class failures stay body-less by design)
//   4. /v1/messages upstream failure  → record carries `prompt`
//   5. unrewritable sensitive tool key → record exists WITHOUT `prompt`
//   6. malformed /v1/messages DLP shape → record exists WITHOUT `prompt`
//   7. /v1/responses upstream failure → record carries `prompt`
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
const GUARDRAIL_SENTINEL = `${FORBIDDEN_WORD} plus context 4b9f0a`;
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

// --- Minimal SLS LogGroup protobuf reader (see sink/sls.rs encoder) -----
// LogGroup { Logs = 1 (message) { Time = 1 (varint), Contents = 2 (message)
// { Key = 1 (string), Value = 2 (string) } } }; unknown fields skipped.

function readVarint(buf: Buffer, pos: number): [number, number] {
  let result = 0;
  let shift = 0;
  for (;;) {
    const b = buf[pos]!;
    pos += 1;
    result += (b & 0x7f) * 2 ** shift;
    if ((b & 0x80) === 0) return [result, pos];
    shift += 7;
  }
}

function skipField(buf: Buffer, pos: number, wireType: number): number {
  if (wireType === 0) return readVarint(buf, pos)[1];
  if (wireType === 2) {
    const [len, p] = readVarint(buf, pos);
    return p + len;
  }
  if (wireType === 5) return pos + 4;
  if (wireType === 1) return pos + 8;
  throw new Error(`unsupported wire type ${wireType}`);
}

function parseContentPair(buf: Buffer): [string, string] {
  let pos = 0;
  let key = "";
  let value = "";
  while (pos < buf.length) {
    const [tag, p] = readVarint(buf, pos);
    pos = p;
    const field = tag >>> 3;
    const wireType = tag & 7;
    if (wireType === 2) {
      const [len, q] = readVarint(buf, pos);
      const bytes = buf.subarray(q, q + len);
      pos = q + len;
      if (field === 1) key = bytes.toString("utf8");
      else if (field === 2) value = bytes.toString("utf8");
    } else {
      pos = skipField(buf, pos, wireType);
    }
  }
  return [key, value];
}

function parseLog(buf: Buffer): Map<string, string> {
  const out = new Map<string, string>();
  let pos = 0;
  while (pos < buf.length) {
    const [tag, p] = readVarint(buf, pos);
    pos = p;
    const field = tag >>> 3;
    const wireType = tag & 7;
    if (field === 2 && wireType === 2) {
      const [len, q] = readVarint(buf, pos);
      const [k, v] = parseContentPair(buf.subarray(q, q + len));
      out.set(k, v);
      pos = q + len;
    } else {
      pos = skipField(buf, pos, wireType);
    }
  }
  return out;
}

/** Decode every log delivered to `logstore` into flat key→value maps. */
function logsFor(sls: MockSls, logstore: string): Map<string, string>[] {
  const logs: Map<string, string>[] = [];
  for (const r of sls.requests) {
    if (r.logstore !== logstore || r.rawSize === 0 || r.body.length === 0) continue;
    const group = lz4DecompressBlock(r.body, r.rawSize);
    let pos = 0;
    while (pos < group.length) {
      const [tag, p] = readVarint(group, pos);
      pos = p;
      const field = tag >>> 3;
      const wireType = tag & 7;
      if (field === 1 && wireType === 2) {
        const [len, q] = readVarint(group, pos);
        logs.push(parseLog(group.subarray(q, q + len)));
        pos = q + len;
      } else {
        pos = skipField(group, pos, wireType);
      }
    }
  }
  return logs;
}

/** Poll until a log in `logstore` matching `pred` arrives (or time out). */
async function waitForLog(
  sls: MockSls,
  pred: (l: Map<string, string>) => boolean,
  what: string,
  logstore = FULL_LOGSTORE,
  timeoutMs = 10_000,
): Promise<Map<string, string>> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = logsFor(sls, logstore).find(pred);
    if (hit) return hit;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`no SLS log matching: ${what}`);
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
    // Second hard-failing target so a routing group can fail on BOTH
    // targets (content must ride the LAST attempt only).
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
    // Fail-then-recover group: the failed attempt must stay content-less
    // while the winner's success event carries the prompt.
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
      (l) => (l.get("prompt") ?? "").includes(UPSTREAM_FAIL_SENTINEL),
      "failed-upstream chat record with prompt",
    );
    // It is the FAILED request's record: non-2xx status, no response text.
    expect(Number(log.get("status_code"))).toBeGreaterThanOrEqual(400);
    expect(log.get("response") ?? "").toBe("");
    // The prompt is the request body — valid JSON with the messages array.
    const prompt = JSON.parse(log.get("prompt")!) as {
      messages: Array<{ content: string }>;
    };
    expect(prompt.messages[0]!.content).toContain(UPSTREAM_FAIL_SENTINEL);
  });

  test("guardrail input block (422): the record suppresses the prompt", async (ctx) => {
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
      "guardrail-blocked record without prompt",
    );
    expect(log.get("status_code")).toBe("422");
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(log.has("prompt")).toBe(false);
    expect([...log.values()].join("\n")).not.toContain(GUARDRAIL_SENTINEL);
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
        input: `responses ${FORBIDDEN_WORD}`,
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
    expect(log.get("prompt")).toBeUndefined();
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

  test("403 model-forbidden: the record exists but stays body-less", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-offlimits", FORBIDDEN_MODEL_SENTINEL);
    expect(res.status).toBe(403);

    // The 403 event lands in SLS…
    const log = await waitForLog(
      sls,
      (l) =>
        l.get("status_code") === "403" &&
        (l.get("requested_model") ?? "") === "failure-content-offlimits",
      "403 record",
    );
    // …but carries no prompt, and the sentinel never reaches the logstore.
    expect(log.get("prompt")).toBeUndefined();
    for (const l of logsFor(sls, FULL_LOGSTORE)) {
      expect(l.get("prompt") ?? "").not.toContain(FORBIDDEN_MODEL_SENTINEL);
    }
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
      (l) => (l.get("prompt") ?? "").includes(MESSAGES_SENTINEL),
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
    expect(res.status).toBe(400);
    expect(responseBody).not.toContain(MALFORMED_MESSAGES_SECRET);
    expect(okUpstream.receivedRequests).toHaveLength(upstreamBaseline);

    for (const logstore of [FULL_LOGSTORE, META_LOGSTORE]) {
      const log = await waitForLog(
        sls,
        (entry) => entry.get("request_id") === MALFORMED_MESSAGES_REQUEST_ID,
        `malformed Messages failure in ${logstore}`,
        logstore,
      );
      expect(log.get("status_code")).toBe("400");
      expect(log.get("prompt")).toBeUndefined();
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
      (l) => (l.get("prompt") ?? "").includes(RESPONSES_SENTINEL),
      "failed /v1/responses record with prompt",
    );
    expect(Number(log.get("status_code"))).toBeGreaterThanOrEqual(400);
  });

  test("blocked request carrying PII never exports the rejected body", async (ctx) => {
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
      "pii-blocked record without prompt",
    );
    expect(log.get("guardrail_blocked")).toBe("true");
    expect(log.has("prompt")).toBe(false);
    for (const l of logsFor(sls, FULL_LOGSTORE)) {
      for (const v of l.values()) {
        expect(v).not.toContain(EMAIL);
        expect(v).not.toContain(CN_ID);
      }
    }
  });

  test("all targets failed: exactly one record carries the prompt, on the last attempt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-group", GROUP_SENTINEL);
    expect(res.status).toBeGreaterThanOrEqual(400);

    const withPrompt = await waitForLog(
      sls,
      (l) => (l.get("prompt") ?? "").includes(GROUP_SENTINEL),
      "all-targets-failed record with prompt",
    );
    const requestId = withPrompt.get("request_id")!;
    expect(requestId).toBeTruthy();

    // Both attempts produced a record…
    const requestLogs = logsFor(sls, FULL_LOGSTORE).filter(
      (l) => l.get("request_id") === requestId,
    );
    expect(requestLogs.length).toBeGreaterThanOrEqual(2);
    // …but exactly ONE carries the prompt, and it is the LAST attempt
    // (the failure the caller actually saw), not the first.
    const promptBearers = requestLogs.filter((l) =>
      (l.get("prompt") ?? "").includes(GROUP_SENTINEL),
    );
    expect(promptBearers.length).toBe(1);
    const maxAttempt = Math.max(
      ...requestLogs.map((l) => Number(l.get("attempt_index") ?? "0")),
    );
    expect(Number(promptBearers[0]!.get("attempt_index"))).toBe(maxAttempt);
    expect(maxAttempt).toBeGreaterThanOrEqual(1);
  });

  test("fallback recovers: failed attempt stays content-less, the success record carries the prompt", async (ctx) => {
    if (!etcdReachable || !app || !sls) {
      ctx.skip();
      return;
    }
    const res = await chat("failure-content-recover", RECOVER_SENTINEL);
    expect(res.status).toBe(200);

    const successLog = await waitForLog(
      sls,
      (l) =>
        (l.get("prompt") ?? "").includes(RECOVER_SENTINEL) &&
        l.get("status_code") === "200",
      "recovered request's success record with prompt",
    );
    const requestId = successLog.get("request_id")!;
    // The failed first attempt is recorded — without the prompt.
    const deadline = Date.now() + 10_000;
    let failedAttempt: Map<string, string> | undefined;
    while (Date.now() < deadline && !failedAttempt) {
      failedAttempt = logsFor(sls, FULL_LOGSTORE).find(
        (l) =>
          l.get("request_id") === requestId &&
          Number(l.get("status_code") ?? "0") >= 400,
      );
      if (!failedAttempt) await new Promise((r) => setTimeout(r, 100));
    }
    expect(failedAttempt, "failed attempt record").toBeDefined();
    expect(failedAttempt!.get("prompt")).toBeUndefined();
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
    }
  });
});
