import { createHash, randomUUID } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  ProxyClient,
  spawnApp,
  startOpenAiUpstream,
  awaitWindowHeadroom,
  waitConfigPropagation,
  metricDelta,
  scrapeMetrics,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// Task 8 (spend-budget-local plan): end-to-end proof that a per-api-key
// spend ceiling (`RateLimitPolicy.max_spend_micro_usd`) is enforced by the
// gateway's own rate limiter, on real HTTP, across every client-facing
// endpoint family — not just `/v1/chat/completions`. This repo's
// most-repeated bug shape is a per-request mechanism wired into chat only,
// with `/v1/messages` (Anthropic SDK) and `/v1/responses` (Codex) silently
// unaffected; a spec that only drives chat would stay green forever while
// two thirds of the feature is dead.
//
// Every scenario below prices a call at exactly 1000 micro-USD (500 prompt
// + 500 completion tokens, each priced at 0.001 USD/1k) and uses a policy
// that sets ONLY `max_spend_micro_usd` — no `max_requests`/`max_tokens` — so
// the *only* layer that can produce a 429 on these keys is the spend
// ceiling itself; there is no ambiguity with a token/request-count layer.
//
// Two corrections vs. the plan's illustrative numbers, verified against the
// source rather than assumed (CLAUDE.md Research Discipline):
//
// 1. `crates/aisix-ratelimit/src/window.rs` `FixedWindowCounter::is_exceeded`
//    checks-but-does-not-increment: `count >= limit`. Spend, like tokens, is
//    only known once the upstream has answered, so the counter is only
//    incremented AFTER a response — the same "first call always slips
//    through" shape as the existing token-cap specs (see
//    `hourly-token-cap-e2e.test.ts`). With a 1000-micro-USD-per-call cost,
//    a cap of 2500 (the plan's illustrative number) is not exceeded until
//    the *4th* call (2000 < 2500 admits the 3rd, landing on 3000). A cap of
//    2000 (exactly 2x the per-call cost) makes the 3rd call the first
//    refused: committed=2000 >= 2000 blocks it. That is what this file uses.
// 2. `crates/aisix-proxy/src/error.rs`: `ErrorBody.budget` is
//    `#[serde(flatten)]`, so a `billing_error` 429's `scope`/`scope_ref`/
//    `limit_usd`/`period`/`retry_after_seconds` land DIRECTLY on `error`,
//    not nested under `error.budget`. Also: this structured detail (and the
//    `billing_error` type token itself) is an OpenAI-envelope extension
//    only — `/v1/messages` always answers in the Anthropic SDK's strict
//    `{type:"error", error:{type, message}}` shape (module doc + the
//    `anthropic_envelope_budget_exceeded_omits_structured_fields` /
//    `anthropic_envelope_429_budget_exceeded_maps_to_rate_limit_error` unit
//    tests in `error.rs`), where a budget 429 reports `error.type ==
//    "rate_limit_error"` with no scope/limit fields at all — indistinguishable
//    on the wire from a token/request-count 429. That is why every
//    `/v1/messages` policy below carries ONLY `max_spend_micro_usd`: it is
//    the only way to attribute the 429 to the spend mechanism from outside.
//
// Also covers, per the task brief, the three call sites earlier tasks left
// with no handler-level test: the cross-provider BRIDGE streaming spend
// commits at `messages.rs` (`cross_provider_dispatch`'s streaming closure)
// and `responses.rs` (`responses_cross_provider_to_target`'s streaming
// closure), and the legacy `/v1/completions` streaming commit. Those three
// use a single-shot-overexpend cap (500, half the 1000-micro-USD call cost)
// so ONE streamed call's post-stream commit alone exceeds it — mirroring
// the accepted pattern in `streaming-tpm-commit-e2e.test.ts` (#688) rather
// than chaining exact call counts across a commit that races the client's
// read of the stream body.

const hash = (s: string) => createHash("sha256").update(s).digest("hex");

