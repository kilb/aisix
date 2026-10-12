import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type ReceivedRequest,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an Anthropic ProviderKey's `request` block shapes every request the
// gateway sends to Claude, whichever protocol the caller speaks.
//
// User journey: an operator puts the Anthropic key behind the gateway and
// configures its `request` block once — a tracing header templated from the
// caller's team, the `anthropic-beta` client header allowed through
// (<https://docs.anthropic.com/en/api/beta-headers>), a default `top_k`, and
// a `temperature` ceiling. The same Claude model is then reached by:
//
//   - the Anthropic SDK through `/v1/messages`,
//   - the OpenAI SDK through `/v1/chat/completions`, streamed or not,
//   - a Responses-API agent through `/v1/responses`, streamed or not.
//
// Each of those calls must reach the upstream carrying the configured
// headers and body fields, and none may carry the caller's own credential:
// the upstream authenticates with the ProviderKey secret only, even when the
// operator lists `x-api-key` in the allowlist or the `default_headers` by
// mistake.

const CALLER = "sk-anth-overrides-caller-PLAINTEXT";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const PROVIDER_SECRET = "sk-ant-provider-secret";
const TEAM_ID = "team-orbit-7";
const BETA = "interleaved-thinking-2025-05-14";
const REPLY = "Sunny in Paris.";

// Headers every caller request carries.
const CLIENT_HEADERS = {
  "anthropic-beta": BETA,
  "x-corp-session": "sess-42",
  "x-not-allowlisted": "nope",
};

// A minimal Claude Messages upstream: it answers `stream: true` with the
// Messages SSE event sequence and anything else with a Message object, the
// way the real API does (<https://docs.anthropic.com/en/api/messages>,
// <https://docs.anthropic.com/en/docs/build-with-claude/streaming>).
interface ClaudeUpstream {
  baseUrl: string;
  received: ReceivedRequest[];
  close(): Promise<void>;
}

async function startClaudeUpstream(): Promise<ClaudeUpstream> {
  const received: ReceivedRequest[] = [];
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      received.push({
        method: req.method ?? "GET",
        path: req.url ?? "/",
        headers: Object.fromEntries(
          Object.entries(req.headers).map(([k, v]) => [k, Array.isArray(v) ? v.join(",") : (v ?? "")]),
        ),
        body: raw,
      });
      const body = JSON.parse(raw || "{}") as { model?: string; stream?: boolean };
      const message = {
        id: "msg_overrides_01",
        type: "message",
        role: "assistant",
        model: body.model ?? "claude-sonnet-4-5",
        content: [{ type: "text", text: REPLY }],
        stop_reason: "end_turn",
        stop_sequence: null,
        usage: { input_tokens: 12, output_tokens: 5 },
      };
      if (!body.stream) {
        res.statusCode = 200;
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(message));
        return;
      }
      res.statusCode = 200;
      res.setHeader("content-type", "text/event-stream");
      const events: Array<Record<string, unknown>> = [
        { type: "message_start", message: { ...message, content: [], stop_reason: null, usage: { input_tokens: 12, output_tokens: 1 } } },
        { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
        { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: REPLY } },
        { type: "content_block_stop", index: 0 },
        { type: "message_delta", delta: { stop_reason: "end_turn", stop_sequence: null }, usage: { output_tokens: 5 } },
        { type: "message_stop" },
      ];
      for (const e of events) res.write(`event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`);
      res.end();
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    received,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

/** Data payloads of a complete `text/event-stream` body, `[DONE]` excluded. */
function sseData(body: string): Array<Record<string, unknown>> {
  return body
    .replace(/\r\n/g, "\n")
    .split("\n\n")
    .flatMap((block) => {
      const data = block
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).trim())
        .join("\n");
      return data && data !== "[DONE]" ? [JSON.parse(data) as Record<string, unknown>] : [];
    });
}

