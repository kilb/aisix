import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockControlPlane,
  startOpenAiUpstream,
  waitConfigPropagation,
  type MockControlPlane,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: every usage row a managed gateway delivers to its control plane
// carries what billing prices it by: an id of the gateway's own for the row,
// the provider key and upstream model that served it, and the service tier
// the caller asked for. Driven on /v1/chat/completions (buffered and
// streamed), /v1/messages and /v1/responses, read off the `/dp/telemetry`
// bodies the control plane receives.
//
// The request id is the caller's to choose, and nothing stops two requests
// from sharing one, so it cannot be what tells two rows apart. Two of the
// requests below send the same one.

const CALLER = "sk-usage-billing-fields-e2e";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER).digest("hex");
const SHARED_REQUEST_ID = "dup";

const BUFFERED_UPSTREAM_MODEL = "gpt-4o-2024-08-06";
const STREAMED_UPSTREAM_MODEL = "gpt-4o-mini-2024-07-18";

const CHAT_BODY = {
  id: "chatcmpl-billing",
  object: "chat.completion",
  created: 1,
  model: BUFFERED_UPSTREAM_MODEL,
  choices: [{ index: 0, message: { role: "assistant", content: "hi" }, finish_reason: "stop" }],
  usage: { prompt_tokens: 7, completion_tokens: 3, total_tokens: 10 },
};

const RESPONSES_BODY = {
  id: "resp_billing",
  object: "response",
  model: BUFFERED_UPSTREAM_MODEL,
  status: "completed",
  output: [
    {
      type: "message",
      id: "msg_billing",
      role: "assistant",
      content: [{ type: "output_text", text: "hi" }],
    },
  ],
  usage: { input_tokens: 6, output_tokens: 2, total_tokens: 8 },
};

const chunk = (delta: Record<string, unknown>, finishReason: string | null, usage?: unknown) =>
  JSON.stringify({
    id: "chatcmpl-billing-stream",
    object: "chat.completion.chunk",
    model: STREAMED_UPSTREAM_MODEL,
    choices: [{ index: 0, delta, finish_reason: finishReason }],
    ...(usage === undefined ? {} : { usage }),
  });

/** The fields of a delivered usage row this spec reads. */
interface Row {
  request_id: string;
  requested_model?: string;
  event_id?: string;
  provider_key_id?: string;
  upstream_provider?: string;
  upstream_model?: string;
  requested_service_tier?: string;
  is_batch?: boolean;
}

