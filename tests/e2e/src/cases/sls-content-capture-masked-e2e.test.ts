import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  decodedTextFor,
  EtcdClient,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForToken,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// #932 × #947: on a streaming response with a mask-action PII
// guardrail, the wire chunks released to the client are masked — and the
// content handed to a `content_mode = full` exporter must be the SAME masked
// text. Pre-fix, the capture accumulator collected raw deltas and only the
// wire bytes were rewritten, so SLS received PII the client never saw. The
// email value is split across two SSE deltas so only the assembled (capture)
// text ever contains it whole — exactly the shape that leaked.

const CALLER_PLAINTEXT = "sk-content-capture-masked-PLAINTEXT";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const BLOCK_CALLER = `${CALLER_PLAINTEXT}-block`;
const BLOCK_CALLER_KEY_HASH = createHash("sha256").update(BLOCK_CALLER).digest("hex");
const PROVIDER_SECRET = "sk-mock-content-capture-masked";

const CREDENTIAL_REF = "mock";
const SLS_PROJECT = "aisix-e2e-obs";
const FULL_LOGSTORE = "full-events-masked";
const META_LOGSTORE = "meta-events-masked";

const EMAIL = "alice@example.com";
const MASK = "[EMAIL_REDACTED]";
const CHAT_PROMPT_TOKEN = "masked-chat-prompt-tok-1f2e3d";
const BRIDGE_PROMPT_TOKEN = "masked-bridge-prompt-tok-4c5b6a";
const DRAIN_TOKEN = "masked-drain-marker-tok-9a8b7c";
const BLOCK_CHAT_REQUEST_ID = "masked-block-chat-request-7d6e5f";
const BLOCK_BRIDGE_REQUEST_ID = "masked-block-bridge-request-4a3b2c";
const AUDIO_REQUEST_ID = "masked-audio-transcript-request-8e7d6c";
const REFUSAL_REQUEST_ID = "masked-responses-refusal-request-5b4a3f";

/** OpenAI chat chunks with the email split across two deltas (channel
 * reassembly): neither wire chunk carries the whole value; only the
 * assembled text does. */
function chatStreamEvents(marker: string): string[] {
  const split = EMAIL.indexOf("@") + 2; // "alice@e" | "xample.com"
  return [
    JSON.stringify({
      id: "mock-masked-1",
      object: "chat.completion.chunk",
      model: "mock-model",
      choices: [{ index: 0, delta: { role: "assistant" } }],
    }),
    JSON.stringify({
      id: "mock-masked-1",
      object: "chat.completion.chunk",
      model: "mock-model",
      choices: [{ index: 0, delta: { content: `${marker} reach me at ${EMAIL.slice(0, split)}` } }],
    }),
    JSON.stringify({
      id: "mock-masked-1",
      object: "chat.completion.chunk",
      model: "mock-model",
      choices: [
        { index: 0, delta: { content: `${EMAIL.slice(split)} thanks` }, finish_reason: "stop" },
      ],
      usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 },
    }),
    "[DONE]",
  ];
}

async function postJson(
  app: SpawnedApp,
  path: string,
  body: unknown,
  requestId?: string,
): Promise<Response> {
  return fetch(`${app.proxyUrl}${path}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CALLER_PLAINTEXT}`,
      "content-type": "application/json",
      ...(requestId ? { "x-aisix-request-id": requestId } : {}),
    },
    body: JSON.stringify(body),
  });
}

