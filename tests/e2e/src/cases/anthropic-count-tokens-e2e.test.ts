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

// E2E: Anthropic `/v1/messages/count_tokens` through the DP (#418).
//
// The Anthropic SDK exposes this as `anthropic.messages.countTokens(...)`
// — the documented, billing-relevant endpoint callers use to size a
// prompt before a paid `/v1/messages` call. Before #418 the route was
// unregistered and the DP returned a bare 404. This test drives the
// endpoint the way a real Anthropic-SDK / Claude-Code caller does (raw
// HTTP with the `x-api-key` + `anthropic-version` auth shape, since there
// is no Anthropic SDK in this harness) and asserts the externally
// observable contract:
//
//   - the caller gets 200 with `{"input_tokens": <number>}`;
//   - the gateway forwarded to the Anthropic upstream's
//     `/v1/messages/count_tokens` sub-route (NOT `/v1/messages`);
//   - it rewrote the `model` alias to the upstream id;
//   - it spoke the Anthropic auth shape (`x-api-key` +
//     `anthropic-version`), not `Authorization: Bearer`.
//
// The mock-upstream harness is path-agnostic, so feeding it the
// count_tokens response body lets it stand in for Anthropic's
// `/v1/messages/count_tokens`. `receivedRequests` confirms the path and
// request shape the gateway actually sent.
//
// Reference:
// - Anthropic Count Message Tokens API:
//   <https://platform.claude.com/docs/en/api/messages-count-tokens>
//   (`POST /v1/messages/count_tokens` → `{"input_tokens": <int>}`).

const CALLER_PLAINTEXT = "sk-ct-e2e-caller";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

const UPSTREAM_MODEL_ID = "claude-haiku-4-5-20251001";
const MODEL_ALIAS = "ct-e2e";
const TOOL_EMAIL = "tool-owner@example.com";
const TOOL_CN_ID = "11010519491231002X";
const TOOL_IDENTIFIER = "sk-abcdefghijklmnopqrstuv";
const MODERATION_ONLY_WORD = "count-tokens-moderation-only";