describe("an Anthropic ProviderKey's request block applies on every endpoint that reaches Claude", () => {
  let app: SpawnedApp | undefined;
  let upstream: ClaudeUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startClaudeUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "overrides-anthropic-pk",
      provider: "anthropic",
      adapter: "anthropic",
      secret: PROVIDER_SECRET,
      api_base: upstream.baseUrl,
      request: {
        default_headers: {
          "x-corp-tenant": "${request.api_key.team_id}",
          "x-corp-static": "static-value",
          // Operator mistakes: an auth header, and the protocol version
          // the gateway itself speaks to the upstream.
          "x-api-key": "sk-operator-typo",
          "anthropic-version": "2099-01-01",
        },
        // Both credential headers listed by mistake too: the caller's
        // credential must still never be relayed.
        forward_client_headers: ["anthropic-beta", "x-corp-*", "x-api-key", "authorization"],
        default_body_fields: { top_k: 7 },
        param_constraints: { temperature_max: 0.5 },
      },
    });
    await seed.createModel({
      display_name: "overrides-claude",
      provider: "anthropic",
      model_name: "claude-sonnet-4-5",
      provider_key_id: pk.id,
    });

    // Caller key last: once it authenticates, every resource above is in
    // the snapshot.
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      team_id: TEAM_ID,
      allowed_models: ["overrides-claude"],
    });
    const proxy = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  /** POST `body` to `path` as the caller; returns the reply and what the upstream received. */
  async function call(path: string, body: Record<string, unknown>, auth: Record<string, string>) {
    const baseline = upstream!.received.length;
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...auth, ...CLIENT_HEADERS },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    return { status: res.status, text, sent: upstream!.received.slice(baseline) };
  }

  const bearer = { authorization: `Bearer ${CALLER}` };

  function expectRequestBlockApplied(sent: ReceivedRequest[]): void {
    expect(sent).toHaveLength(1);
    const req = sent[0]!;
    const h = req.headers;
    // The upstream authenticates with the ProviderKey secret alone, and
    // speaks the protocol version the gateway speaks.
    expect(h["x-api-key"]).toBe(PROVIDER_SECRET);
    expect(h["anthropic-version"]).toBe("2023-06-01");
    for (const [name, value] of Object.entries(h)) {
      expect(value, `caller credential leaked in "${name}"`).not.toContain(CALLER);
    }
    // default_headers, with the template rendered for this caller.
    expect(h["x-corp-tenant"]).toBe(TEAM_ID);
    expect(h["x-corp-static"]).toBe("static-value");
    // forward_client_headers: exact name and glob through, others not.
    expect(h["anthropic-beta"]).toBe(BETA);
    expect(h["x-corp-session"]).toBe("sess-42");
    expect(h["x-not-allowlisted"]).toBeUndefined();
    // default_body_fields fills an absent field; param_constraints clamps;
    // a value no override touches arrives exactly as the caller sent it.
    const body = JSON.parse(req.body) as Record<string, unknown>;
    expect(body.top_k).toBe(7);
    expect(body.temperature).toBe(0.5);
    expect(body.top_p).toBe(0.9);
  }

  test("/v1/messages → Claude", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await call(
      "/v1/messages",
      {
        model: "overrides-claude",
        max_tokens: 256,
        temperature: 0.9,
        top_p: 0.9,
        messages: [{ role: "user", content: "Weather in Paris?" }],
      },
      { "x-api-key": CALLER, "anthropic-version": "2023-06-01" },
    );
    expect(status, text).toBe(200);
    expect((JSON.parse(text) as { content: Array<{ text: string }> }).content[0]!.text).toBe(REPLY);
    expectRequestBlockApplied(sent);
  });

  test("/v1/chat/completions → Claude", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await call(
      "/v1/chat/completions",
      {
        model: "overrides-claude",
        temperature: 0.9,
        top_p: 0.9,
        messages: [{ role: "user", content: "Weather in Paris?" }],
      },
      bearer,
    );
    expect(status, text).toBe(200);
    expect((JSON.parse(text) as { choices: Array<{ message: { content: string } }> }).choices[0]!.message.content).toBe(
      REPLY,
    );
    expectRequestBlockApplied(sent);
  });

  test("/v1/chat/completions stream → Claude", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await call(
      "/v1/chat/completions",
      {
        model: "overrides-claude",
        stream: true,
        temperature: 0.9,
        top_p: 0.9,
        messages: [{ role: "user", content: "Weather in Paris?" }],
      },
      bearer,
    );
    expect(status, text).toBe(200);
    const content = sseData(text)
      .map((c) => (c.choices as Array<{ delta?: { content?: string } }> | undefined)?.[0]?.delta?.content ?? "")
      .join("");
    expect(content).toBe(REPLY);
    expectRequestBlockApplied(sent);
  });

  test("/v1/responses → Claude", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await call(
      "/v1/responses",
      { model: "overrides-claude", temperature: 0.9, top_p: 0.9, input: "Weather in Paris?" },
      bearer,
    );
    expect(status, text).toBe(200);
    const output = (JSON.parse(text) as { output: Array<{ content?: Array<{ text?: string }> }> }).output;
    expect(output.flatMap((o) => o.content ?? []).map((c) => c.text ?? "").join("")).toBe(REPLY);
    expectRequestBlockApplied(sent);
  });

  test("/v1/responses stream → Claude", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { status, text, sent } = await call(
      "/v1/responses",
      { model: "overrides-claude", stream: true, temperature: 0.9, top_p: 0.9, input: "Weather in Paris?" },
      bearer,
    );
    expect(status, text).toBe(200);
    const deltas = sseData(text)
      .filter((e) => e.type === "response.output_text.delta")
      .map((e) => e.delta as string)
      .join("");
    expect(deltas).toBe(REPLY);
    expectRequestBlockApplied(sent);
  });
});