describe("usage rows carry the fields billing needs", () => {
  let cp: MockControlPlane | undefined;
  let app: SpawnedApp | undefined;
  const upstreams: OpenAiUpstream[] = [];
  let etcdReachable = false;
  let bufferedKeyId = "";
  let streamedKeyId = "";

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    const buffered = await startOpenAiUpstream({
      nonStreamBody: CHAT_BODY,
      pathBodies: { "/v1/chat/completions": CHAT_BODY, "/v1/responses": RESPONSES_BODY },
    });
    const streamed = await startOpenAiUpstream({
      streamEvents: [
        chunk({ role: "assistant", content: "hi" }, null),
        chunk({}, "stop", { prompt_tokens: 4, completion_tokens: 1, total_tokens: 5 }),
        "[DONE]",
      ],
    });
    upstreams.push(buffered, streamed);

    cp = await startMockControlPlane();
    app = await spawnApp({ extra: { managed: cp.managed }, awaitProxyListener: false });
    const seed = new SeedClient(etcd, `${app.etcdPrefix}/${cp.envId}`);

    const bufferedKey = await seed.createProviderKey({
      display_name: "billing-buffered-pk",
      secret: "sk-mock",
      api_base: `${buffered.baseUrl}/v1`,
    });
    const streamedKey = await seed.createProviderKey({
      display_name: "billing-streamed-pk",
      secret: "sk-mock",
      api_base: `${streamed.baseUrl}/v1`,
    });
    bufferedKeyId = bufferedKey.id;
    streamedKeyId = streamedKey.id;
    await seed.createModel({
      display_name: "billing-buffered",
      provider: "openai",
      model_name: BUFFERED_UPSTREAM_MODEL,
      provider_key_id: bufferedKey.id,
    });
    await seed.createModel({
      display_name: "billing-streamed",
      provider: "openai",
      model_name: STREAMED_UPSTREAM_MODEL,
      provider_key_id: streamedKey.id,
    });
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });

    await waitConfigPropagation(async () => {
      try {
        return (await new ProxyClient(app!.proxyUrl, CALLER).listModels()).status === 200;
      } catch {
        // The proxy listener binds once the first configuration applies.
        return false;
      }
    });
  });

  afterAll(async () => {
    await app?.exit();
    await cp?.close();
    for (const u of upstreams) await u.close();
  });

  /** POST `body` as the caller under `requestId`, reading the whole response. */
  const send = async (path: string, requestId: string, body: unknown) => {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
        "x-aisix-request-id": requestId,
      },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    return { status: res.status, text };
  };

  /** Every usage row the control plane has received for `requestIds`. */
  const rowsFor = (requestIds: string[]): Row[] =>
    cp!.telemetryBodies
      .flatMap((body) => (JSON.parse(body) as { events: Row[] }).events)
      .filter((row) => requestIds.includes(row.request_id));

  test("each row has its own event id, names its provider key and upstream model, and the tier asked for", async (ctx) => {
    if (!etcdReachable || !app || !cp) return ctx.skip();
    const messages = [{ role: "user", content: "hello" }];

    const sent = [
      await send("/v1/chat/completions", SHARED_REQUEST_ID, {
        model: "billing-buffered",
        messages,
        service_tier: "flex",
      }),
      await send("/v1/chat/completions", SHARED_REQUEST_ID, {
        model: "billing-streamed",
        messages,
        stream: true,
        stream_options: { include_usage: true },
      }),
      await send("/v1/messages", "billing-messages", {
        model: "billing-buffered",
        max_tokens: 16,
        messages,
        service_tier: "auto",
      }),
      await send("/v1/responses", "billing-responses", {
        model: "billing-buffered",
        input: "hello",
        service_tier: "priority",
      }),
    ];
    for (const res of sent) expect(res.status, res.text).toBe(200);

    const ids = [SHARED_REQUEST_ID, "billing-messages", "billing-responses"];
    await waitConfigPropagation(async () => rowsFor(ids).length >= 4, 30_000);
    const rows = rowsFor(ids);
    expect(rows, JSON.stringify(rows)).toHaveLength(4);

    const one = (requestId: string, model: string): Row => {
      const matching = rows.filter((r) => r.request_id === requestId && r.requested_model === model);
      expect(matching, `${requestId} / ${model}: ${JSON.stringify(rows)}`).toHaveLength(1);
      return matching[0];
    };
    const chat = one(SHARED_REQUEST_ID, "billing-buffered");
    const streamedChat = one(SHARED_REQUEST_ID, "billing-streamed");
    const anthropic = one("billing-messages", "billing-buffered");
    const responses = one("billing-responses", "billing-buffered");

    // An id of the gateway's own on every row: present, never the caller's
    // request id, and different on the two rows that share one.
    for (const row of rows) {
      expect(typeof row.event_id, JSON.stringify(row)).toBe("string");
      expect(row.event_id!.length, JSON.stringify(row)).toBeGreaterThan(0);
      expect(row.event_id, JSON.stringify(row)).not.toBe(row.request_id);
    }
    expect(new Set(rows.map((r) => r.event_id)).size).toBe(rows.length);
    expect(chat.event_id).not.toBe(streamedChat.event_id);

    // The provider key and upstream model each row's request was served by,
    // as seeded.
    for (const row of [chat, anthropic, responses]) {
      expect(row.provider_key_id, JSON.stringify(row)).toBe(bufferedKeyId);
      expect(row.upstream_provider, JSON.stringify(row)).toBe("openai");
      expect(row.upstream_model, JSON.stringify(row)).toBe(BUFFERED_UPSTREAM_MODEL);
    }
    expect(streamedChat.provider_key_id).toBe(streamedKeyId);
    expect(streamedChat.upstream_provider).toBe("openai");
    expect(streamedChat.upstream_model).toBe(STREAMED_UPSTREAM_MODEL);

    // The tier each request asked for, and none where it asked for none.
    expect(chat.requested_service_tier).toBe("flex");
    expect(anthropic.requested_service_tier).toBe("auto");
    expect(responses.requested_service_tier).toBe("priority");
    expect("requested_service_tier" in streamedChat, JSON.stringify(streamedChat)).toBe(false);

    // None of these is a batch row, and a field that does not apply is left
    // off the wire rather than written as `false`.
    for (const row of rows) expect("is_batch" in row, JSON.stringify(row)).toBe(false);
  }, 90_000);
});