// ---- shared pricing model -------------------------------------------------
// 500 + 500 tokens, each side priced at 0.001 USD/1k: (500*0.001 +
// 500*0.001)/1000 = 0.001 USD = 1000 micro-USD, exactly. Both sides are
// non-zero so no handler's "estimate when a usage field reads 0" fallback
// (chat.rs, completions.rs) ever substitutes a different number.
const TOKENS = 500;
const COST = { input_per_1k: 0.001, output_per_1k: 0.001 };
const COST_PER_CALL_MICRO = 1000;
const CAP_MULTI = 2000; // 3rd call is the first refused (see note 1 above)
const CAP_SINGLE = 500; // every call after the first is refused

function chatBody(id: string) {
  return {
    id,
    object: "chat.completion",
    created: 0,
    model: "gpt-4o-mini",
    choices: [
      { index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" },
    ],
    usage: { prompt_tokens: TOKENS, completion_tokens: TOKENS, total_tokens: TOKENS * 2 },
  };
}

function anthropicBody(id: string) {
  return {
    id,
    type: "message",
    role: "assistant",
    model: "claude-3-5-haiku-20241022",
    content: [{ type: "text", text: "ok" }],
    stop_reason: "end_turn",
    usage: { input_tokens: TOKENS, output_tokens: TOKENS },
  };
}

function responsesBody(id: string) {
  return {
    id,
    object: "response",
    created_at: 0,
    status: "completed",
    model: "gpt-4o-mini",
    output: [
      {
        id: "msg_1",
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: "ok" }],
      },
    ],
    usage: { input_tokens: TOKENS, output_tokens: TOKENS, total_tokens: TOKENS * 2 },
  };
}

// OpenAI chat.completion.chunk stream, terminal frame carrying the
// authoritative usage — the shape `cross_provider_dispatch` (messages.rs)
// bridges from when the dispatched model speaks OpenAI but the caller
// addressed it through `/v1/messages`.
function chatStreamEvents(id: string): string[] {
  const chunk = (json: Record<string, unknown>) =>
    JSON.stringify({ id, object: "chat.completion.chunk", created: 0, model: "gpt-4o-mini", ...json });
  return [
    chunk({ choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] }),
    chunk({ choices: [{ index: 0, delta: { content: "hi" }, finish_reason: null }] }),
    chunk({ choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }),
    chunk({
      choices: [],
      usage: { prompt_tokens: TOKENS, completion_tokens: TOKENS, total_tokens: TOKENS * 2 },
    }),
    "[DONE]",
  ];
}

// Anthropic Messages streaming wire shape — the shape
// `responses_cross_provider_to_target` (responses.rs) bridges from when the
// dispatched model speaks Anthropic but the caller addressed it through
// `/v1/responses`. input_tokens lands in message_start, output_tokens in
// message_delta (real Anthropic streaming semantics).
function anthropicStreamEvents(id: string): string[] {
  return [
    JSON.stringify({
      type: "message_start",
      message: {
        id,
        type: "message",
        role: "assistant",
        model: "claude-3-5-haiku-20241022",
        content: [],
        usage: { input_tokens: TOKENS, output_tokens: 0 },
      },
    }),
    JSON.stringify({
      type: "content_block_start",
      index: 0,
      content_block: { type: "text", text: "" },
    }),
    JSON.stringify({
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "hi" },
    }),
    JSON.stringify({ type: "content_block_stop", index: 0 }),
    JSON.stringify({
      type: "message_delta",
      delta: { stop_reason: "end_turn" },
      usage: { output_tokens: TOKENS },
    }),
    JSON.stringify({ type: "message_stop" }),
  ];
}

// Legacy /v1/completions streaming shape, terminal frame carrying usage.
function completionStreamEvents(id: string): string[] {
  return [
    JSON.stringify({ id, object: "text_completion", choices: [{ index: 0, text: "hi", finish_reason: null }] }),
    JSON.stringify({ id, object: "text_completion", choices: [{ index: 0, text: " there", finish_reason: "stop" }] }),
    JSON.stringify({
      id,
      object: "text_completion",
      choices: [],
      usage: { prompt_tokens: TOKENS, completion_tokens: TOKENS, total_tokens: TOKENS * 2 },
    }),
    "[DONE]",
  ];
}

