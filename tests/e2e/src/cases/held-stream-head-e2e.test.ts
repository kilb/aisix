import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type OpenAiUpstream,
  type OpenAiUpstreamOptions,
  type SpawnedApp,
} from "../harness/index.js";

// A long hold behind anything with an idle timeout must not look like a dead
// connection: a held /v1/responses or /v1/completions stream sends its head and
// heartbeats while the answer is judged, as chat and translated /v1/responses
// do. So a refusal is a 200 with failure frames, and a request refused before
// any hold began keeps its real status.
const CALLER_PLAINTEXT = "sk-held-stream-head-PLAINTEXT";
const CALLER_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const CREDENTIAL_REF = "mock";
const SLS_PROJECT = "aisix-e2e-obs";
const LOGSTORE = "held-stream-head";
const FORBIDDEN = "heldheadsentinel";
const INPUT_FORBIDDEN = "heldheadinputsentinel";
const HEARTBEAT_INTERVAL_S = 1;
/** Long enough for several heartbeats, split across frames so none can release early. */
const EVENT_DELAY_MS = 900;
const EVENTS = 4;
const HOLD_MS = EVENT_DELAY_MS * EVENTS;

const completionFrames = (parts: string[]): OpenAiUpstreamOptions => ({
  eventDelayMs: EVENT_DELAY_MS,
  streamEvents: [
    ...parts.map((text) =>
      JSON.stringify({
        id: "cmpl-held-head",
        object: "text_completion",
        created: 0,
        model: "upstream-model",
        choices: [{ index: 0, text, finish_reason: null }],
      }),
    ),
    "[DONE]",
  ],
});

const responseFrames = (parts: string[]): OpenAiUpstreamOptions => ({
  eventDelayMs: EVENT_DELAY_MS,
  rawStreamFrames: [
    ...parts.map(
      (delta) =>
        `event: response.output_text.delta\ndata: ${JSON.stringify({
          type: "response.output_text.delta",
          delta,
        })}\n\n`,
    ),
    `event: response.completed\ndata: ${JSON.stringify({
      type: "response.completed",
      response: {
        id: "resp_held_head",
        model: "upstream-model",
        output: [
          {
            type: "message",
            role: "assistant",
            content: [{ type: "output_text", text: parts.join("") }],
          },
        ],
        usage: { input_tokens: 31, output_tokens: 17, total_tokens: 48 },
      },
    })}\n\n`,
  ],
});

const CLEAN = ["a perfectly ", "clean ", "answer", "."];
const REFUSED = ["say ", FORBIDDEN.slice(0, 8), FORBIDDEN.slice(8), " now"];

const MODELS: Record<string, OpenAiUpstreamOptions> = {
  "hh-completions-clean": completionFrames(CLEAN),
  "hh-completions-refused": completionFrames(REFUSED),
  "hh-responses-clean": responseFrames(CLEAN),
  "hh-responses-refused": responseFrames(REFUSED),
};

const REQUESTS = {
  completions: (model: string, prompt = "go") => ({
    path: "/v1/completions",
    body: { model, prompt, stream: true },
  }),
  responses: (model: string, input = "go") => ({
    path: "/v1/responses",
    body: { model, input, stream: true },
  }),
};

