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

// E2E: legacy `POST /v1/completions` endpoint.
//
// Per `docs/api-proxy.md` §4.3, the legacy OpenAI Completions
// endpoint is text-in / text-out (the pre-chat API). It uses
// `prompt: string` instead of `messages: [...]` on the request,
// and `choices[i].text` instead of `choices[i].message.content`
// on the response. Auth and error semantics match chat.
//
// Closes #151 C7 (`/v1/completions` had zero e2e coverage).
// Several real-world callers — long-running `instructor` deployments
// on text-davinci-002, legacy autocomplete tools, retrieval
// pipelines that grep stored prompts — still drive this endpoint.
// A regression that broke the request-body field-name mapping
// (prompt vs messages) or rewrote the response shape would silently
// snap every legacy caller.
//
// One contract pinned:
//
//   - The gateway forwards the caller's `{model, prompt}` body
//     verbatim to the upstream's `/v1/completions` path, with the
//     gateway's Model alias rewritten to the upstream model id and
//     the configured ProviderKey secret as the Authorization bearer.
//   - The upstream's text-completion response shape
//     `{choices: [{text, finish_reason}], usage}` passes back to
//     the caller unmodified.
//
// Reference:
//   - OpenAI legacy Completions API
//     <https://platform.openai.com/docs/api-reference/completions>
//   - `docs/api-proxy.md` §4.3

const CALLER_PLAINTEXT = "sk-completions-legacy-caller";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

const PROMPT_TEXT = "Once upon a time in a";
const COMPLETION_TEXT = " distant kingdom, there lived";

const NON_STREAM_BODY = {
  id: "cmpl-completions-legacy-1",
  object: "text_completion",
  created: Math.floor(Date.now() / 1000),
  model: "gpt-3.5-turbo-instruct",
  choices: [
    {
      text: COMPLETION_TEXT,
      index: 0,
      logprobs: null,
      finish_reason: "stop",
    },
  ],
  usage: {
    prompt_tokens: 6,
    completion_tokens: 7,
    total_tokens: 13,
  },
};

const streamChunk = (text: string, finish_reason: string | null) => ({
  id: "cmpl-completions-legacy-stream",
  object: "text_completion",
  created: 1_700_000_000,
  model: "gpt-3.5-turbo-instruct",
  choices: [{ text, index: 0, logprobs: null, finish_reason }],
});
const STREAM_CHUNKS = [streamChunk(" distant", null), streamChunk(" kingdom", null), streamChunk("", "stop")];

