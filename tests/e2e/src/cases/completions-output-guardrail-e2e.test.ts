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

// E2E for #911 finding [23]: /v1/completions must run OUTPUT guardrails, not
// just the input hook. Pre-fix the model's completion text was relayed
// unscanned, so a keyword/DLP block enforced on /v1/chat/completions was
// bypassable by moving the response leg to the legacy completions surface.
// This drives an innocent prompt at an upstream that emits a forbidden word in
// its completion `text`; the output guardrail must turn it into a redacted
// content_filter 422 that never carries the forbidden word. Pre-fix the caller
// received a 200 with the leaked text.

const CALLER_PLAINTEXT = "sk-cmpl-out-gr-caller";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

const FORBIDDEN_WORD = "leakedsecret";
const GUARDRAIL_NAME = "cmpl-out-gr-keyword";

describe("completions output guardrail (#911 [23])", () => {
  let app: SpawnedApp | undefined;
  let nonStreamUpstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let cleanUpstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    // Legacy /v1/completions response shape, carrying the forbidden word in
    // the choice `text` — the caller's prompt is innocent, the forbidden
    // content originates from the model.
    nonStreamUpstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "cmpl-leak",
        object: "text_completion",
        created: 0,
        model: "gpt-3.5-turbo-instruct",
        choices: [
          {
            text: `Sure, here it is: ${FORBIDDEN_WORD}.`,
            index: 0,
            finish_reason: "stop",
            logprobs: null,
          },
        ],
        usage: { prompt_tokens: 5, completion_tokens: 8, total_tokens: 13 },
      },
    });
    streamUpstream = await startOpenAiUpstream({
      streamEvents: [
        JSON.stringify({
          id: "cmpl-stream-leak",
          object: "text_completion",
          choices: [{ index: 0, text: "leaked", finish_reason: null }],
        }),
        JSON.stringify({
          id: "cmpl-stream-leak",
          object: "text_completion",
          choices: [{ index: 0, text: "secret", finish_reason: "stop" }],
        }),
        "[DONE]",
      ],
    });
    cleanUpstream = await startOpenAiUpstream({
      streamEvents: [
        JSON.stringify({
          id: "cmpl-stream-clean",
          object: "text_completion",
          choices: [{ index: 0, text: "first", finish_reason: null }],
        }),
        JSON.stringify({
          id: "cmpl-stream-clean",
          object: "text_completion",
          choices: [{ index: 0, text: "second", finish_reason: "stop" }],
        }),
        "[DONE]",
      ],
    });

    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "cmpl-out-gr-pk",
      secret: "sk-mock",
      api_base: `${nonStreamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "cmpl-out-gr",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: pk.id,
    });
    const streamPk = await seed.createProviderKey({
      display_name: "cmpl-out-gr-stream-pk",
      secret: "sk-mock",
      api_base: `${streamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "cmpl-out-gr-stream",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: streamPk.id,
    });
    const cleanPk = await seed.createProviderKey({
      display_name: "cmpl-out-gr-clean-pk",
      secret: "sk-mock",
      api_base: `${cleanUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "cmpl-out-gr-clean",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: cleanPk.id,
    });
    // Output keyword guardrail (env-wide) — runs against the completion text
    // after the upstream call returns, before relay to the caller.
    await seed.createGuardrail({
      name: GUARDRAIL_NAME,
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN_WORD }],
    });
    // Authentication is seeded last so a successful request implies every
    // model and guardrail row above was already visible to the watch.
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["cmpl-out-gr", "cmpl-out-gr-stream", "cmpl-out-gr-clean"],
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
      const ids = new Set(body.data?.map((model) => model.id));
      return ["cmpl-out-gr", "cmpl-out-gr-stream", "cmpl-out-gr-clean"].every((id) =>
        ids.has(id),
      );
    });
  });

  afterAll(async () => {
    await app?.exit();
    await nonStreamUpstream?.close();
    await streamUpstream?.close();
    await cleanUpstream?.close();
  });

  async function postCompletion(): Promise<Response> {
    return fetch(`${app!.proxyUrl}/v1/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ model: "cmpl-out-gr", prompt: "innocent question" }),
    });
  }

  test("model-emitted forbidden text on /v1/completions is blocked with content_filter 422", async (ctx) => {
    if (!etcdReachable || !app || !nonStreamUpstream) {
      ctx.skip();
      return;
    }

    const upstreamBaseline = nonStreamUpstream.receivedRequests.length;
    const res = await postCompletion();
    expect(res.status).toBe(422);
    const bodyText = await res.text();

    // The forbidden word MUST NOT reach the caller anywhere in the envelope —
    // that is the whole point of the output guardrail (echoing it back would
    // defeat it).
    expect(bodyText).not.toContain(FORBIDDEN_WORD);

    const body = JSON.parse(bodyText) as {
      error?: { type?: unknown; message?: unknown };
    };
    // Pin the OpenAI/Azure content_filter taxonomy so a 422 from a different
    // path (schema validation, etc.) would fail this test.
    expect(body.error?.type).toBe("content_filter");
    // #519 B.4b: the redacted message names WHICH guardrail fired (operator
    // metadata, not matched content).
    expect(String(body.error?.message)).toContain(`guardrail '${GUARDRAIL_NAME}'`);
    expect(nonStreamUpstream.receivedRequests).toHaveLength(upstreamBaseline + 1);
    expect(nonStreamUpstream.receivedRequests.at(-1)?.body).toContain("innocent question");

    // #911 [23] billed-then-blocked telemetry: the upstream already charged
    // for this response, so the block must be recorded on the CHARGED path
    // (carrying the real provider + resolved model + billed usage into the
    // UsageEvent) rather than the zeroed error path. The observable signature
    // is the `provider` label on the 422 request metric: the charged Ok path
    // records the real provider ("openai"); the pre-fix bare-error path
    // recorded "unknown" and dropped the billed usage from cp-api's ledger.
    const scrape = await fetch(`${app.metricsUrl}/metrics`).then((r) => r.text());
    const blocked422 = scrape
      .split("\n")
      .filter((l) => l.startsWith("aisix_requests_total{"))
      .filter((l) => /status="422"/.test(l));
    // The block went through the charged path: real provider, resolved model.
    expect(
      blocked422.some((l) => /provider="openai"/.test(l) && /model="cmpl-out-gr"/.test(l)),
      `no charged-path 422 metric (provider=openai, model=cmpl-out-gr):\n${blocked422.join("\n")}`,
    ).toBe(true);
    // And NOT the zeroed error path (which attributes provider="unknown").
    expect(
      blocked422.filter((l) => /provider="unknown"/.test(l)),
      `billed-then-blocked completion fell onto the zeroed error path:\n${blocked422.join("\n")}`,
    ).toHaveLength(0);
  });

  test("split forbidden text in an SSE stream is blocked before any upstream frame is released", async (ctx) => {
    if (!etcdReachable || !app || !streamUpstream) {
      ctx.skip();
      return;
    }

    const upstreamBaseline = streamUpstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "cmpl-out-gr-stream",
        prompt: "innocent question",
        stream: true,
      }),
    });
    expect(res.status).toBe(422);
    const body = await res.text();
    expect(body).not.toContain("leaked");
    expect(body).not.toContain("secret");
    expect(body).not.toContain("cmpl-stream-leak");
    expect(JSON.parse(body).error?.type).toBe("content_filter");
    expect(streamUpstream.receivedRequests).toHaveLength(upstreamBaseline + 1);
    expect(streamUpstream.receivedRequests.at(-1)?.body).toContain("innocent question");
  });

  test("clean SSE frames keep their observable order", async (ctx) => {
    if (!etcdReachable || !app || !cleanUpstream) {
      ctx.skip();
      return;
    }

    const res = await fetch(`${app.proxyUrl}/v1/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "cmpl-out-gr-clean",
        prompt: "innocent question",
        stream: true,
      }),
    });
    expect(res.status).toBe(200);
    const body = await res.text();
    expect(body.indexOf("first")).toBeGreaterThanOrEqual(0);
    expect(body.indexOf("second")).toBeGreaterThan(body.indexOf("first"));
    expect(body.indexOf("[DONE]")).toBeGreaterThan(body.indexOf("second"));
  });
});
