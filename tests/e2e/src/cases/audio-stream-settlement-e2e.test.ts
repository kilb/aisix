import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  metricDelta,
  scrapeMetrics,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForLogLine,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a relayed audio stream — synthesized speech, a streamed transcription
// — is settled by how it ENDS, not by its response head.
//
//   - An upstream that stalls after the head went out ends on the model's
//     stream read timeout. That is the upstream failing the request: the
//     model goes into cooldown, exactly as the same timeout before the head
//     does. Settling at the head recorded it healthy instead.
//   - A transcription's `transcript.text.done` is its documented terminal
//     event. A caller that closes on it, while the upstream has not closed
//     its body yet, received the whole transcript: the request is a 200, not
//     a client abandonment (499).

const CALLER = "sk-audio-stream-settlement";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const READ_TIMEOUT_MS = 300;
const STALL_MS = 3_000;
// A held transcription is bounded by the request budget only until it is
// handed to the live relay; a healthy relay is bounded per frame instead.
const REQUEST_TIMEOUT_MS = 400;
const STREAM_TIMEOUT_MS = 1_000;
const FRAME_GAP_MS = 250;
const HOLD_CAP_BYTES = 16;

const delta = (text: string) =>
  `data: ${JSON.stringify({ type: "transcript.text.delta", delta: text })}\n\n`;
const DONE_EVENT = `data: ${JSON.stringify({
  type: "transcript.text.done",
  text: "hello world",
  usage: { type: "tokens", input_tokens: 4, output_tokens: 2, total_tokens: 6 },
})}\n\n`;

let app: SpawnedApp | undefined;

async function modelStatus(
  modelId: string,
): Promise<{ status?: string; status_reason?: string } | undefined> {
  const response = await fetch(`${app!.metricsUrl}/status/models`);
  if (response.status !== 200) {
    throw new Error(`model status probe returned ${response.status}`);
  }
  const rows = (await response.json()) as Array<{
    id?: string;
    status?: string;
    status_reason?: string;
  }>;
  return rows.find((row) => row.id === modelId);
}

function transcription(model: string): Promise<Response> {
  const form = new FormData();
  form.set("model", model);
  form.set("stream", "true");
  form.set("file", new Blob([new Uint8Array([0x49, 0x44, 0x33])], { type: "audio/mpeg" }), "a.mp3");
  return fetch(`${app!.proxyUrl}/v1/audio/transcriptions`, {
    method: "POST",
    headers: { authorization: `Bearer ${CALLER}` },
    body: form,
  });
}

function speech(model: string): Promise<Response> {
  return fetch(`${app!.proxyUrl}/v1/audio/speech`, {
    method: "POST",
    headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
    body: JSON.stringify({ model, input: "Hello there.", voice: "alloy" }),
  });
}

/** Read a committed body to its end; a cut-off body may end in an error. */
async function drain(res: Response): Promise<string> {
  const reader = res.body!.getReader();
  const decoder = new TextDecoder();
  let body = "";
  try {
    for (;;) {
      const next = await reader.read();
      if (next.done) break;
      body += decoder.decode(next.value, { stream: true });
    }
  } catch {
    // The relay ended the body on the timeout.
  }
  return body + decoder.decode();
}

