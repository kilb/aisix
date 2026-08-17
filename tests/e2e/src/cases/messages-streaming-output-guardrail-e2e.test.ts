import { createHash, randomUUID } from "node:crypto";
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

// E2E: STREAMING /v1/messages runs output guardrails at end-of-stream
// (#448 #22). The Anthropic passthrough forwards bytes verbatim, so a
// blocked response is signalled with a terminal `error` event (mirroring
// /v1/chat/completions and the common streaming-guardrail pattern). We stream
// Anthropic SSE whose text_delta carries a forbidden token and require
// the response to end with a content_filter error event.

const CALLER = "sk-msgstream-gr-caller";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const FORBIDDEN = "forbiddenstreamtoken";
const STREAM_EVENTS = [
  JSON.stringify({
    type: "message_start",
    message: { id: "msg_s", role: "assistant", content: [], model: "claude-3-5-haiku-20241022", stop_reason: null, usage: { input_tokens: 5, output_tokens: 1 } },
  }),
  JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "here is forbiddenstream" } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "token in the stream" } }),
  JSON.stringify({ type: "content_block_stop", index: 0 }),
  JSON.stringify({ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 12 } }),
  JSON.stringify({ type: "message_stop" }),
];
const CLEAN_STREAM_EVENTS = [
  JSON.stringify({
    type: "message_start",
    message: { id: "msg_clean", role: "assistant", content: [], model: "claude-3-5-haiku-20241022", stop_reason: null, usage: { input_tokens: 5, output_tokens: 1 } },
  }),
  JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "clean streamed reply" } }),
  JSON.stringify({ type: "content_block_stop", index: 0 }),
  JSON.stringify({ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 3 } }),
  JSON.stringify({ type: "message_stop" }),
];

function anthropicSseChunks(events: string[]): string[] {
  return events.map((data) => {
    const { type } = JSON.parse(data) as { type: string };
    return `event: ${type}\r\ndata: ${data}\r\n\r\n`;
  });
}

function parseSseFrames(body: string): Array<{ event: string; data: string }> {
  return body
    .replace(/\r\n?/g, "\n")
    .split("\n\n")
    .filter((block) => block.trim().length > 0)
    .map((block) => {
      let event = "";
      const data: string[] = [];
      for (const line of block.split("\n")) {
        if (line.startsWith("event:")) event = line.slice("event:".length).trimStart();
        if (line.startsWith("data:")) data.push(line.slice("data:".length).trimStart());
      }
      return { event, data: data.join("\n") };
    });
}

describe("streaming /v1/messages output guardrail (#448)", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({
      scriptedResponses: [
        { rawSseChunks: anthropicSseChunks(STREAM_EVENTS), eventDelayMs: 2 },
        { rawSseChunks: anthropicSseChunks(CLEAN_STREAM_EVENTS), eventDelayMs: 2 },
      ],
    });
    app = await spawnApp();
    seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "msgstream-gr-pk",
      secret: "sk-anth-mock",
      api_base: upstream.baseUrl,
    });
    await seed.createModel({
      display_name: "msgstream-gr",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: pk.id,
    });
    await seed.createGuardrail({
      name: "msgstream-gr-output-keyword",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN }],
    });
    await seed.createApiKey({ key_hash: HASH, allowed_models: ["msgstream-gr"] });

    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      if (response.status === 401) {
        await response.text();
        return false;
      }
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      return body.data?.some((model) => model.id === "msgstream-gr") === true;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  const stream = (content: string) =>
    fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER },
      body: JSON.stringify({
        model: "msgstream-gr",
        max_tokens: 64,
        stream: true,
        messages: [{ role: "user", content }],
      }),
    });

  test("a forbidden streamed response ends with a content_filter error event", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const blockedMarker = `blocked-stream-${randomUUID()}`;
    const blockedBaseline = upstream.receivedRequests.length;
    const res = await stream(blockedMarker);
    expect(res.status).toBe(200); // stream starts 200; the block is in-band
    const body = await res.text();
    expect(upstream.receivedRequests).toHaveLength(blockedBaseline + 1);
    expect(upstream.receivedRequests.at(-1)?.body).toContain(blockedMarker);
    // #932 / #466-class: keyword output guardrails carry the BufferFull
    // hold-back policy, so /v1/messages streaming now withholds the whole
    // response until it scans clean — the matched content must NOT reach
    // the wire (pre-fix it was forwarded verbatim before the error frame).
    for (const event of STREAM_EVENTS) {
      expect(body, "hold-back keeps every upstream frame off the wire").not.toContain(event);
    }
    const blockedFrames = parseSseFrames(body);
    expect(blockedFrames).toHaveLength(1);
    expect(blockedFrames[0]?.event).toBe("error");
    expect(JSON.parse(blockedFrames[0]!.data)).toMatchObject({
      type: "error",
      error: { type: "content_filter" },
    });

    const cleanMarker = `clean-stream-${randomUUID()}`;
    const cleanBaseline = upstream.receivedRequests.length;
    const clean = await stream(cleanMarker);
    expect(clean.status).toBe(200);
    const cleanBody = await clean.text();
    expect(upstream.receivedRequests).toHaveLength(cleanBaseline + 1);
    expect(upstream.receivedRequests.at(-1)?.body).toContain(cleanMarker);
    expect(cleanBody).not.toContain("content_filter");
    expect(parseSseFrames(cleanBody)).toEqual(
      CLEAN_STREAM_EVENTS.map((data) => ({
        event: (JSON.parse(data) as { type: string }).type,
        data,
      })),
    );
  });
});