describe("sls content capture e2e: streaming capture is post-mask (#932 × #947)", () => {
  let etcdReachable = false;
  let chatUpstream: OpenAiUpstream | undefined;
  let bridgeUpstream: OpenAiUpstream | undefined;
  let audioUpstream: OpenAiUpstream | undefined;
  let refusalUpstream: OpenAiUpstream | undefined;
  let sls: MockSls | undefined;
  const apps: SpawnedApp[] = [];

  beforeAll(async () => {
    etcdReachable = await new EtcdClient().ping();
    if (!etcdReachable) return;
    chatUpstream = await startOpenAiUpstream({ streamEvents: chatStreamEvents("chat-reply") });
    // The /v1/responses cross-provider bridge consumes OpenAI chat chunks too.
    bridgeUpstream = await startOpenAiUpstream({ streamEvents: chatStreamEvents("bridge-reply") });
    audioUpstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "chatcmpl-audio-dlp",
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: "gpt-audio",
        choices: [
          {
            index: 0,
            message: {
              role: "assistant",
              content: null,
              audio: {
                id: "audio-dlp",
                data: "UklGRiQAAABXQVZF",
                transcript: `call ${EMAIL}`,
                expires_at: 1_900_000_000,
              },
            },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 },
      },
    });
    refusalUpstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "resp-refusal-dlp",
        object: "response",
        created_at: Math.floor(Date.now() / 1000),
        model: "gpt-4o-mini",
        status: "completed",
        output: [
          {
            id: "msg-refusal-dlp",
            type: "message",
            role: "assistant",
            status: "completed",
            content: [{ type: "refusal", refusal: `contact ${EMAIL}` }],
          },
        ],
        usage: { input_tokens: 5, output_tokens: 3, total_tokens: 8 },
      },
    });
    sls = await startMockSls();
  });

  afterAll(async () => {
    await Promise.all(apps.map((a) => a.exit()));
    await chatUpstream?.close();
    await bridgeUpstream?.close();
    await audioUpstream?.close();
    await refusalUpstream?.close();
    await sls?.close();
  });

  test(
    "full-capture exporter receives the masked stream text, never the raw PII",
    async (ctx) => {
      if (
        !etcdReachable ||
        !chatUpstream ||
        !bridgeUpstream ||
        !audioUpstream ||
        !refusalUpstream ||
        !sls
      ) {
        ctx.skip();
        return;
      }
      const app = await spawnApp({
        extraEnv: {
          [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
          [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
        },
      });
      apps.push(app);
      const seed = new SeedClient(new EtcdClient(), app.etcdPrefix);
      await seed.createObservabilityExporter({
        name: "sls-full-masked",
        enabled: true,
        kind: "aliyun_sls",
        endpoint: sls.url,
        project: SLS_PROJECT,
        logstore: FULL_LOGSTORE,
        credential_ref: CREDENTIAL_REF,
        content_mode: "full",
      });
      await seed.createObservabilityExporter({
        name: "sls-meta-masked",
        enabled: true,
        kind: "aliyun_sls",
        endpoint: sls.url,
        project: SLS_PROJECT,
        logstore: META_LOGSTORE,
        credential_ref: CREDENTIAL_REF,
        content_mode: "metadata_only",
      });
      const chatPk = await seed.createProviderKey({
        display_name: "masked-chat-pk",
        secret: PROVIDER_SECRET,
        api_base: `${chatUpstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: "masked-chat-model",
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: chatPk.id,
      });
      const audioPk = await seed.createProviderKey({
        display_name: "masked-audio-pk",
        secret: PROVIDER_SECRET,
        api_base: `${audioUpstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: "masked-audio-model",
        provider: "openai",
        model_name: "gpt-audio",
        provider_key_id: audioPk.id,
      });
      const refusalPk = await seed.createProviderKey({
        display_name: "masked-refusal-pk",
        secret: PROVIDER_SECRET,
        api_base: `${refusalUpstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: "masked-refusal-model",
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: refusalPk.id,
      });
      const bridgePk = await seed.createProviderKey({
        display_name: "masked-bridge-pk",
        secret: PROVIDER_SECRET,
        api_base: `${bridgeUpstream.baseUrl}/v1`,
        provider: "deepseek",
        adapter: "openai",
      });
      await seed.createModel({
        display_name: "masked-bridge-model",
        provider: "deepseek",
        model_name: "gpt-4o-mini",
        provider_key_id: bridgePk.id,
      });
      await seed.createApiKey({
        key_hash: CALLER_KEY_HASH,
        allowed_models: [
          "masked-chat-model",
          "masked-bridge-model",
          "masked-audio-model",
          "masked-refusal-model",
        ],
      });
      // Env-wide output-hook PII guardrail: email → mask. Its presence also
      // forces the streaming hold-back (BufferFull) path — the path where
      // the capture/wire divergence lived.
      await seed.createGuardrail({
        name: "masked-capture-guard",
        enabled: true,
        hook_point: "output",
        kind: "pii",
        detectors: [{ type: "email", action: "mask" }],
      });

      await waitConfigPropagation(async () => {
        try {
          const r = await postJson(app, "/v1/chat/completions", {
            model: "masked-chat-model",
            stream: true,
            messages: [{ role: "user", content: "warmup" }],
          });
          const body = await r.text();
          // The guardrail must be live too, not just the model: wait until
          // the masked form appears in the wire response.
          return r.status === 200 && body.includes(MASK);
        } catch {
          return false;
        }
      });

      // Everything above is warm-up: those requests were served while the
      // guardrail was still propagating, so the exporter shipped their
      // UNMASKED replies — which is correct behaviour for a gateway that had
      // no masking rule yet, and not what this test is about. Assert only on
      // what was exported from here on.
      //
      // An index boundary alone would not separate them: the sink buffers
      // records and flushes on a 5s tick, so a warm-up record still sitting
      // in that buffer would ship in the SAME PutLogs body as the requests
      // below. Send one marker request — masked, since the guardrail is live
      // now — and wait for it to arrive. The sink flushes its buffer in
      // order, so the marker being visible proves every warm-up record has
      // already shipped, and only then does the boundary mean anything.
      await (
        await postJson(app, "/v1/chat/completions", {
          model: "masked-chat-model",
          stream: true,
          messages: [{ role: "user", content: `note ${DRAIN_TOKEN}` }],
        })
      ).text();
      await waitForToken(sls, FULL_LOGSTORE, DRAIN_TOKEN, 20_000);
      const afterWarmup = sls.requests.length;

      // -- streaming chat (hold-back release path) --
      const chatRes = await postJson(app, "/v1/chat/completions", {
        model: "masked-chat-model",
        stream: true,
        messages: [{ role: "user", content: `note ${CHAT_PROMPT_TOKEN}` }],
      });
      expect(chatRes.status).toBe(200);
      const chatBody = await chatRes.text();
      expect(chatBody).toContain(MASK); // client got masked text
      expect(chatBody).not.toContain(EMAIL);

      await waitForToken(sls, FULL_LOGSTORE, CHAT_PROMPT_TOKEN, 10_000, afterWarmup);
      let fullText = decodedTextFor(sls, FULL_LOGSTORE, afterWarmup);
      expect(fullText).toContain(CHAT_PROMPT_TOKEN);
      expect(fullText).toContain(MASK); // capture carries the masked reply
      expect(fullText).not.toContain(EMAIL); // and never the raw PII

      // -- streaming /v1/responses via the cross-provider bridge --
      const bridgeRes = await postJson(app, "/v1/responses", {
        model: "masked-bridge-model",
        stream: true,
        input: `note ${BRIDGE_PROMPT_TOKEN}`,
      });
      expect(bridgeRes.status).toBe(200);
      const bridgeBody = await bridgeRes.text();
      expect(bridgeBody).toContain(MASK);
      expect(bridgeBody).not.toContain(EMAIL);

      await waitForToken(sls, FULL_LOGSTORE, BRIDGE_PROMPT_TOKEN, 10_000, afterWarmup);
      fullText = decodedTextFor(sls, FULL_LOGSTORE, afterWarmup);
      expect(fullText).toContain(BRIDGE_PROMPT_TOKEN);
      expect(fullText).not.toContain(EMAIL);

      // Chat audio is one indivisible sensitive output: changing only its
      // transcript would leave the spoken PII in `audio.data`, so a mask hit
      // fails closed and neither wire nor exporter receives it.
      const audioRes = await postJson(
        app,
        "/v1/chat/completions",
        {
          model: "masked-audio-model",
          messages: [{ role: "user", content: "generate clean audio" }],
          modalities: ["text", "audio"],
          audio: { voice: "alloy", format: "wav" },
        },
        AUDIO_REQUEST_ID,
      );
      expect(audioRes.status).toBe(422);
      expect(await audioRes.text()).not.toContain(EMAIL);

      // Responses refusal text is ordinary caller-visible model output and
      // is safe to rewrite in place.
      const refusalRes = await postJson(
        app,
        "/v1/responses",
        { model: "masked-refusal-model", input: "clean refusal request" },
        REFUSAL_REQUEST_ID,
      );
      expect(refusalRes.status).toBe(200);
      const refusalBody = await refusalRes.text();
      expect(refusalBody).toContain(MASK);
      expect(refusalBody).not.toContain(EMAIL);

      await waitForToken(sls, FULL_LOGSTORE, AUDIO_REQUEST_ID, 10_000, afterWarmup);
      await waitForToken(sls, FULL_LOGSTORE, REFUSAL_REQUEST_ID, 10_000, afterWarmup);
      fullText = decodedTextFor(sls, FULL_LOGSTORE, afterWarmup);
      expect(fullText).toContain(MASK);
      expect(fullText).not.toContain(EMAIL);

      // metadata_only exporter never sees content at all.
      const metaText = decodedTextFor(sls, META_LOGSTORE, afterWarmup);
      expect(metaText).not.toContain(EMAIL);
      expect(metaText).not.toContain(MASK);
      expect(metaText).not.toContain(CHAT_PROMPT_TOKEN);

      // Add a block-action rule, then place a new authenticated API key after
      // it in the ordered config stream. Exact model discovery through that
      // key is the revision barrier; the requests below do not double as
      // readiness probes for the behavior they verify.
      await seed.createGuardrail({
        name: "blocked-capture-guard",
        enabled: true,
        hook_point: "output",
        kind: "pii",
        detectors: [{ type: "email", action: "block" }],
      });
      await seed.createApiKey({
        key_hash: BLOCK_CALLER_KEY_HASH,
        allowed_models: [
          "masked-chat-model",
          "masked-bridge-model",
          "masked-audio-model",
          "masked-refusal-model",
        ],
      });
      await waitConfigPropagation(async () => {
        const response = await fetch(`${app.proxyUrl}/v1/models`, {
          headers: { authorization: `Bearer ${BLOCK_CALLER}` },
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
        return (
          ids.size === 4 &&
          ids.has("masked-chat-model") &&
          ids.has("masked-bridge-model") &&
          ids.has("masked-audio-model") &&
          ids.has("masked-refusal-model")
        );
      });

      // The previous bridge marker was awaited above, so the exporter has
      // flushed every earlier record. Everything after this boundary belongs
      // to the two blocked streams.
      const beforeBlocked = sls.requests.length;
      const blockedPost = (path: string, requestId: string, body: unknown) =>
        fetch(`${app.proxyUrl}${path}`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${BLOCK_CALLER}`,
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify(body),
        });

      const blockedChat = await blockedPost(
        "/v1/chat/completions",
        BLOCK_CHAT_REQUEST_ID,
        {
          model: "masked-chat-model",
          stream: true,
          messages: [{ role: "user", content: "blocked chat capture" }],
        },
      );
      expect(blockedChat.status).toBe(200);
      const blockedChatBody = await blockedChat.text();
      expect(blockedChatBody).toContain("content_filter");
      expect(blockedChatBody).not.toContain(EMAIL);

      const blockedBridge = await blockedPost(
        "/v1/responses",
        BLOCK_BRIDGE_REQUEST_ID,
        {
          model: "masked-bridge-model",
          stream: true,
          input: "blocked bridge capture",
        },
      );
      expect(blockedBridge.status).toBe(200);
      const blockedBridgeBody = await blockedBridge.text();
      expect(blockedBridgeBody).toContain("content_filter");
      expect(blockedBridgeBody).not.toContain(EMAIL);

      await waitForToken(
        sls,
        FULL_LOGSTORE,
        BLOCK_CHAT_REQUEST_ID,
        10_000,
        beforeBlocked,
      );
      await waitForToken(
        sls,
        FULL_LOGSTORE,
        BLOCK_BRIDGE_REQUEST_ID,
        10_000,
        beforeBlocked,
      );
      const blockedFullText = decodedTextFor(sls, FULL_LOGSTORE, beforeBlocked);
      expect(blockedFullText).toContain(BLOCK_CHAT_REQUEST_ID);
      expect(blockedFullText).toContain(BLOCK_BRIDGE_REQUEST_ID);
      expect(blockedFullText).not.toContain(EMAIL);
      expect(blockedFullText).not.toContain("chat-reply");
      expect(blockedFullText).not.toContain("bridge-reply");
    },
    120_000,
  );
});