describe("a held stream sends its head and heartbeats during the hold", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  const upstreams = new Map<string, OpenAiUpstream>();
  /** Answers a whole completion, `[DONE]` included, and never ends the body. */
  let openAfterDone: Server | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    app = await spawnApp({
      extra: { downstream: { sse_keepalive_interval_secs: HEARTBEAT_INTERVAL_S } },
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-held-head",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: SLS_PROJECT,
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });
    await seed.createGuardrail({
      name: "hh-output-block",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN }],
    });
    await seed.createGuardrail({
      name: "hh-input-block",
      enabled: true,
      hook_point: "input",
      kind: "keyword",
      patterns: [{ kind: "literal", value: INPUT_FORBIDDEN }],
    });
    for (const [model, options] of [
      ...Object.entries(MODELS),
      // An upstream that refuses the request outright, for both endpoints.
      [
        "hh-upstream-refuses",
        { status: 400, errorBody: { error: { message: "bad", type: "invalid_request_error" } } },
      ] as const,
    ] as Array<[string, OpenAiUpstreamOptions]>) {
      const upstream = await startOpenAiUpstream(options);
      upstreams.set(model, upstream);
      const pk = await seed.createProviderKey({
        display_name: `${model}-pk`,
        secret: "sk-mock",
        api_base: `${upstream.baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: model,
        provider: "openai",
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
    }
    openAfterDone = createServer((req, res) => {
      req.resume();
      req.on("end", () => {
        res.writeHead(200, { "content-type": "text/event-stream" });
        res.write(
          `data: ${JSON.stringify({
            id: "cmpl-open",
            object: "text_completion",
            created: 0,
            model: "upstream-model",
            choices: [{ index: 0, text: "a whole answer", finish_reason: "stop" }],
          })}\n\ndata: [DONE]\n\n`,
        );
      });
    });
    await new Promise<void>((resolve) => openAfterDone!.listen(0, "127.0.0.1", resolve));
    const openPort = (openAfterDone.address() as { port: number }).port;
    const openPk = await seed.createProviderKey({
      display_name: "hh-completions-open-pk",
      secret: "sk-mock",
      api_base: `http://127.0.0.1:${openPort}/v1`,
    });
    await seed.createModel({
      display_name: "hh-completions-open",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: openPk.id,
    });
    // Caller key last: it doubles as the propagation barrier for all of it.
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      return res.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await Promise.all([...upstreams.values()].map((u) => u.close()));
    openAfterDone?.closeAllConnections();
    openAfterDone?.close();
  });

  const ready = () => etcdReachable && !!app && !!sls;

  function post(path: string, body: Record<string, unknown>): Promise<Response> {
    return fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    });
  }

  /** The body, and how long into the stream its first real frame came. */
  async function read(res: Response, started: number) {
    const reader = res.body!.getReader();
    const decoder = new TextDecoder();
    let body = "";
    let firstFrameAt: number | undefined;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return { body, firstFrameAt };
      body += decoder.decode(value, { stream: true });
      if (firstFrameAt === undefined && body.replaceAll(":\n\n", "").length > 0) {
        firstFrameAt = Date.now() - started;
      }
    }
  }

  const heartbeatsAhead = (body: string, firstFrame: string): number => {
    const at = body.indexOf(firstFrame);
    expect(at, body).toBeGreaterThan(0);
    return body.slice(0, at).split(":\n\n").length - 1;
  };

  test("completions: a held answer ends at its [DONE], though the upstream keeps the body open", async (ctx) => {
    if (!ready()) return ctx.skip();
    const started = Date.now();
    const { path, body } = REQUESTS.completions("hh-completions-open");
    const res = await post(path, body);
    expect(res.status).toBe(200);
    const wire = await res.text();
    expect(Date.now() - started, "nothing waits on what comes after the end").toBeLessThan(5_000);
    expect(wire).toContain("a whole answer");
    expect(wire.trimEnd().endsWith("data: [DONE]"), wire).toBe(true);
  }, 30_000);

  for (const endpoint of ["completions", "responses"] as const) {
    test(`${endpoint}: a clean answer longer than the heartbeat interval is preceded by heartbeats`, async (ctx) => {
      if (!ready()) return ctx.skip();
      const started = Date.now();
      const { path, body } = REQUESTS[endpoint](`hh-${endpoint}-clean`);
      const res = await post(path, body);
      const headAt = Date.now() - started;
      expect(res.status).toBe(200);
      expect(res.headers.get("content-type")).toContain("text/event-stream");
      expect(headAt, "the head does not wait for the verdict").toBeLessThan(HOLD_MS / 2);
      const { body: wire, firstFrameAt } = await read(res, started);
      expect(firstFrameAt!, "what is held stays held until the answer is whole").toBeGreaterThan(
        (EVENT_DELAY_MS * (EVENTS - 1)) / 2,
      );
      const firstFrame = endpoint === "completions" ? "data: " : "event: ";
      expect(heartbeatsAhead(wire, firstFrame)).toBeGreaterThanOrEqual(1);
      expect(wire).toContain("clean");
      expect(wire).toContain(endpoint === "completions" ? "data: [DONE]" : "response.completed");
    }, 30_000);

    test(`${endpoint}: a refusal arrives as failure frames after the heartbeats, and its row is a 200`, async (ctx) => {
      if (!ready()) return ctx.skip();
      const started = Date.now();
      const { path, body } = REQUESTS[endpoint](`hh-${endpoint}-refused`);
      const res = await post(path, body);
      expect(res.status).toBe(200);
      const requestId = res.headers.get("x-aisix-request-id") ?? "";
      const { body: wire } = await read(res, started);
      expect(heartbeatsAhead(wire, "event: ")).toBeGreaterThanOrEqual(1);
      expect(wire).not.toContain(FORBIDDEN.slice(0, 8));
      expect(wire).toContain("content_filter");
      const events = wire
        .split("\n\n")
        .filter((frame) => frame.startsWith("event: "))
        .map((frame) => /^event: (.*)$/m.exec(frame)?.[1]);
      expect(events).toEqual(
        endpoint === "completions"
          ? ["error"]
          : ["response.created", "response.in_progress", "error", "response.failed"],
      );
      const row = await waitForSlsLog(
        sls!,
        LOGSTORE,
        (log) => log.get("request_id") === requestId,
        `the refused ${endpoint} row`,
        20_000,
      );
      expect(row.get("status_code")).toBe("200");
      expect(row.get("guardrail_blocked")).toBe("true");
      expect(Number(row.get("completion_tokens")), "the upstream billed it").toBeGreaterThan(0);
    }, 30_000);

    test(`${endpoint}: a request refused before any upstream call keeps its real status`, async (ctx) => {
      if (!ready()) return ctx.skip();
      const model = `hh-${endpoint}-clean`;
      const upstream = upstreams.get(model)!;
      const baseline = upstream.receivedRequests.length;
      const { path, body } = REQUESTS[endpoint](model, `please ${INPUT_FORBIDDEN}`);
      const res = await post(path, body);
      expect(res.status).toBe(422);
      expect((await res.json()) as { error?: { type?: string } }).toMatchObject({
        error: { type: "content_filter" },
      });
      expect(upstream.receivedRequests).toHaveLength(baseline);
    });

    test(`${endpoint}: an upstream that refuses the request keeps its status too`, async (ctx) => {
      if (!ready()) return ctx.skip();
      const { path, body } = REQUESTS[endpoint]("hh-upstream-refuses");
      const res = await post(path, body);
      await res.text();
      expect(res.status).toBe(400);
    });
  }
});
