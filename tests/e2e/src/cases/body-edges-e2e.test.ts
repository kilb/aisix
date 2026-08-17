import { createHash } from "node:crypto";
import OpenAI, { APIError } from "openai";
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

// E2E: request body edge cases. Three user journeys that prior
// coverage skipped — every existing chat-completions test sends a
// single one-message array, so the gateway's behavior on real-world
// shapes was unverified:
//
//   1. Multi-turn 10+ messages — long conversation history with
//      system/user/assistant interleave must reach the upstream
//      byte-for-byte. A regression that truncated, dropped, or
//      reordered messages would silently lose context for every
//      conversational caller.
//
//   2. Body exceeds the configured size limit — caller must see
//      RFC 9110 §15.5.14 `413 Content Too Large`, NOT a 500 or
//      `ECONNRESET` from a mid-write socket close (which is
//      indistinguishable from a network failure or a gateway
//      crash). The gateway's `request_body_limit_bytes` is set to
//      10 MiB by this suite's harness configuration.
//
//   3. Empty `messages: []` — OpenAI Chat Completions spec requires
//      a non-empty messages array. Gateway must reject with a
//      4xx error envelope, NOT 500 / panic / hang.
//
// References:
// - OpenAI Chat Completions API spec
//   <https://platform.openai.com/docs/api-reference/chat/create>
// - OpenAI error envelope spec
//   <https://platform.openai.com/docs/guides/error-codes/api-errors>
// - RFC 9110 §15.5.14 "413 Content Too Large"
//   <https://datatracker.ietf.org/doc/html/rfc9110#section-15.5.14>

const CALLER_PLAINTEXT = "sk-body-edges-caller";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

/** An access-log line for a `/v1/completions` request the gateway refused. */
const isRefusal = (line: string): boolean =>
  line.includes("proxy request completed") &&
  line.includes('path="/v1/completions"') &&
  line.includes("status=413");

const countRefusals = (output: string): number =>
  output.split("\n").filter(isRefusal).length;

async function waitForModel(app: SpawnedApp, apiKey: string, model: string) {
  await waitConfigPropagation(async () => {
    const response = await fetch(`${app.proxyUrl}/v1/models`, {
      headers: { authorization: `Bearer ${apiKey}` },
    });
    if (response.status === 401) {
      await response.text();
      return false;
    }
    if (response.status !== 200) {
      throw new Error(`model propagation probe returned ${response.status}`);
    }
    const body = (await response.json()) as { data?: Array<{ id?: string }> };
    return body.data?.some((entry) => entry.id === model) === true;
  });
}

