import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  metricDelta,
  scrapeMetrics,
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

// E2E: a `stream=true` transcription held back for an output guardrail that
// can block or mask.
//
// R-044: the transcript reaches the caller inside SSE events as JSON strings,
// where an escape (`@`) spells the same text with other bytes. The output
// rule must judge, and mask, the text the caller decodes: a block rule refuses
// it, a mask rule rewrites it, framing and usage untouched.
//
// R-094: the hold-back keeps no more than the rule's `max_buffer_bytes` of
// transcript, and past it does what `on_buffer_exceeded` says, as every other
// held stream does: `fail_closed` refuses, `fail_open` releases the
// transcript unscanned and records the bypass on the request's row.
//
// A refusal past the cap still bills what the upstream answered: the
// transcript was produced, and its terminal event reports the tokens whether
// or not the caller gets to read it. So the refused request's row carries the
// upstream's usage, and the upstream counts as having answered, even when the
// cap is crossed frames before that terminal event arrives.

const CALLER = "sk-held-transcription-caller";
const CREDENTIAL_REF = "mock";
const LOGSTORE = "held-transcription";
const hash = (s: string) => createHash("sha256").update(s).digest("hex");
const FRAME_END = "\r\n\r\n";

/** The frames of a streamed transcription, each with its terminator. */
const sseFrames = (text: string, deltas: string[]) =>
  [
    ...deltas.map((delta) => `data: ${JSON.stringify({ type: "transcript.text.delta", delta })}`),
    `data: ${JSON.stringify({
      type: "transcript.text.done",
      text,
      usage: { type: "tokens", input_tokens: 26, output_tokens: 12, total_tokens: 38 },
    })}`,
  ].map((frame) => frame + FRAME_END);
const sse = (text: string, deltas: string[]) => sseFrames(text, deltas).join("");

// The address written as JSON escapes on the wire: no `@` in the bytes.
const escaped = (s: string) => s.replace("@", "\\u0040");
const ESCAPED_SSE = [
  `data: {"type":"transcript.text.delta","delta":"${escaped("mail alice@exam")}"}`,
  `data: {"type":"transcript.text.delta","delta":"ple.com now"}`,
  `data: {"type":"transcript.text.done","text":"${escaped("mail alice@example.com now")}",` +
    `"usage":{"type":"tokens","input_tokens":26,"output_tokens":12,"total_tokens":38}}`,
]
  .join(FRAME_END)
  .concat(FRAME_END);

const CAP = 16;
const LONG_CLEAN = "this transcript runs well past sixteen bytes";
const LONG_WITH_ADDRESS = "this transcript names bob@example.com past the cap";

/** The transcript a caller reads off the stream: every delta, in order. */
function streamedText(body: string): string {
  return body
    .split(/\r?\n\r?\n/)
    .map((frame) => frame.split(/\r?\n/).find((l) => l.startsWith("data:"))?.slice(5).trim())
    .filter((p): p is string => !!p && p !== "[DONE]")
    .map((p) => JSON.parse(p) as { type?: string; delta?: string })
    .filter((e) => e.type === "transcript.text.delta")
    .map((e) => e.delta ?? "")
    .join("");
}

