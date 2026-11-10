import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  slsLogsFor,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E (R-072): a passthrough route holds a streamed answer back for an output
// guardrail, and every hold-back also bounds the RAW bytes it keeps (128 times
// `max_buffer_bytes`). When the frame that crosses that bound is a complete
// one carrying usage — here the Anthropic `message_delta` with the final
// `output_tokens` — the cap decides what happens to the stream, but the
// upstream already reported (and billed) those tokens. The row must carry
// them, whether the cap refuses the stream (`fail_closed`) or releases it
// unscanned (`fail_open`).
//
// The visible text ("Hello!") stays under the content cap, so only the raw
// bound trips, and it trips exactly on the usage frame: the frames before it
// are padded to sit just under the bound.
//
// The crossing frame can be an error event too. Under `fail_open` it is
// released like every later frame: less the credentials the request sent,
// and recorded as the stream's failure.

const CALLER = "sk-passthrough-crossing-usage";
const PK_SECRET = "sk-ant-crossing-secret";
const CREDENTIAL_REF = "mock";
const LOGSTORE = "passthrough-crossing-usage";
const CAP = 7;
const RAW_BOUND = 128 * CAP;
const hash = (s: string) => createHash("sha256").update(s).digest("hex");

const frame = (type: string, body: Record<string, unknown>) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...body })}\n\n`;

const messageStart = (padding: string) =>
  frame("message_start", {
    message: {
      id: "msg_crossing",
      type: "message",
      role: "assistant",
      model: "claude-3-5-haiku-20241022",
      content: [],
      stop_reason: null,
      usage: { input_tokens: 25, output_tokens: 1 },
      padding,
    },
  });

const middle = [
  frame("content_block_start", { index: 0, content_block: { type: "text", text: "" } }),
  frame("content_block_delta", { index: 0, delta: { type: "text_delta", text: "Hello!" } }),
  frame("content_block_stop", { index: 0 }),
];
const usageFrame = frame("message_delta", {
  delta: { stop_reason: "end_turn", stop_sequence: null },
  usage: { output_tokens: 15 },
});
const stopFrame = frame("message_stop", {});

/** The stream, with the frames ahead of the usage frame padded to end
 * `headroom` bytes under the raw bound. */
function frames(): string[] {
  const unpadded = messageStart("").length + middle.reduce((n, f) => n + f.length, 0);
  const headroom = 10;
  const fill = RAW_BOUND - headroom - unpadded;
  const head = [messageStart("p".repeat(fill)), ...middle];
  const before = head.reduce((n, f) => n + f.length, 0);
  // The fixture is what the test claims: under the bound until the usage
  // frame, past it with that frame.
  if (before > RAW_BOUND || before + usageFrame.length <= RAW_BOUND) {
    throw new Error(`fixture does not cross on the usage frame: ${before} + ${usageFrame.length}`);
  }
  return [...head, usageFrame, stopFrame];
}

/** An upstream that names the key it was sent in an error event. */
const errorFrame = frame("error", {
  error: { type: "overloaded_error", message: `overloaded for key ${PK_SECRET}` },
});

/** A stream whose error event is the frame that crosses the raw bound. */
function errorFrames(): string[] {
  const fill = RAW_BOUND - 10 - messageStart("").length;
  const head = messageStart("p".repeat(fill));
  if (head.length > RAW_BOUND || head.length + errorFrame.length <= RAW_BOUND) {
    throw new Error(`fixture does not cross on the error frame: ${head.length}`);
  }
  return [head, errorFrame];
}

describe("a passthrough hold-back cap that trips on a usage frame keeps its usage", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    const body = frames().join("");
    const errorBody = errorFrames().join("");
    upstream = createServer((req, res) => {
      req.on("data", () => {});
      req.on("end", () => {
        res.statusCode = 200;
        res.setHeader("content-type", "text/event-stream");
        res.end((req.url ?? "").startsWith("/error/") ? errorBody : body);
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const upstreamUrl = `http://127.0.0.1:${(upstream.address() as AddressInfo).port}`;
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-passthrough-crossing-usage",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "metadata_only",
    });
    const pk = await seed.createProviderKey({
      display_name: "crossing-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: PK_SECRET,
      api_base: upstreamUrl,
    });
    for (const [name, policy, target] of [
      ["fail_closed", "fail_closed", upstreamUrl],
      ["fail_open", "fail_open", upstreamUrl],
      ["error-fail_open", "fail_open", `${upstreamUrl}/error`],
    ]) {
      const guard = await seed.createGuardrail(
        {
          name: `crossing-${name}`,
          enabled: true,
          hook_point: "output",
          kind: "pii",
          detectors: [{ type: "email", action: "block" }],
          max_buffer_bytes: CAP,
          on_buffer_exceeded: policy,
        },
        { attach: false },
      );
      const route = await seed.createPassthroughRoute({
        name: `crossing-${name}`,
        path_prefix: `/passthrough/crossing-${name}`,
        target_url: target,
        provider_key_id: pk.id,
      });
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: guard.id,
        scope_type: "passthrough_route",
        scope_id: route.id,
        priority: 100,
      });
    }
    // The key last: once it authenticates, the rest is in the snapshot.
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: ["*"], allowed_routes: ["*"] });
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
    if (upstream) {
      upstream.closeAllConnections();
      await new Promise<void>((resolve) => upstream!.close(() => resolve()));
    }
  });

  for (const [policy, released] of [
    ["fail_closed", false],
    ["fail_open", true],
  ] as const) {
    test(`${policy}: the row bills the output tokens the crossing frame reported`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      const res = await fetch(`${app!.proxyUrl}/passthrough/crossing-${policy}/v1/messages`, {
        method: "POST",
        headers: {
          "x-api-key": CALLER,
          "anthropic-version": "2023-06-01",
          "content-type": "application/json",
        },
        body: JSON.stringify({
          model: "claude-3-5-haiku-20241022",
          max_tokens: 64,
          stream: true,
          messages: [{ role: "user", content: "say hello" }],
        }),
      });
      const text = await res.text();
      const requestId = res.headers.get("x-aisix-request-id") ?? "";
      expect(requestId).not.toBe("");
      // The cap's own decision, which this change leaves as it was.
      if (released) {
        expect(text, "fail_open releases the stream").toContain('"output_tokens":15');
      } else {
        expect(text, "fail_closed refuses the stream").not.toContain('"output_tokens":15');
      }

      await waitForSlsLog(
        sls!,
        LOGSTORE,
        (log) => log.get("request_id") === requestId,
        `the usage row of ${requestId}`,
        20_000,
      );
      const rows = slsLogsFor(sls!, LOGSTORE).filter((log) => log.get("request_id") === requestId);
      expect(rows).toHaveLength(1);
      expect(rows[0].get("prompt_tokens")).toBe("25");
      expect(
        rows[0].get("completion_tokens"),
        "the output tokens the upstream reported on the crossing frame",
      ).toBe("15");
    });
  }

  test("fail_open: an error event that crosses the bound goes out scrubbed, as the stream's failure", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await fetch(`${app!.proxyUrl}/passthrough/crossing-error-fail_open/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "claude-3-5-haiku-20241022",
        max_tokens: 64,
        stream: true,
        messages: [{ role: "user", content: "say hello" }],
      }),
    });
    const text = await res.text();
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    expect(requestId).not.toBe("");
    expect(text, "fail_open releases the error event").toContain("overloaded_error");
    expect(text, "the key the gateway sent reached the caller").not.toContain(PK_SECRET);

    const row = await waitForSlsLog(
      sls!,
      LOGSTORE,
      (log) => log.get("request_id") === requestId,
      `the usage row of ${requestId}`,
      20_000,
    );
    expect(Number(row.get("status_code")), "the error event is the stream's failure").toBeGreaterThanOrEqual(500);
  });
});