describe("legacy /v1/completions e2e: text-in / text-out passthrough", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let streamUpstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      nonStreamBody: NON_STREAM_BODY,
    });
    streamUpstream = await startOpenAiUpstream({
      streamEvents: [...STREAM_CHUNKS.map((chunk) => JSON.stringify(chunk)), "[DONE]"],
    });
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "completions-legacy-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "completions-legacy-model",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: pk.id,
    });
    const streamPk = await seed.createProviderKey({
      display_name: "completions-legacy-stream-pk",
      secret: "sk-mock",
      api_base: `${streamUpstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "completions-legacy-stream-model",
      provider: "openai",
      model_name: "gpt-3.5-turbo-instruct",
      provider_key_id: streamPk.id,
    });
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["completions-legacy-model", "completions-legacy-stream-model"],
    });

    // The caller key is seeded last, so it authenticating implies every
    // earlier resource is in the snapshot (see this directory's AGENTS.md).
    // Gating here rather than inside a test means each test does not depend
    // on an earlier one having warmed the snapshot — an ordering dependency
    // that passes locally and races the moment a test is run on its own.
    // `/v1/models` is also not a request that exercises anything asserted
    // below, so a broken dispatch surfaces as its own assertion rather than
    // as a propagation timeout.
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      if (res.status !== 200) {
        await res.text();
        return false;
      }
      const body = (await res.json()) as { data?: Array<{ id?: string }> };
      return (body.data ?? []).some((m) => m.id === "completions-legacy-model");
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await streamUpstream?.close();
  });

  test(
    "POST /v1/completions: prompt forwarded, text response passes back",
    async (ctx) => {
      if (!etcdReachable || !app || !upstream) {
        ctx.skip();
        return;
      }

      const reqHeaders = {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      };

      // Readiness — drive the same endpoint with a distinct prompt
      // so the gate verifies the legacy completions dispatcher, not
      // just Model+key+pk visibility on a chat path.
      await waitConfigPropagation(async () => {
        try {
          const r = await fetch(`${app!.proxyUrl}/v1/completions`, {
            method: "POST",
            headers: reqHeaders,
            body: JSON.stringify({
              model: "completions-legacy-model",
              prompt: "ready-probe",
            }),
          });
          await r.text();
          return r.status === 200;
        } catch {
          return false;
        }
      });

      const baseline = upstream.receivedRequests.length;

      // Asserted call.
      const res = await fetch(`${app.proxyUrl}/v1/completions`, {
        method: "POST",
        headers: reqHeaders,
        body: JSON.stringify({
          model: "completions-legacy-model",
          prompt: PROMPT_TEXT,
          max_tokens: 32,
          temperature: 0.7,
        }),
      });

      // (1) Status 200 — endpoint is wired and reachable.
      expect(res.status).toBe(200);
      const body = (await res.json()) as {
        object?: string;
        choices?: Array<{
          text?: string;
          finish_reason?: string;
        }>;
        usage?: {
          total_tokens?: number;
        };
      };

      // (2) Response shape is text-completion (NOT chat-completion).
      //     A regression that re-routed legacy to the chat handler
      //     would surface `object: "chat.completion"` and the
      //     wrong choices shape.
      expect(body.object).toBe("text_completion");
      expect(body.choices?.[0]?.text).toBe(COMPLETION_TEXT);
      expect(body.choices?.[0]?.finish_reason).toBe("stop");
      expect(body.usage?.total_tokens).toBe(13);

      // (3) Upstream wire-shape: exactly one POST to /v1/completions
      //     with our auth + the prompt + the upstream model id.
      //     Closes the wire-shape blind spot CLAUDE.md §8 calls out.
      const sent = upstream.receivedRequests
        .slice(baseline)
        .filter((r) => r.path === "/v1/completions");
      expect(sent).toHaveLength(1);
      const sentReq = sent[0]!;
      expect(sentReq.method).toBe("POST");
      expect(sentReq.headers.authorization).toBe("Bearer sk-mock");
      const sentBody = JSON.parse(sentReq.body);
      // (4) Model alias is rewritten to the upstream model id.
      expect(sentBody.model).toBe("gpt-3.5-turbo-instruct");
      // (5) `prompt` field is preserved as-is. Catches a regression
      //     that translated legacy `prompt` into chat `messages`
      //     somewhere in the pipeline — which would change every
      //     downstream tool that grep'd request logs for the
      //     `prompt` field.
      expect(sentBody.prompt).toBe(PROMPT_TEXT);
      // Optional params survive verbatim too.
      expect(sentBody.max_tokens).toBe(32);
      expect(sentBody.temperature).toBe(0.7);
      // (6) `messages` field MUST NOT appear — legacy callers do
      //     not send it, and the gateway should not synthesize one
      //     from `prompt`.
      expect(sentBody.messages).toBeUndefined();
    },
    60_000,
  );

  test(
    "POST /v1/completions with stream:true relays the upstream's frames, calling it once",
    async (ctx) => {
      if (!etcdReachable || !app || !streamUpstream) {
        ctx.skip();
        return;
      }

      const baseline = streamUpstream.receivedRequests.length;
      const res = await fetch(`${app.proxyUrl}/v1/completions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${CALLER_PLAINTEXT}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model: "completions-legacy-stream-model",
          prompt: PROMPT_TEXT,
          stream: true,
        }),
      });

      expect(res.status).toBe(200);
      expect(res.headers.get("content-type")).toContain("text/event-stream");
      const frames = (await res.text())
        .split(/\r?\n\r?\n/)
        .map((block) =>
          block
            .split(/\r?\n/)
            .filter((line) => line.startsWith("data:"))
            .map((line) => line.slice("data:".length).trim())
            .join("\n"),
        )
        .filter((data) => data !== "");
      expect(frames.at(-1)).toBe("[DONE]");
      expect(frames.slice(0, -1).map((frame) => JSON.parse(frame) as unknown)).toEqual(STREAM_CHUNKS);

      const sent = streamUpstream.receivedRequests.slice(baseline);
      expect(sent).toHaveLength(1);
      expect(sent[0]!.path).toBe("/v1/completions");
      const forwarded = JSON.parse(sent[0]!.body) as { model?: string; prompt?: string; stream?: boolean };
      expect(forwarded.stream).toBe(true);
      expect(forwarded.prompt).toBe(PROMPT_TEXT);
      expect(forwarded.model).toBe("gpt-3.5-turbo-instruct");
    },
    60_000,
  );

  test(
    "POST /v1/completions without stream still answers 200 with usage",
    async (ctx) => {
      if (!etcdReachable || !app || !upstream) {
        ctx.skip();
        return;
      }

      const reqHeaders = {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      };
      const baseline = upstream.receivedRequests.length;
      const res = await fetch(`${app.proxyUrl}/v1/completions`, {
        method: "POST",
        headers: reqHeaders,
        body: JSON.stringify({
          model: "completions-legacy-model",
          prompt: PROMPT_TEXT,
          stream: false,
        }),
      });

      expect(res.status).toBe(200);
      const body = (await res.json()) as {
        object?: string;
        choices?: Array<{ text?: string }>;
        usage?: { prompt_tokens?: number; completion_tokens?: number; total_tokens?: number };
      };
      expect(body.object).toBe("text_completion");
      expect(body.choices?.[0]?.text).toBe(COMPLETION_TEXT);
      expect(body.usage?.prompt_tokens).toBe(6);
      expect(body.usage?.completion_tokens).toBe(7);
      expect(body.usage?.total_tokens).toBe(13);

      const sent = upstream.receivedRequests
        .slice(baseline)
        .filter((r) => r.path === "/v1/completions");
      expect(sent).toHaveLength(1);
      expect(JSON.parse(sent[0]!.body).stream).toBe(false);
    },
    60_000,
  );
});