describe("relayed audio streams are settled by how they end", () => {
  const upstreams: OpenAiUpstream[] = [];
  let speechStallId = "";
  let transcriptStallId = "";
  let overCapOpenId = "";
  let overCapStalledId = "";
  let overCapCompleteId = "";
  let overCapReadErrorId = "";
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    const speechStall = await startOpenAiUpstream({
      rawBodyChunks: ["ID3first-part", "never-in-time"],
      rawContentType: "audio/mpeg",
      eventDelayMs: STALL_MS,
    });
    const transcriptStall = await startOpenAiUpstream({
      rawStreamFrames: [delta("hello"), delta(" world"), DONE_EVENT],
      eventDelayMs: STALL_MS,
    });
    // The terminal event goes out, then the upstream keeps its body open for
    // seconds more, writing only SSE comments: the relay forwards them, and
    // the writes to a caller that has gone are what end the relay while the
    // upstream is still open.
    const transcriptDoneThenOpen = await startOpenAiUpstream({
      rawStreamFrames: [
        delta("hello world"),
        DONE_EVENT,
        ...Array.from({ length: 30 }, () => ": keep-alive\n\n"),
        "data: [DONE]\n\n",
      ],
      eventDelayMs: 300,
    });
    // A transcript that outgrows a small hold cap on its first frame and then
    // keeps producing for longer than the request budget, well inside the
    // stream budget between any two frames.
    const overCapStream = async () => {
      const upstream = await startOpenAiUpstream({
        rawStreamFrames: [
          delta("twenty clean letters cross the cap"),
          delta(" then still producing"),
          delta(" and still more"),
          DONE_EVENT,
        ],
        eventDelayMs: FRAME_GAP_MS,
      });
      upstreams.push(upstream);
      return upstream;
    };
    const overCapOpen = await overCapStream();
    const overCapStalled = await startOpenAiUpstream({
      rawStreamFrames: [delta("twenty clean letters cross the cap"), DONE_EVENT],
      eventDelayMs: STALL_MS,
    });
    // The cap-crossing frame lands and is read, then the connection drops:
    // the drain that follows the refusal reads an error rather than the
    // upstream's terminal event. The delay is what puts the drop AFTER the
    // crossing frame — without it the socket closes before the gateway has
    // read anything, which is an ordinary upstream failure, not a drain.
    const overCapReadError = await startOpenAiUpstream({
      rawStreamFrames: [
        delta("twenty clean letters cross the cap"),
        delta(" then gone"),
      ],
      eventDelayMs: 250,
      disconnectAfterEvents: 1,
    });
    const overCapComplete = await startOpenAiUpstream({
      rawStreamFrames: [
        delta("twenty clean letters cross the cap"),
        delta(" then finished"),
        DONE_EVENT,
      ],
      eventDelayMs: 20,
    });
    upstreams.push(overCapStalled, overCapReadError, overCapComplete);

    upstreams.push(speechStall, transcriptStall, transcriptDoneThenOpen);

    // The access-log line is `tracing::info!`.
    app = await spawnApp({ logLevel: "info" });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = async (name: string, upstream: OpenAiUpstream) =>
      (
        await seed.createProviderKey({
          display_name: `${name}-pk`,
          secret: "sk-mock",
          api_base: `${upstream.baseUrl}/v1`,
        })
      ).id;
    speechStallId = (
      await seed.createModel({
        display_name: "settle-speech-stall",
        provider: "openai",
        model_name: "tts-1",
        provider_key_id: await pk("settle-speech-stall", speechStall),
        stream_timeout: READ_TIMEOUT_MS,
        retries: 0,
        cooldown: { enabled: true, default_seconds: 60 },
      })
    ).id;
    transcriptStallId = (
      await seed.createModel({
        display_name: "settle-transcript-stall",
        provider: "openai",
        model_name: "gpt-4o-transcribe",
        provider_key_id: await pk("settle-transcript-stall", transcriptStall),
        stream_timeout: READ_TIMEOUT_MS,
        retries: 0,
        cooldown: { enabled: true, default_seconds: 60 },
      })
    ).id;
    await seed.createModel({
      display_name: "settle-transcript-done",
      provider: "openai",
      model_name: "gpt-4o-transcribe",
      provider_key_id: await pk("settle-transcript-done", transcriptDoneThenOpen),
    });
    // One hold cap, two overflow policies, and the same upstream twice: the
    // overflow under `fail_open` hands the rest to the live relay, under
    // `fail_closed` the gateway refuses and drains it for its counters.
    const heldModel = async (
      name: string,
      upstream: OpenAiUpstream,
      policy: "fail_open" | "fail_closed",
    ) => {
      const model = await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-transcribe",
        provider_key_id: await pk(name, upstream),
        timeout: REQUEST_TIMEOUT_MS,
        stream_timeout: STREAM_TIMEOUT_MS,
        retries: 0,
        cooldown: { enabled: true, default_seconds: 60 },
      });
      const guard = await seed.createGuardrail(
        {
          name: `${name}-rule`,
          enabled: true,
          kind: "pii",
          hook_point: "output",
          detectors: [{ type: "email", action: "block" }],
          max_buffer_bytes: HOLD_CAP_BYTES,
          on_buffer_exceeded: policy,
        },
        { attach: false },
      );
      await seed.attachGuardrailToModel(guard.id, model.id);
      return model.id;
    };
    overCapOpenId = await heldModel("settle-over-cap-open", overCapOpen, "fail_open");
    overCapStalledId = await heldModel("settle-over-cap-stalled", overCapStalled, "fail_closed");
    overCapCompleteId = await heldModel("settle-over-cap-complete", overCapComplete, "fail_closed");
    overCapReadErrorId = await heldModel("settle-over-cap-read-error", overCapReadError, "fail_closed");

    // Seeded last: once it authenticates, every resource above is live.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: [
        "settle-speech-stall",
        "settle-transcript-stall",
        "settle-transcript-done",
        "settle-over-cap-open",
        "settle-over-cap-stalled",
        "settle-over-cap-complete",
        "settle-over-cap-read-error",
      ],
    });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((u) => u.close()));
  });

  test("speech that stalls after its head times out and puts the model in cooldown", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await speech("settle-speech-stall");
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    const body = await drain(res);
    expect(body).toContain("ID3first-part");
    expect(body).not.toContain("never-in-time");
    const line = await waitForLogLine(
      app,
      (l) => l.includes(`request_id="${requestId}"`) && l.includes("proxy request completed"),
      `the access-log line for ${requestId}`,
    );
    expect(line).toContain("status_code=504");
    await waitConfigPropagation(async () => {
      const row = await modelStatus(speechStallId);
      return row?.status === "cooldown" && row.status_reason === "request_timeout";
    });
  });

  test("a streamed transcription that stalls after its head times out and puts the model in cooldown", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await transcription("settle-transcript-stall");
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    const body = await drain(res);
    expect(body).toContain("hello");
    expect(body).not.toContain("transcript.text.done");
    const line = await waitForLogLine(
      app,
      (l) => l.includes(`request_id="${requestId}"`) && l.includes("proxy request completed"),
      `the access-log line for ${requestId}`,
    );
    expect(line).toContain("status_code=504");
    await waitConfigPropagation(async () => {
      const row = await modelStatus(transcriptStallId);
      return row?.status === "cooldown" && row.status_reason === "request_timeout";
    });
  });

  test("a caller that closes on transcript.text.done received the response: a 200, not a 499", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const res = await transcription("settle-transcript-done");
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    const reader = res.body!.getReader();
    const decoder = new TextDecoder();
    let seen = "";
    while (!seen.includes("transcript.text.done")) {
      const next = await reader.read();
      if (next.done) break;
      seen += decoder.decode(next.value, { stream: true });
    }
    expect(seen).toContain("transcript.text.done");
    // The caller has the whole transcript and hangs up; the upstream has not
    // closed its body yet.
    await reader.cancel();
    const line = await waitForLogLine(
      app,
      (l) => l.includes(`request_id="${requestId}"`) && l.includes("proxy request completed"),
      `the access-log line for ${requestId}`,
    );
    expect(line).toContain("status_code=200");
  });

  test("a held transcript that overflows under fail_open outlives the request budget and completes", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const before = await scrapeMetrics(app.metricsUrl);
    const started = Date.now();
    const res = await transcription("settle-over-cap-open");
    expect(res.status).toBe(200);
    const body = await drain(res);
    const elapsed = Date.now() - started;

    // Every frame reached the caller, the terminal one included, and the
    // whole request took longer than the request budget.
    expect(body).toContain("twenty clean letters cross the cap");
    expect(body).toContain("then still producing");
    expect(body).toContain("transcript.text.done");
    expect(elapsed).toBeGreaterThan(REQUEST_TIMEOUT_MS);
    // A healthy relay is not the upstream failing: no cooldown, no failure.
    const row = await modelStatus(overCapOpenId);
    expect(row?.status).not.toBe("cooldown");
    const after = await scrapeMetrics(app.metricsUrl);
    expect(
      metricDelta(before, after, "aisix_deployment_success_responses_total", {
        model: "settle-over-cap-open",
      }),
    ).toBe(1);
    expect(
      metricDelta(before, after, "aisix_deployment_failure_responses_total", {
        model: "settle-over-cap-open",
      }),
    ).toBe(0);
  }, 30_000);

  test("a refused transcript whose drain times out settles as the upstream failing, not as a success", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const before = await scrapeMetrics(app.metricsUrl);
    const res = await transcription("settle-over-cap-stalled");

    // The refusal is still the gateway's local one.
    expect(res.status, await res.clone().text()).toBe(422);
    expect(await res.text()).toContain("output_buffer_exceeded");
    // ...but the upstream never finished the answer the gateway was reading
    // past the cap, and that is what its health and the counters say.
    await waitConfigPropagation(async () => {
      const row = await modelStatus(overCapStalledId);
      return row?.status === "cooldown" && row.status_reason === "request_timeout";
    });
    const after = await scrapeMetrics(app.metricsUrl);
    expect(
      metricDelta(before, after, "aisix_deployment_failure_responses_total", {
        model: "settle-over-cap-stalled",
      }),
    ).toBe(1);
    expect(
      metricDelta(before, after, "aisix_deployment_success_responses_total", {
        model: "settle-over-cap-stalled",
      }),
    ).toBe(0);
  }, 30_000);

  test("a refused transcript the upstream finished settles as a success, as before", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const before = await scrapeMetrics(app.metricsUrl);
    const res = await transcription("settle-over-cap-complete");

    expect(res.status, await res.clone().text()).toBe(422);
    expect(await res.text()).toContain("output_buffer_exceeded");
    const row = await modelStatus(overCapCompleteId);
    expect(row?.status).not.toBe("cooldown");
    const after = await scrapeMetrics(app.metricsUrl);
    expect(
      metricDelta(before, after, "aisix_deployment_success_responses_total", {
        model: "settle-over-cap-complete",
      }),
    ).toBe(1);
    expect(
      metricDelta(before, after, "aisix_deployment_failure_responses_total", {
        model: "settle-over-cap-complete",
      }),
    ).toBe(0);
  }, 30_000);

  test("a refused transcript whose drain hits a read error settles as a failure", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const before = await scrapeMetrics(app!.metricsUrl);
    const res = await transcription("settle-over-cap-read-error");

    // The caller's contract is unchanged: the gateway's own refusal.
    expect(res.status, await res.clone().text()).toBe(422);
    expect(await res.text()).toContain("output_buffer_exceeded");
    // The upstream never finished the answer: a failure, never a success.
    // (A read error on a 2xx body is not a health signal — `note_failure`
    // leaves the cooldown alone — so the deployment counters are what this
    // case can hold the settlement to.)
    await waitConfigPropagation(async () => {
      const after = await scrapeMetrics(app!.metricsUrl);
      return (
        metricDelta(before, after, "aisix_deployment_failure_responses_total", {
          model: "settle-over-cap-read-error",
        }) === 1
      );
    });
    expect((await modelStatus(overCapReadErrorId))?.status).not.toBe("cooldown");
    const after = await scrapeMetrics(app!.metricsUrl);
    expect(
      metricDelta(before, after, "aisix_deployment_success_responses_total", {
        model: "settle-over-cap-read-error",
      }),
    ).toBe(0);
  }, 30_000);
});