interface ErrorEnvelope {
  error?: {
    type?: string;
    code?: string;
    message?: string;
    scope?: string;
    scope_ref?: string;
    limit_usd?: string;
    period?: string;
    retry_after_seconds?: number;
  };
}
interface AnthropicErrorEnvelope {
  type?: string;
  error?: { type?: string; message?: string };
}

describe("spend budget e2e (task 8: three endpoint families + bridge/legacy streaming)", () => {
  let app: SpawnedApp | undefined;
  let etcd: EtcdClient | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  const upstreams: OpenAiUpstream[] = [];

  async function up(opts: Parameters<typeof startOpenAiUpstream>[0]): Promise<OpenAiUpstream> {
    const u = await startOpenAiUpstream(opts);
    upstreams.push(u);
    return u;
  }

  // Fixed identities so a policy's `scope_ref` can name a key before that
  // key is written (`PolicyScope::ApiKey` matches on the key's own etcd
  // entry id).
  const CHAT_KEY = "sk-sb-chat";
  const CHAT_KEY_ID = randomUUID();
  const MSG_KEY = "sk-sb-messages";
  const MSG_KEY_ID = randomUUID();
  const RESP_KEY = "sk-sb-responses";
  const RESP_KEY_ID = randomUUID();
  const UNPRICED_KEY = "sk-sb-unpriced";
  const UNPRICED_KEY_ID = randomUUID();
  const MSG_BRIDGE_KEY = "sk-sb-messages-bridge";
  const MSG_BRIDGE_KEY_ID = randomUUID();
  const RESP_BRIDGE_KEY = "sk-sb-responses-bridge";
  const RESP_BRIDGE_KEY_ID = randomUUID();
  const CMPL_STREAM_KEY = "sk-sb-completions-stream";
  const CMPL_STREAM_KEY_ID = randomUUID();

  const UNPRICED_MODEL = "sb-unpriced";
  const UNPRICED_POLICY_NAME = "sb-unpriced-budget";

  beforeAll(async () => {
    etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    // ---- /v1/chat/completions family (direct OpenAI dispatch) ----
    const chatUp = await up({ nonStreamBody: chatBody("sb-chat") });
    const chatPk = await seed.createProviderKey({
      display_name: "sb-chat-pk",
      secret: "sk-mock",
      api_base: `${chatUp.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sb-chat",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: chatPk.id,
      cost: COST,
    });
    // Same upstream, no `cost`: the unpriced-visibility case.
    await seed.createModel({
      display_name: UNPRICED_MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: chatPk.id,
    });

    // ---- /v1/messages family (native Anthropic passthrough) ----
    const msgUp = await up({ nonStreamBody: anthropicBody("sb-messages") });
    const msgPk = await seed.createProviderKey({
      display_name: "sb-messages-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: msgUp.baseUrl,
    });
    await seed.createModel({
      display_name: "sb-messages",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: msgPk.id,
      cost: COST,
    });

    // ---- /v1/responses family (direct OpenAI dispatch) ----
    const respUp = await up({ nonStreamBody: responsesBody("sb-responses") });
    const respPk = await seed.createProviderKey({
      display_name: "sb-responses-pk",
      secret: "sk-mock",
      api_base: `${respUp.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sb-responses",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: respPk.id,
      cost: COST,
    });

    // ---- /v1/messages cross-provider BRIDGE, streaming (messages.rs) ----
    // provider "openai" + speaks_anthropic()==false → cross_provider_dispatch.
    const msgBridgeUp = await up({ streamEvents: chatStreamEvents("sb-msg-bridge"), eventDelayMs: 2 });
    const msgBridgePk = await seed.createProviderKey({
      display_name: "sb-msg-bridge-pk",
      secret: "sk-mock",
      api_base: `${msgBridgeUp.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sb-msg-bridge",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: msgBridgePk.id,
      cost: COST,
    });

    // ---- /v1/responses cross-provider BRIDGE, streaming (responses.rs) ----
    // provider "anthropic" != "openai" → responses_cross_provider_to_target.
    const respBridgeUp = await up({ streamEvents: anthropicStreamEvents("sb-resp-bridge"), eventDelayMs: 2 });
    const respBridgePk = await seed.createProviderKey({
      display_name: "sb-resp-bridge-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: respBridgeUp.baseUrl,
    });
    await seed.createModel({
      display_name: "sb-resp-bridge",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: respBridgePk.id,
      cost: COST,
    });

    // ---- legacy /v1/completions, streaming (completions.rs:1075) ----
    const cmplUp = await up({ streamEvents: completionStreamEvents("sb-cmpl-stream"), eventDelayMs: 2 });
    const cmplPk = await seed.createProviderKey({
      display_name: "sb-cmpl-stream-pk",
      secret: "sk-mock",
      api_base: `${cmplUp.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sb-cmpl-stream",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: cmplPk.id,
      cost: COST,
    });

    // ---- policies: classic form, ONLY `max_spend_micro_usd` set ----
    await seed.createRateLimitPolicy({
      name: "sb-chat-budget",
      scope: "api_key",
      scope_ref: CHAT_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_MULTI,
    });
    await seed.createRateLimitPolicy({
      name: "sb-messages-budget",
      scope: "api_key",
      scope_ref: MSG_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_MULTI,
    });
    await seed.createRateLimitPolicy({
      name: "sb-responses-budget",
      scope: "api_key",
      scope_ref: RESP_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_MULTI,
    });
    await seed.createRateLimitPolicy({
      name: UNPRICED_POLICY_NAME,
      scope: "api_key",
      scope_ref: UNPRICED_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_MULTI,
    });
    await seed.createRateLimitPolicy({
      name: "sb-messages-bridge-budget",
      scope: "api_key",
      scope_ref: MSG_BRIDGE_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_SINGLE,
    });
    await seed.createRateLimitPolicy({
      name: "sb-responses-bridge-budget",
      scope: "api_key",
      scope_ref: RESP_BRIDGE_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_SINGLE,
    });
    await seed.createRateLimitPolicy({
      name: "sb-completions-stream-budget",
      scope: "api_key",
      scope_ref: CMPL_STREAM_KEY_ID,
      window: "minute",
      max_spend_micro_usd: CAP_SINGLE,
    });

    // ---- API keys LAST (tests/e2e/AGENTS.md): raw etcd writes with fixed
    // ids so the policies' `scope_ref` above names them correctly. ----
    const putKey = (id: string, plaintext: string, models: string[]) =>
      etcd!.put(
        `${app!.etcdPrefix}/api_keys/${id}`,
        JSON.stringify({ key_hash: hash(plaintext), allowed_models: models }),
      );
    await putKey(CHAT_KEY_ID, CHAT_KEY, ["sb-chat"]);
    await putKey(MSG_KEY_ID, MSG_KEY, ["sb-messages"]);
    await putKey(RESP_KEY_ID, RESP_KEY, ["sb-responses"]);
    await putKey(UNPRICED_KEY_ID, UNPRICED_KEY, [UNPRICED_MODEL]);
    await putKey(MSG_BRIDGE_KEY_ID, MSG_BRIDGE_KEY, ["sb-msg-bridge"]);
    await putKey(RESP_BRIDGE_KEY_ID, RESP_BRIDGE_KEY, ["sb-resp-bridge"]);
    await putKey(CMPL_STREAM_KEY_ID, CMPL_STREAM_KEY, ["sb-cmpl-stream"]);

    // Gate on the LAST-written key: revision order means its visibility
    // implies every row above (models, policies, every earlier key) is
    // already in the snapshot.
    const probe = new ProxyClient(app.proxyUrl, CMPL_STREAM_KEY);
    await waitConfigPropagation(async () => {
      const res = await probe.listModels();
      if (res.status !== 200) return false;
      const data = (res.body as { data?: Array<{ id?: string }> }).data ?? [];
      return data.some((m) => m.id === "sb-cmpl-stream");
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  function post(path: string, body: unknown, key: string): Promise<Response> {
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${key}`,
        "x-api-key": key,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
  }

  /** First `admits` calls must be 200; the next call must be 429. Returns the
   * blocked response so the caller can inspect its body. */
  async function expectAdmittedThenBlocked(
    make: () => Promise<Response>,
    admits: number,
  ): Promise<Response> {
    for (let i = 0; i < admits; i++) {
      const res = await make();
      expect(res.status, `call ${i + 1} should be admitted`).toBe(200);
      await res.text();
    }
    const blocked = await make();
    expect(blocked.status, `call ${admits + 1} should be refused`).toBe(429);
    return blocked;
  }

  /** Retry `make` until it 429s (streamed commits land a tick after the
   * client finishes reading the body — see #688's `nextCallEventually429`,
   * the same accepted pattern this mirrors) or the deadline passes. */
  async function untilBlocked(
    make: () => Promise<Response>,
    deadlineMs = 5000,
  ): Promise<{ status: number; text: string }> {
    const deadline = Date.now() + deadlineMs;
    let status = 0;
    let text = "";
    while (Date.now() < deadline) {
      const res = await make();
      status = res.status;
      text = await res.text();
      if (status === 429) return { status, text };
      await new Promise((r) => setTimeout(r, 100));
    }
    return { status, text };
  }

  test("/v1/chat/completions: 3rd call over a 2-call spend ceiling is a precise billing_error 429", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const make = () =>
      post("/v1/chat/completions", { model: "sb-chat", messages: [{ role: "user", content: "hi" }] }, CHAT_KEY);
    const blocked = await expectAdmittedThenBlocked(make, 2);
    const body = (await blocked.json()) as ErrorEnvelope;

    expect(body.error?.type).toBe("billing_error");
    expect(body.error?.code).toBe("budget_exceeded");
    expect(body.error?.scope).toBe("api_key");
    expect(body.error?.scope_ref).toBe(CHAT_KEY_ID);
    expect(body.error?.limit_usd).toBe("0.002000");
    expect(body.error?.period).toBe("minute");
    expect(typeof body.error?.retry_after_seconds).toBe("number");
    expect(body.error?.retry_after_seconds).toBeGreaterThan(0);
    expect(body.error?.retry_after_seconds).toBeLessThanOrEqual(60);
    expect(blocked.headers.get("retry-after")).toBeTruthy();
  }, 30_000);

  test("/v1/messages: 3rd call over a 2-call spend ceiling is 429 (Anthropic envelope, no request/token layer to attribute it to but spend)", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const make = () =>
      post(
        "/v1/messages",
        { model: "sb-messages", max_tokens: 64, messages: [{ role: "user", content: "hi" }] },
        MSG_KEY,
      );
    const blocked = await expectAdmittedThenBlocked(make, 2);
    const body = (await blocked.json()) as AnthropicErrorEnvelope;

    // The Anthropic SDK's strict ErrorType has no billing-specific literal:
    // a budget 429 reports the same `rate_limit_error` a token/request 429
    // would (error.rs's `anthropic_kind_from_status` + the unit tests cited
    // at the top of this file). This policy carries ONLY
    // `max_spend_micro_usd`, so there is no other layer that could have
    // produced this 429.
    expect(body.type).toBe("error");
    expect(body.error?.type).toBe("rate_limit_error");
    expect(blocked.headers.get("retry-after")).toBeTruthy();
  }, 30_000);

  test("/v1/responses: 3rd call over a 2-call spend ceiling is a precise billing_error 429", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const make = () => post("/v1/responses", { model: "sb-responses", input: "hi" }, RESP_KEY);
    const blocked = await expectAdmittedThenBlocked(make, 2);
    const body = (await blocked.json()) as ErrorEnvelope;

    expect(body.error?.type).toBe("billing_error");
    expect(body.error?.code).toBe("budget_exceeded");
    expect(body.error?.scope).toBe("api_key");
    expect(body.error?.scope_ref).toBe(RESP_KEY_ID);
    expect(body.error?.limit_usd).toBe("0.002000");
    expect(body.error?.period).toBe("minute");
  }, 30_000);

  test("a model with no `cost` is admitted under a spend-capped policy and counted as unpriced", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const before = await scrapeMetrics(app.metricsUrl);
    const make = () =>
      post("/v1/chat/completions", { model: UNPRICED_MODEL, messages: [{ role: "user", content: "hi" }] }, UNPRICED_KEY);

    // Spend can't be computed for an unpriced row, so the ceiling can never
    // bind: every call is admitted, however many are sent.
    for (let i = 0; i < 3; i++) {
      const res = await make();
      expect(res.status, `unpriced call ${i + 1}`).toBe(200);
      await res.text();
    }

    let delta = 0;
    for (let i = 0; i < 40; i++) {
      delta = metricDelta(before, await scrapeMetrics(app.metricsUrl), "aisix_budget_unpriced_requests_total", {
        policy: UNPRICED_POLICY_NAME,
        model: UNPRICED_MODEL,
      });
      if (delta > 0) break;
      await new Promise((r) => setTimeout(r, 50));
    }
    expect(delta, "an unpriced request under a spend-capped policy must be visible on the metric").toBeGreaterThan(0);
  }, 30_000);

  test("/v1/messages cross-provider bridge streaming commits spend (messages.rs cross_provider_dispatch closure)", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const make = () =>
      post(
        "/v1/messages",
        { model: "sb-msg-bridge", max_tokens: 64, stream: true, messages: [{ role: "user", content: "hi" }] },
        MSG_BRIDGE_KEY,
      );

    // Single call already exceeds the 500-micro-USD cap (cost=1000/call):
    // this is the streaming path's own post-stream commit closure, so its
    // result must be visible on the NEXT call without needing a multi-call
    // sequence that could race the commit.
    const first = await make();
    expect(first.status).toBe(200);
    expect(await first.text()).toContain("message_stop");

    const { status, text } = await untilBlocked(make);
    expect(status, "spend committed by the bridge streaming closure must eventually refuse the next call").toBe(429);
    const body = JSON.parse(text) as AnthropicErrorEnvelope;
    expect(body.error?.type).toBe("rate_limit_error");
  }, 30_000);

  test("/v1/responses cross-provider bridge streaming commits spend (responses.rs responses_cross_provider_to_target closure)", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const make = () => post("/v1/responses", { model: "sb-resp-bridge", input: "hi", stream: true }, RESP_BRIDGE_KEY);

    const first = await make();
    expect(first.status).toBe(200);
    expect(await first.text()).toContain("event: response.completed");

    const { status, text } = await untilBlocked(make);
    expect(status, "spend committed by the bridge streaming closure must eventually refuse the next call").toBe(429);
    const body = JSON.parse(text) as ErrorEnvelope;
    expect(body.error?.type).toBe("billing_error");
    expect(body.error?.scope).toBe("api_key");
    expect(body.error?.scope_ref).toBe(RESP_BRIDGE_KEY_ID);
  }, 30_000);

  test("legacy /v1/completions streaming commits spend (completions.rs post-stream spend commit)", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    const make = () => post("/v1/completions", { model: "sb-cmpl-stream", prompt: "hi", stream: true }, CMPL_STREAM_KEY);

    const first = await make();
    expect(first.status).toBe(200);
    expect(await first.text()).toContain("[DONE]");

    const { status, text } = await untilBlocked(make);
    expect(status, "spend committed by the legacy completions streaming path must eventually refuse the next call").toBe(429);
    const body = JSON.parse(text) as ErrorEnvelope;
    expect(body.error?.type).toBe("billing_error");
    expect(body.error?.scope).toBe("api_key");
    expect(body.error?.scope_ref).toBe(CMPL_STREAM_KEY_ID);
  }, 30_000);
});

// `scope: team` and `scope: team_member` were only ever exercised by unit
// tests. They are the two scopes where the bucket key is NOT one-per-key, so
// they are also the two where a wrong bucket is invisible: collapse
// `team_member` into one shared bucket and nothing errors — the first member
// to arrive simply eats the whole team's budget, and every other member gets
// a 429 that looks exactly like a legitimate one.
//
// The contract these two cases pin, stated without reference to the
// implementation: a team ceiling is one budget the whole team draws from; a
// team-member ceiling is that same number handed to each member separately.
describe("spend budget e2e: team and team_member ceilings divide the budget differently", () => {
  let app: SpawnedApp | undefined;
  let etcd: EtcdClient | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;
  let upstream: OpenAiUpstream | undefined;

  const SHARED_TEAM = "sb-team-shared";
  const PER_MEMBER_TEAM = "sb-team-per-member";

  // Two keys per team, belonging to two different members.
  const TEAM_A = "sk-sb-team-a";
  const TEAM_A_ID = randomUUID();
  const TEAM_B = "sk-sb-team-b";
  const TEAM_B_ID = randomUUID();
  const MEMBER_C = "sk-sb-member-c";
  const MEMBER_C_ID = randomUUID();
  const MEMBER_D = "sk-sb-member-d";
  const MEMBER_D_ID = randomUUID();

  beforeAll(async () => {
    etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    upstream = await startOpenAiUpstream({ nonStreamBody: chatBody("sb-team-model") });
    const pk = await seed.createProviderKey({
      display_name: "sb-team-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "sb-team-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
      cost: COST,
    });

    // Only `max_spend_micro_usd` on both, so any 429 here is the spend layer
    // and nothing else.
    await seed.createRateLimitPolicy({
      name: "sb-team-shared-budget",
      scope: "team",
      scope_ref: SHARED_TEAM,
      window: "minute",
      max_spend_micro_usd: CAP_MULTI,
    });
    await seed.createRateLimitPolicy({
      name: "sb-team-per-member-budget",
      scope: "team_member",
      scope_ref: PER_MEMBER_TEAM,
      window: "minute",
      max_spend_micro_usd: CAP_MULTI,
    });

    const putKey = (id: string, plaintext: string, team: string, user: string) =>
      etcd!.put(
        `${app!.etcdPrefix}/api_keys/${id}`,
        JSON.stringify({
          key_hash: hash(plaintext),
          allowed_models: ["sb-team-model"],
          team_id: team,
          user_id: user,
        }),
      );
    await putKey(TEAM_A_ID, TEAM_A, SHARED_TEAM, "u1");
    await putKey(TEAM_B_ID, TEAM_B, SHARED_TEAM, "u2");
    await putKey(MEMBER_C_ID, MEMBER_C, PER_MEMBER_TEAM, "u1");
    await putKey(MEMBER_D_ID, MEMBER_D, PER_MEMBER_TEAM, "u2");

    const probe = new ProxyClient(app.proxyUrl, MEMBER_D);
    await waitConfigPropagation(async () => {
      const res = await probe.listModels();
      if (res.status !== 200) return false;
      const data = (res.body as { data?: Array<{ id?: string }> }).data ?? [];
      return data.some((m) => m.id === "sb-team-model");
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    if (etcd && app) await etcd.deletePrefix(app.etcdPrefix);
  });

  function call(key: string): Promise<Response> {
    return fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
      body: JSON.stringify({
        model: "sb-team-model",
        messages: [{ role: "user", content: "hi" }],
      }),
    });
  }

  async function admit(key: string, times: number): Promise<void> {
    for (let i = 0; i < times; i++) {
      const res = await call(key);
      expect(res.status, `${key} call ${i + 1} should be admitted`).toBe(200);
      await res.text();
    }
  }

  test("a team ceiling is one budget the whole team draws from", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    // One member spends the whole ceiling...
    await admit(TEAM_A, 2);

    // ...and the OTHER member's very first call is already refused. This is
    // the assertion that fails if each key got its own bucket.
    const blocked = await call(TEAM_B);
    expect(blocked.status, "a teammate must inherit the team's exhausted budget").toBe(429);
    const body = (await blocked.json()) as ErrorEnvelope;
    expect(body.error?.type).toBe("billing_error");
    expect(body.error?.code).toBe("budget_exceeded");
    expect(body.error?.scope).toBe("team");
    expect(body.error?.scope_ref).toBe(SHARED_TEAM);
  }, 30_000);

  test("a team_member ceiling hands that same budget to each member separately", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    await awaitWindowHeadroom(5);

    // One member exhausts their own ceiling.
    await admit(MEMBER_C, 2);
    const cBlocked = await call(MEMBER_C);
    expect(cBlocked.status, "a member must be capped at their own ceiling").toBe(429);
    const body = (await cBlocked.json()) as ErrorEnvelope;
    expect(body.error?.scope).toBe("team_member");
    // The team id alone would be ambiguous here: every member shares it,
    // but each has a separate budget, so it cannot say WHICH budget ran
    // out. The envelope names the exhausted budget, member included.
    expect(body.error?.scope_ref).toBe(`${PER_MEMBER_TEAM}:u1`);
    await cBlocked.text().catch(() => undefined);

    // Their teammate is untouched — same policy, independent budget. If the
    // member suffix ever stops reaching the spend bucket, this is the line
    // that catches it: the ceiling silently becomes team-wide.
    await admit(MEMBER_D, 2);
  }, 30_000);
});

// A deployment with no spend ceiling anywhere and one comfortably under
// its ceilings look identical on the rejection counters: both report zero
// budget 429s. So how many ceilings are configured has to be its own
// series, or "nobody ever set a budget" is unmonitorable.
//
// File mode rather than etcd: this block needs a config it can rewrite and
// reload on demand, and it must run wherever the suite runs — an
// etcd-gated block that silently skips is exactly how a metric ships
// without ever being scraped.
describe("spend budget e2e: configured-ceiling count is scrapeable and follows reloads", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;

  const CALLER = "sk-sb-gauge-caller";
  const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");

  // `spendPolicies` spend-capped policies plus one policy that caps only
  // requests — the request-capped one must NOT be counted, or the series
  // degenerates into "how many rate-limit policies exist".
  function resources(upstreamBase: string, spendPolicies: number): string {
    const budgets = Array.from({ length: spendPolicies }, (_, i) => `
  - name: sb-gauge-budget-${i}
    scope: api_key
    scope_ref: sb-gauge-caller
    window: day
    max_spend_micro_usd: ${1_000_000 * (i + 1)}`).join("");
    return `
_format_version: "1"
provider_keys:
  - display_name: sb-gauge-pk
    provider: openai
    api_key: sk-mock
    api_base: ${upstreamBase}/v1
models:
  - display_name: sb-gauge-model
    provider: openai
    model_name: gpt-4o-mini
    provider_key: sb-gauge-pk
api_keys:
  - display_name: sb-gauge-caller
    key_hash: ${CALLER_HASH}
    allowed_models: ["*"]
rate_limit_policies:
  - name: sb-gauge-requests-only
    scope: api_key
    scope_ref: sb-gauge-caller
    window: day
    max_requests: 100000${budgets}
`;
  }

  async function configuredCeilings(): Promise<number | undefined> {
    const samples = await scrapeMetrics(app!.metricsUrl);
    return samples.find((s) => s.name === "aisix_budget_policies_configured")?.value;
  }

  beforeAll(async () => {
    upstream = await startOpenAiUpstream();
    app = await spawnApp({ resourcesFile: resources(upstream.baseUrl, 2) });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("the count is exposed, excludes request-only policies, and drops to zero when the last ceiling is removed", async () => {
    if (!app || !upstream || !app.resourcesPath) throw new Error("setup failed");

    expect(await configuredCeilings()).toBe(2);

    // Removing the last spend ceiling must be visible. Zero has to be
    // reported as zero — an absent series reads as "no traffic yet", which
    // is the state this metric exists to distinguish from.
    await writeFile(app.resourcesPath, resources(upstream.baseUrl, 0), "utf8");
    app.signal("SIGHUP");
    await waitConfigPropagation(async () => (await configuredCeilings()) === 0);
    expect(await configuredCeilings()).toBe(0);

    // And it comes back up on the next reload — a one-way gauge would
    // still pass the assertion above.
    await writeFile(app.resourcesPath, resources(upstream.baseUrl, 3), "utf8");
    app.signal("SIGHUP");
    await waitConfigPropagation(async () => (await configuredCeilings()) === 3);
    expect(await configuredCeilings()).toBe(3);
  }, 30_000);
});