describe("anthropic count_tokens e2e: /v1/messages/count_tokens through the DP (#418)", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      // Anthropic's documented count_tokens response shape.
      nonStreamBody: { input_tokens: 42 },
    });
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    // Anthropic bridge appends the path to the bare host (no `/v1`).
    const pk = await seed.createProviderKey({
      display_name: "ct-e2e-pk",
      secret: "sk-ant-mock",
      api_base: upstream.baseUrl,
    });
    await seed.createModel({
      display_name: MODEL_ALIAS,
      provider: "anthropic",
      model_name: UPSTREAM_MODEL_ID,
      provider_key_id: pk.id,
    });
    await seed.createGuardrail({
      name: "ct-e2e-dlp",
      enabled: true,
      hook_point: "input",
      kind: "pii",
      detectors: [
        { type: "email", action: "mask" },
        { type: "china_id_card", action: "block" },
        { type: "api_key", action: "mask" },
      ],
    });
    await seed.createGuardrail({
      name: "ct-e2e-moderation-exempt",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: MODERATION_ONLY_WORD }],
    });
    // Seed caller auth last. Once it authenticates, the ordered etcd watch has
    // already applied the model and both guardrails above.
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: [MODEL_ALIAS],
    });

    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      if (response.status === 401) {
        await response.text();
        return false;
      }
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      return body.data?.some((model) => model.id === MODEL_ALIAS) === true;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("counts tokens against the Anthropic upstream and returns input_tokens", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    const countTokens = (model: string) =>
      fetch(`${app!.proxyUrl}/v1/messages/count_tokens`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "x-api-key": CALLER_PLAINTEXT,
          "anthropic-version": "2023-06-01",
        },
        body: JSON.stringify({
          model,
          messages: [{ role: "user", content: "hello" }],
        }),
      });

    // Baseline-isolate so the assertions below match this count request.
    const baseline = upstream.receivedRequests.length;

    const res = await countTokens(MODEL_ALIAS);

    // Caller-visible contract: 200 + the documented count_tokens body.
    expect(res.status).toBe(200);
    const body = (await res.json()) as { input_tokens?: unknown };
    expect(typeof body.input_tokens).toBe("number");
    expect(body.input_tokens).toBe(42);

    // Request-side wire shape. Pin the sub-route explicitly: a regression
    // that routed count_tokens to `/v1/messages` (or any other path)
    // would still 200 against the path-agnostic mock without this.
    const ctReq = upstream.receivedRequests
      .slice(baseline)
      .find((r) => r.path === "/v1/messages/count_tokens");
    expect(ctReq).toBeDefined();

    // model alias rewritten to the upstream id.
    const sentBody = JSON.parse(ctReq!.body) as {
      model?: string;
      messages?: Array<{ role?: string; content?: unknown }>;
    };
    expect(sentBody.model).toBe(UPSTREAM_MODEL_ID);
    expect(sentBody.messages?.[0]?.role).toBe("user");

    // Anthropic auth shape forwarded to upstream — not Bearer. A
    // regression that forwarded the OpenAI auth shape would 401 in
    // production but pass against the permissive mock without this.
    expect(ctReq!.headers["x-api-key"]).toBe("sk-ant-mock");
    expect(ctReq!.headers["anthropic-version"]).toBeDefined();
    expect(ctReq!.headers["authorization"]).toBeUndefined();
  });

  test("count_tokens on an unknown model returns 404, not a bare route 404", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    // The route exists; an unknown model must surface the gateway's
    // model-not-found path. This guards against a future regression that
    // unregisters the route (which would 404 every model identically).
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: "no-such-model",
        messages: [{ role: "user", content: "hi" }],
      }),
    });
    expect(res.status).toBe(404);
    // Anthropic-shape error envelope so the Claude SDK can parse it.
    const body = (await res.json()) as {
      type?: string;
      error?: { type?: string };
    };
    expect(body.type).toBe("error");
    expect(body.error?.type).toBe("not_found_error");
  });

  test("DLP masks sensitive tool descriptions and schema values before egress", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: "lookup",
            description: `owned by ${TOOL_EMAIL}`,
            input_schema: {
              type: "object",
              properties: {
                owner: { type: "string", default: TOOL_EMAIL },
              },
            },
          },
        ],
      }),
    });
    expect(res.status).toBe(200);
    await res.text();
    const request = upstream.receivedRequests
      .slice(baseline)
      .find((candidate) => candidate.path === "/v1/messages/count_tokens");
    expect(request).toBeDefined();
    expect(request!.body).not.toContain(TOOL_EMAIL);
    expect(request!.body).toContain("[EMAIL_REDACTED]");
  });

  test("DLP blocks sensitive tool schema values before egress", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: "lookup",
            input_schema: {
              type: "object",
              properties: {
                owner: { type: "string", default: TOOL_CN_ID },
              },
            },
          },
        ],
      }),
    });
    expect(res.status).toBe(422);
    expect(await res.text()).not.toContain(TOOL_CN_ID);
    expect(upstream.receivedRequests).toHaveLength(baseline);
  });

  test("DLP rejects a sensitive tool schema key before egress", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: "lookup",
            input_schema: {
              type: "object",
              properties: {
                [TOOL_EMAIL]: { type: "string" },
              },
            },
          },
        ],
      }),
    });

    expect(res.status).toBe(422);
    expect(await res.text()).not.toContain(TOOL_EMAIL);
    expect(upstream.receivedRequests).toHaveLength(baseline);
  });

  test("DLP rejects a mask-matching tool name before egress", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: TOOL_IDENTIFIER,
            input_schema: { type: "object", properties: {} },
          },
        ],
      }),
    });

    expect(res.status).toBe(422);
    expect(await res.text()).not.toContain(TOOL_IDENTIFIER);
    expect(upstream.receivedRequests).toHaveLength(baseline);

    const forced = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [{ role: "user", content: "clean" }],
        tools: [
          {
            name: "lookup",
            input_schema: { type: "object", properties: {} },
          },
        ],
        tool_choice: { type: "tool", name: TOOL_IDENTIFIER },
      }),
    });

    expect(forced.status).toBe(422);
    expect(await forced.text()).not.toContain(TOOL_IDENTIFIER);
    expect(upstream.receivedRequests).toHaveLength(baseline);
  });

  test("DLP rejects a sensitive historical tool input key before egress", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [
          {
            role: "assistant",
            content: [
              {
                type: "tool_use",
                id: "toolu_history",
                name: "lookup",
                input: { [TOOL_EMAIL]: "safe" },
              },
            ],
          },
        ],
      }),
    });

    expect(res.status).toBe(422);
    expect(await res.text()).not.toContain(TOOL_EMAIL);
    expect(upstream.receivedRequests).toHaveLength(baseline);
  });

  test("DLP inspection failure rejects before any upstream request", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({ model: MODEL_ALIAS, messages: "not-an-array" }),
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error?: { type?: string } };
    expect(body.error?.type).toBe("invalid_request_error");
    expect(upstream.receivedRequests).toHaveLength(baseline);
  });

  test("content-moderation keyword guardrails remain exempt", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const baseline = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages/count_tokens`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": CALLER_PLAINTEXT,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: MODEL_ALIAS,
        messages: [{ role: "user", content: MODERATION_ONLY_WORD }],
      }),
    });
    expect(res.status).toBe(200);
    await res.text();
    const request = upstream.receivedRequests
      .slice(baseline)
      .find((candidate) => candidate.path === "/v1/messages/count_tokens");
    expect(request?.body).toContain(MODERATION_ONLY_WORD);
  });
});