describe("body edges e2e: multi-turn, oversize body, empty messages", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let seed: SeedClient | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream();
    // `info` so the oversize case can assert on the access-log line the
    // gateway emits for the request it refuses.
    app = await spawnApp({ logLevel: "info" });
    seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "body-edges-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "body-edges",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["body-edges"],
    });

    // Authentication is seeded last; model discovery proves every earlier
    // routing row is applied without exercising chat behavior under test.
    await waitForModel(app, CALLER_PLAINTEXT, "body-edges");
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("multi-turn: 12-message history (system + 5×user/assistant + final user) reaches upstream byte-for-byte", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    // Realistic conversation shape: system primer + 5 turns of
    // back-and-forth + final user query awaiting response. Caller
    // SDKs build histories like this all the time.
    const history = [
      { role: "system" as const, content: "You are a helpful assistant." },
      { role: "user" as const, content: "Hi, what's 2+2?" },
      { role: "assistant" as const, content: "It's 4." },
      { role: "user" as const, content: "What about 3+3?" },
      { role: "assistant" as const, content: "That's 6." },
      { role: "user" as const, content: "Now 4+4?" },
      { role: "assistant" as const, content: "Eight." },
      { role: "user" as const, content: "And 5+5?" },
      { role: "assistant" as const, content: "Ten." },
      { role: "user" as const, content: "Last one: 6+6?" },
      { role: "assistant" as const, content: "Twelve." },
      { role: "user" as const, content: "Thanks. Now summarise." },
    ];

    const client = new OpenAI({
      apiKey: CALLER_PLAINTEXT,
      baseURL: `${app.proxyUrl}/v1`,
      maxRetries: 0,
    });

    const baseline = upstream.receivedRequests.length;
    const completion = await client.chat.completions.create({
      model: "body-edges",
      messages: history,
    });

    // Caller-side: 200 success with assistant role.
    expect(completion.choices[0]?.message.role).toBe("assistant");

    // Upstream-side: every message reached the upstream with role
    // and content intact. A regression that truncated to the last
    // message (or dropped the system primer) would fail here.
    const testCalls = upstream.receivedRequests
      .slice(baseline)
      .filter((r) => r.path === "/v1/chat/completions");
    expect(testCalls).toHaveLength(1);
    const sentBody = JSON.parse(testCalls[0]!.body) as {
      model?: string;
      messages?: Array<{ role?: string; content?: string }>;
    };
    // Gateway must translate the caller's display name into the
    // upstream-supplied model_name. A regression that forwarded the
    // caller's name to the upstream would 4xx in production
    // (upstream doesn't recognise "body-edges") but pass against a
    // permissive mock — pinning this catches that wire-shape gap.
    expect(sentBody.model).toBe("gpt-4o-mini");
    expect(sentBody.messages).toHaveLength(history.length);
    for (let i = 0; i < history.length; i++) {
      expect(sentBody.messages?.[i]?.role).toBe(history[i]!.role);
      expect(sentBody.messages?.[i]?.content).toBe(history[i]!.content);
    }
  });

  test("oversize body (> 10 MiB): caller sees 413, upstream untouched", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    // The harness configures `request_body_limit_bytes: 10485760`
    // (10 MiB). Construct a body just over the limit by stuffing a
    // 10.5 MiB filler into a single user message. `JSON.stringify`
    // sets a Content-Length header on the fetch request, which the
    // gateway's middleware uses to short-circuit before reading
    // any body bytes.
    const filler = "x".repeat(10 * 1024 * 1024 + 512 * 1024);
    const oversizedBody = JSON.stringify({
      model: "body-edges",
      messages: [{ role: "user", content: filler }],
    });

    const upstreamHitsBefore = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: oversizedBody,
    });

    // RFC 9110 §15.5.14: `413 Content Too Large` is the standard
    // status for a request body that exceeds a server-imposed
    // limit. A 5xx here would mislead callers into retrying (the
    // request will never succeed at this size); ECONNRESET (the
    // pre-fix behavior) is indistinguishable from a network
    // failure or a gateway crash.
    expect(res.status).toBe(413);
    // OpenAI envelope shape: `{ "error": { "message": ..., "type": ... } }`.
    // A regression that returned a non-OpenAI shape (e.g. axum's
    // default `"Failed to buffer the request body: ..."`) would
    // fail the type/message assertions below. Pin `error.type` to
    // the exact OpenAI taxonomy value so a regression that emitted
    // any other non-empty string would fail.
    const body = (await res.json()) as {
      error?: { type?: unknown; message?: unknown };
    };
    expect(body.error?.type).toBe("invalid_request_error");
    expect(typeof body.error?.message).toBe("string");
    expect(body.error?.message as string).toMatch(/limit/i);

    // Hard contract: an over-limit request must never reach the
    // upstream — the gateway's own body-size cap is meant to
    // protect the upstream from oversized payloads, not pre-route
    // them.
    expect(upstream.receivedRequests.length).toBe(upstreamHitsBefore);
  });

  test("oversize body: the gateway records the request it refused", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // The 413 above is answered by the body-cap middleware, which
    // short-circuits BEFORE any handler runs — and the access log is
    // emitted BY the handlers. That left an operator with nothing to
    // look at: a caller reporting a 413 the gateway had no record of is
    // indistinguishable from the request never arriving.
    //
    // (`observability.access_log` in the harness config is the reserved
    // field nothing reads today — the access log is gated by the log
    // level alone, which is why this suite raises it to `info`.)
    const filler = "x".repeat(10 * 1024 * 1024 + 512 * 1024);
    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "body-edges",
        messages: [{ role: "user", content: filler }],
      }),
    });
    expect(res.status).toBe(413);

    // Join the log line to THIS request instead of grepping for a bare
    // `413` any other case in the suite could also have produced.
    const requestId = res.headers.get("x-aisix-request-id");
    expect(requestId).toBeTruthy();

    let line: string | undefined;
    await waitConfigPropagation(async () => {
      line = app!
        .output()
        .split("\n")
        .find(
          (l) =>
            l.includes(requestId!) && l.includes("proxy request completed"),
        );
      return line !== undefined;
    });
    expect(line).toMatch(/status=413/);
    expect(line).toContain("/v1/chat/completions");
    // The reason, not just the status: `error_kind` carries the OpenAI
    // envelope's coarse `invalid_request_error`, so a cap hit is only
    // nameable through the message.
    expect(line).toMatch(/request body exceeds/);

    // Same blindness on the metrics plane — no handler ran, so nothing
    // counted the refusal either.
    const scrape = await fetch(`${app.metricsUrl}/metrics`).then((r) =>
      r.text(),
    );
    expect(scrape).toMatch(/aisix_requests_total\{[^}]*status="413"/);

    // The access log says the request was refused; it cannot say what
    // the gateway did with the body it refused. That second question is
    // what an operator has when the caller reports a connection reset
    // rather than a 413 — so the same request also carries a diagnostic
    // naming the declared size, the cap, how much was absorbed and how
    // the drain ended, joined by the same request id.
    const detail = app
      .output()
      .split("\n")
      .find(
        (l) =>
          l.includes(requestId!) &&
          l.includes("request body exceeded the configured limit"),
      );
    expect(detail).toBeDefined();
    expect(detail).toMatch(/declared_content_length=\d+/);
    expect(detail).toMatch(/configured_limit_bytes=10485760/);
    expect(detail).toMatch(/drained_bytes=\d+/);
    // This caller sent everything it declared, so the connection stayed
    // usable and it got to read the 413 above.
    expect(detail).toMatch(/drain_outcome="completed"/);
    expect(detail).toMatch(/endpoint="\/v1\/chat\/completions"/);

    const sample = scrape
      .split("\n")
      .find((l) =>
        l.startsWith("aisix_proxy_request_body_limit_rejections_total{"),
      );
    expect(sample).toBeDefined();
    expect(sample).toContain('endpoint="/v1/chat/completions"');
    expect(sample).toContain('outcome="completed"');
  });

  test("chunked oversize body: the handler that rejected it records the refusal", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // No Content-Length, so the middleware can't judge the request up
    // front: it reaches the handler, whose body extractor rejects once
    // the cap is crossed. Same blindness one layer further in, so it
    // needs its own coverage. `/v1/completions` isolates the count —
    // nothing else in this suite calls it.
    const before = countRefusals(app.output());

    const chunk = "x".repeat(512 * 1024);
    const body = new ReadableStream({
      start(controller) {
        for (let i = 0; i < 22; i++) controller.enqueue(new TextEncoder().encode(chunk));
        controller.close();
      },
    });
    // A client streaming into a cap can legitimately lose the connection
    // mid-write instead of reading the 413 — the failure mode the
    // Content-Length path exists to avoid and this one cannot. The
    // gateway's record is the subject here, so tolerate either outcome.
    await fetch(`${app.proxyUrl}/v1/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body,
      duplex: "half",
    } as RequestInit & { duplex: "half" }).catch(() => undefined);

    await waitConfigPropagation(async () => countRefusals(app!.output()) > before);
    const line = app
      .output()
      .split("\n")
      .filter((l) => isRefusal(l))
      .at(-1)!;
    expect(line).toMatch(/status=413/);
    expect(line).toMatch(/request body exceeds/);
    // Auth ran before the body extractor here, so unlike the
    // middleware's short-circuit the refusal is attributable to a caller.
    expect(line).toMatch(/api_key_id/);
  });

  test("empty messages array: 4xx with OpenAI-shape error envelope, upstream untouched", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    const client = new OpenAI({
      apiKey: CALLER_PLAINTEXT,
      baseURL: `${app.proxyUrl}/v1`,
      maxRetries: 0,
    });

    // Path-filtered baseline (matches the convention used across
    // the suite): a regression that triggered an unrelated upstream
    // call would not falsely inflate this counter.
    const upstreamChatHitsBefore = upstream.receivedRequests.filter(
      (r) => r.path === "/v1/chat/completions",
    ).length;

    let caught: unknown;
    try {
      await client.chat.completions.create({
        model: "body-edges",
        // OpenAI Chat Completions spec requires a non-empty
        // messages array. Empty must be rejected at the validation
        // boundary, not bubbled up as a 500 / panic.
        messages: [],
      });
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(APIError);
    if (!(caught instanceof APIError)) {
      throw new Error("unreachable: caught is not APIError");
    }
    // OpenAI Chat Completions request schema declares
    // `messages: minItems: 1` — a schema-violation 400 is the only
    // spec-conformant outcome. 401/403/404/422 here would all
    // signal a different bug (auth ordering, model resolution,
    // schema choice).
    expect(caught.status).toBe(400);
    // Pin the OpenAI error vocabulary: the gateway is rejecting
    // on OpenAI's behalf, so it must use OpenAI's published value
    // for schema violations rather than a gateway-internal string
    // <https://platform.openai.com/docs/guides/error-codes/api-errors>.
    const err = caught.error as { type?: unknown; message?: unknown };
    expect(err.type).toBe("invalid_request_error");
    expect(typeof err.message).toBe("string");
    expect((err.message as string).length).toBeGreaterThan(0);

    // Validation must short-circuit before dispatch.
    const upstreamChatHitsAfter = upstream.receivedRequests.filter(
      (r) => r.path === "/v1/chat/completions",
    ).length;
    expect(upstreamChatHitsAfter).toBe(upstreamChatHitsBefore);
  });
});

// When `request_body_limit_bytes` is omitted, the gateway selects finite
// endpoint-aware limits. The standard JSON limit admits realistic long
// prompts while the Messages boundary follows the upstream 32 MB contract:
// <https://platform.claude.com/docs/en/api/errors#request-size-limits>.
describe("body edges e2e: automatic endpoint-aware defaults", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream();
    app = await spawnApp({ requestBodyLimitBytes: null });
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "body-automatic-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "body-automatic",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["body-automatic"],
    });

    await waitForModel(app, CALLER_PLAINTEXT, "body-automatic");
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("a 12 MiB JSON body reaches the upstream", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    // Comfortably above both the harness's explicit 10 MiB test cap and
    // axum's built-in 2 MiB fallback, but below the automatic JSON limit.
    const filler = "x".repeat(12 * 1024 * 1024);
    const upstreamHitsBefore = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "body-automatic",
        messages: [{ role: "user", content: filler }],
      }),
    });

    expect(res.status).toBe(200);
    await res.json();
    expect(upstream.receivedRequests.length).toBe(upstreamHitsBefore + 1);
    const sent = upstream.receivedRequests[upstreamHitsBefore]!;
    const sentBody = JSON.parse(sent.body) as {
      messages?: Array<{ content?: string }>;
    };
    expect(sentBody.messages?.[0]?.content?.length).toBe(filler.length);
  }, 60_000);

  test("a Messages body above 32 MiB is rejected before dispatch", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }

    const filler = "x".repeat(32 * 1024 * 1024 + 512 * 1024);
    const upstreamHitsBefore = upstream.receivedRequests.length;
    const res = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER_PLAINTEXT}`,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: "body-automatic",
        max_tokens: 8,
        messages: [{ role: "user", content: filler }],
      }),
    });

    expect(res.status).toBe(413);
    const body = (await res.json()) as {
      type?: unknown;
      error?: { type?: unknown; message?: unknown };
    };
    expect(body.type).toBe("error");
    expect(body.error?.type).toBe("request_too_large");
    expect(typeof body.error?.message).toBe("string");
    expect(upstream.receivedRequests.length).toBe(upstreamHitsBefore);
  }, 60_000);
});