describe("a held streamed transcription is judged decoded and under its buffer cap", () => {
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  const upstreams: OpenAiUpstream[] = [];
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-held-transcription",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });

    // A body is one write, or a list of frames written apart.
    const cells: Array<[string, string | string[], Record<string, unknown>]> = [
      ["held-escaped-block", ESCAPED_SSE, { detectors: [{ type: "email", action: "block" }] }],
      ["held-escaped-mask", ESCAPED_SSE, { detectors: [{ type: "email", action: "mask" }] }],
      [
        // Frame by frame: the first delta crosses the cap, and the terminal
        // event with the usage arrives later.
        "held-cap-closed",
        sseFrames(LONG_CLEAN, [LONG_CLEAN.slice(0, 20), LONG_CLEAN.slice(20)]),
        {
          detectors: [{ type: "email", action: "block" }],
          max_buffer_bytes: CAP,
          on_buffer_exceeded: "fail_closed",
        },
      ],
      [
        "held-cap-open",
        sse(LONG_WITH_ADDRESS, [LONG_WITH_ADDRESS.slice(0, 20), LONG_WITH_ADDRESS.slice(20)]),
        {
          detectors: [{ type: "email", action: "block" }],
          max_buffer_bytes: CAP,
          on_buffer_exceeded: "fail_open",
        },
      ],
    ];
    for (const [name, body, rule] of cells) {
      const upstream = await startOpenAiUpstream(
        Array.isArray(body)
          ? { rawBodyChunks: body, eventDelayMs: 100, rawContentType: "text/event-stream" }
          : { rawBody: body, rawContentType: "text/event-stream" },
      );
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-openai-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      const model = await seed.createModel({
        display_name: name,
        provider: "openai",
        model_name: "gpt-4o-transcribe",
        provider_key_id: pk.id,
      });
      const guard = await seed.createGuardrail(
        { name: `${name}-rule`, enabled: true, kind: "pii", hook_point: "output", ...rule },
        { attach: false },
      );
      await seed.attachGuardrailToModel(guard.id as string, model.id as string);
    }
    // The key last: once it authenticates, the rest is in the snapshot.
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: cells.map(([name]) => name) });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      await res.text();
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    for (const u of upstreams) await u.close();
  });

  const transcribe = async (model: string) => {
    const form = new FormData();
    form.set("model", model);
    form.set("stream", "true");
    form.set("file", new Blob([new Uint8Array([0x49, 0x44, 0x33])], { type: "audio/mpeg" }), "a.mp3");
    const res = await fetch(`${app!.proxyUrl}/v1/audio/transcriptions`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}` },
      body: form,
    });
    return {
      status: res.status,
      body: await res.text(),
      requestId: res.headers.get("x-aisix-request-id") ?? "",
    };
  };

  const rowOf = async (requestId: string) => {
    expect(requestId).not.toBe("");
    return waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      `the usage row of ${requestId}`,
      20_000,
    );
  };

  test("a block rule refuses an address the stream spells with JSON escapes", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await transcribe("held-escaped-block");
    expect(res.status, res.body).toBe(422);
    expect(res.body).not.toContain("alice");
  });

  test("a mask rule rewrites the decoded transcript and keeps the stream's framing and usage", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await transcribe("held-escaped-mask");
    expect(res.status, res.body).toBe(200);
    const text = streamedText(res.body);
    expect(text, "the address reached the caller").not.toContain("alice@example.com");
    expect(text).toBe("mail [EMAIL_REDACTED] now");
    expect(res.body).toContain('"output_tokens":12');
    const row = await rowOf(res.requestId);
    expect(row.get("completion_tokens")).toBe("12");
  });

  test("past max_buffer_bytes, fail_closed refuses the transcript and bills what the upstream reported", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = await scrapeMetrics(app!.metricsUrl);
    const res = await transcribe("held-cap-closed");
    expect(res.status, res.body).toBe(422);
    expect(res.body).toContain("output_buffer_exceeded");
    expect(res.body).not.toContain("sixteen");
    const row = await rowOf(res.requestId);
    expect(row.get("prompt_tokens"), "the upstream's input tokens").toBe("26");
    expect(row.get("completion_tokens"), "the upstream's output tokens").toBe("12");
    // The upstream answered in full; the refusal was the gateway's.
    const after = await scrapeMetrics(app!.metricsUrl);
    const delta = (name: string) => metricDelta(before, after, name, { model: "held-cap-closed" });
    expect(delta("aisix_deployment_success_responses_total")).toBe(1);
    expect(delta("aisix_deployment_failure_responses_total")).toBe(0);
  });

  test("past max_buffer_bytes, fail_open releases it unscanned and records the bypass", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await transcribe("held-cap-open");
    expect(res.status, res.body).toBe(200);
    // Released without an output scan: the block rule never read it.
    expect(streamedText(res.body)).toBe(LONG_WITH_ADDRESS);
    const row = await rowOf(res.requestId);
    expect(row.get("guardrail_bypassed_reason")).toBe("output_buffer_exceeded");
    expect(row.get("completion_tokens"), "the stream's usage is billed").toBe("12");
  });
});
