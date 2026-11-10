import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForLogLine,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a Chat Completions upstream whose finish chunk reports the prompt but
// not the completion. Reached through the Responses→Chat and Messages→Chat
// bridges, the terminal event the caller reads (`response.completed`, the
// closing `message_delta`) is built from that chunk. The completion count it
// carries must be the one the request is recorded with — the local estimate
// the gateway fills the missing counter with — not a 0 next to a billed
// estimate.

const CALLER = "sk-bridge-partial-usage";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const ANSWER = "a bridged answer long enough to count as several tokens";

const chunk = (delta: Record<string, unknown>, finish: string | null, usage?: unknown) =>
  JSON.stringify({
    id: "chatcmpl-partial",
    object: "chat.completion.chunk",
    created: 1,
    model: "gpt-4o-mini",
    choices: [{ index: 0, delta, finish_reason: finish }],
    ...(usage ? { usage } : {}),
  });

/** One `key=value` field of an access-log line. */
function field(line: string, name: string): string | undefined {
  const m = line.match(new RegExp(`\\b${name}=(?:"([^"]*)"|([^\\s]+))`));
  return m ? (m[1] ?? m[2]) : undefined;
}

/** The SSE events of a body, as `[event, data]`. */
function events(body: string): Array<[string, Record<string, unknown>]> {
  return body
    .split("\n\n")
    .map((frame) => {
      const event = frame.match(/^event: (.+)$/m)?.[1] ?? "";
      const data = frame.match(/^data: (.+)$/m)?.[1];
      return [event, data && data !== "[DONE]" ? JSON.parse(data) : {}] as [string, Record<string, unknown>];
    })
    .filter(([event]) => event !== "");
}

describe("a bridged stream's terminal usage is the usage it is recorded with", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream({
      streamEvents: [
        chunk({ role: "assistant", content: ANSWER }, null),
        chunk({}, "stop", { prompt_tokens: 3, completion_tokens: 0, total_tokens: 3 }),
        "[DONE]",
      ],
    });
    // The access-log line is `tracing::info!`.
    app = await spawnApp({ logLevel: "info" });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "bridge-partial-usage-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
      // No native Responses or Messages surface: both routes bridge to chat.
      apis: {},
    });
    await seed.createModel({
      display_name: "bridge-partial-usage",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // Seeded last: once it authenticates, every resource above is live.
    await seed.createApiKey({ key_hash: HASH, allowed_models: ["bridge-partial-usage"] });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  async function stream(path: string, body: Record<string, unknown>) {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({ model: "bridge-partial-usage", stream: true, ...body }),
    });
    expect(res.status).toBe(200);
    const requestId = res.headers.get("x-aisix-request-id") ?? "";
    const text = await res.text();
    const line = await waitForLogLine(
      app!,
      (l) => l.includes(`request_id="${requestId}"`) && l.includes("proxy request completed"),
      `the access-log line for ${requestId}`,
    );
    return { events: events(text), line };
  }

  test("/v1/responses: response.completed reports the completion the row records", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { events: evs, line } = await stream("/v1/responses", { input: "hi" });
    const completed = evs.find(([e]) => e === "response.completed")?.[1] as
      | { response?: { usage?: { input_tokens?: number; output_tokens?: number } } }
      | undefined;
    const usage = completed?.response?.usage;
    expect(usage?.input_tokens).toBe(3);
    const recorded = Number(field(line, "completion_tokens"));
    expect(recorded).toBeGreaterThan(0);
    expect(usage?.output_tokens).toBe(recorded);
  });

  test("/v1/messages: the closing message_delta reports the completion the row records", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { events: evs, line } = await stream("/v1/messages", {
      max_tokens: 64,
      messages: [{ role: "user", content: "hi" }],
    });
    const delta = evs.find(([e]) => e === "message_delta")?.[1] as
      | { usage?: { output_tokens?: number } }
      | undefined;
    const recorded = Number(field(line, "completion_tokens"));
    expect(recorded).toBeGreaterThan(0);
    expect(delta?.usage?.output_tokens).toBe(recorded);
  });
});
