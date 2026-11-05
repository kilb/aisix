import { createHash } from "node:crypto";
import { createServer, type IncomingMessage, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { WebSocket } from "ws";
import {
  EtcdClient,
  SeedClient,
  slsLogsFor,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForLogLine,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: an upstream that quotes the credential it was sent back in its error
// does not hand that credential to the caller.
//
// Some upstreams echo the key in a refusal ("Incorrect API key provided: …",
// or the headers they saw). The caller receives the upstream's error with its
// status and the rest of its message, but never the provider key the gateway
// sent: not in the response, not in the exported row, not in the gateway's
// log. Checked on every request family, on a refused request and a streamed
// one, on an error event inside a stream the upstream had already started,
// and on a passthrough route that relays the upstream's error as sent.
//
// The key carries no recognisable prefix, so only the gateway knowing what it
// sent can take it out wherever the upstream put it.

const CALLER = "sk-credential-scrub-caller";
const KEY = "pkScrubE2Ez9Y8X7W6V5U4T3S2R1Q0";
const LOGSTORE = "credential-scrub-rows";
const REFUSED = "Incorrect API key provided.";
const BROKE_OFF = "The server had an error while streaming.";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

/**
 * What the upstream saw, quoted the way a provider quotes a bad key: the key
 * itself, then each credential header as it arrived.
 */
const saw = (req: IncomingMessage) => {
  const header = (name: string) => {
    const value = req.headers[name];
    return typeof value === "string" ? value : "-";
  };
  const key = header("authorization").replace(/^Bearer /, "");
  const sent = key !== "-" ? key : header("x-api-key") !== "-" ? header("x-api-key") : header("api-key");
  return (
    `You sent ${sent}. Upstream saw Authorization: ${header("authorization")}; ` +
    `x-api-key: ${header("x-api-key")}; api-key: ${header("api-key")}`
  );
};

const sse = (event: string | null, data: unknown) =>
  `${event ? `event: ${event}\n` : ""}data: ${JSON.stringify(data)}\n\n`;

describe("an upstream that quotes the credential it was sent", () => {
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;
  const servers: Server[] = [];

  async function listen(server: Server): Promise<string> {
    servers.push(server);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    return `http://127.0.0.1:${(server.address() as { port: number }).port}`;
  }

  /** Refuses every request with a 401 quoting the credential headers. */
  const refusingUpstream = () =>
    listen(
      createServer((req, res) => {
        req.resume();
        req.on("end", () => {
          res.writeHead(401, { "content-type": "application/json" });
          res.end(
            JSON.stringify({
              error: {
                message: `${REFUSED} ${saw(req)}`,
                type: "invalid_request_error",
                code: "invalid_api_key",
              },
            }),
          );
        });
      }),
    );

  /** Starts a stream, then reports an error in it quoting the credential. */
  const failingStreamUpstream = () =>
    listen(
      createServer((req, res) => {
        req.resume();
        req.on("end", () => {
          const message = `${BROKE_OFF} ${saw(req)}`;
          res.writeHead(200, { "content-type": "text/event-stream" });
          const url = req.url ?? "";
          if (url.includes("/messages")) {
            res.write(
              sse("message_start", {
                type: "message_start",
                message: {
                  id: "msg_1",
                  type: "message",
                  role: "assistant",
                  model: "m",
                  content: [],
                  stop_reason: null,
                  usage: { input_tokens: 1, output_tokens: 0 },
                },
              }),
            );
            res.end(sse("error", { type: "error", error: { type: "overloaded_error", message } }));
          } else if (url.includes("/responses")) {
            res.write(
              sse("response.created", {
                type: "response.created",
                response: { id: "resp_1", object: "response", status: "in_progress", model: "m", output: [] },
              }),
            );
            res.end(sse("error", { type: "error", code: "server_error", message, param: null }));
          } else if (url.includes("/completions") && !url.includes("/chat/")) {
            res.write(sse(null, { id: "c1", object: "text_completion", model: "m", choices: [{ index: 0, text: "hel" }] }));
            res.end(sse(null, { error: { message, type: "server_error" } }));
          } else {
            res.write(
              sse(null, {
                id: "c1",
                object: "chat.completion.chunk",
                created: 1,
                model: "m",
                choices: [{ index: 0, delta: { role: "assistant", content: "hel" }, finish_reason: null }],
              }),
            );
            res.end(sse(null, { error: { message, type: "server_error" } }));
          }
        });
      }),
    );

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        RUST_LOG: "info",
        SLS_CRED_MOCK_AK_ID: "LTAI_mock_ak",
        SLS_CRED_MOCK_AK_SECRET: "mock_ak_secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: LOGSTORE,
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: "mock",
      content_mode: "full",
    });

    const refusing = await refusingUpstream();
    const failing = await failingStreamUpstream();
    const names: string[] = [];
    const model = async (name: string, provider: string, apiBase: string, adapter?: string) => {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: KEY,
        provider,
        ...(adapter ? { adapter } : {}),
        api_base: apiBase,
      });
      await seed.createModel({
        display_name: name,
        provider,
        model_name: "m",
        provider_key_id: pk.id,
        cooldown: { enabled: false },
      });
      names.push(name);
      return pk.id;
    };
    const openAiPk = await model("openai-refusing", "openai", `${refusing}/v1`);
    await model("anthropic-refusing", "anthropic", refusing);
    await model("azure-refusing", "azure-openai", refusing, "azure-openai");
    await model("cohere-refusing", "cohere", refusing, "openai");
    await model("openai-failing", "openai", `${failing}/v1`);
    await model("anthropic-failing", "anthropic", failing);
    await seed.createPassthroughRoute({
      name: "scrub-passthrough",
      path_prefix: "/scrub-passthrough",
      target_url: refusing,
      provider_key_id: openAiPk,
    });
    await seed.createApiKey({ key_hash: sha256(CALLER), allowed_routes: ["*"], allowed_models: names });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      const { data } = (await res.json()) as { data?: unknown[] };
      return data?.length === names.length;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    for (const s of servers) s.close();
  });

  const json = (body: unknown): RequestInit => ({
    method: "POST",
    headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const multipart = (fields: Record<string, string>): RequestInit => {
    const form = new FormData();
    for (const [k, v] of Object.entries(fields)) form.set(k, v);
    form.set("file", new Blob([new Uint8Array(16)]), "input.png");
    return { method: "POST", headers: { authorization: `Bearer ${CALLER}` }, body: form };
  };
  const messages = [{ role: "user", content: "hi" }];

  async function send(path: string, init: RequestInit) {
    const res = await fetch(`${app!.proxyUrl}${path}`, init);
    return { status: res.status, text: await res.text(), requestId: res.headers.get("x-aisix-request-id")! };
  }

  /** The request's rows never hold the key, and name the refusal in its words. */
  async function expectRowsClean(requestId: string, text?: string) {
    const final = await waitForSlsLog(
      sls!,
      LOGSTORE,
      (l) => l.get("request_id") === requestId && l.get("is_final_attempt") === "true",
      `the row of ${requestId}`,
    );
    if (text) expect(final.get("error_message") ?? "").toContain(text);
    for (const row of slsLogsFor(sls!, LOGSTORE).filter((l) => l.get("request_id") === requestId)) {
      expect(JSON.stringify([...row.entries()])).not.toContain(KEY);
    }
    await waitForLogLine(app!, (l) => l.includes(requestId), `the access-log line of ${requestId}`);
  }

  const REFUSED_ROUTES: [string, string, RequestInit][] = [
    ["chat", "/v1/chat/completions", json({ model: "openai-refusing", messages })],
    ["streamed chat", "/v1/chat/completions", json({ model: "openai-refusing", messages, stream: true })],
    ["chat on azure", "/v1/chat/completions", json({ model: "azure-refusing", messages })],
    ["embeddings", "/v1/embeddings", json({ model: "openai-refusing", input: "hi" })],
    ["completions", "/v1/completions", json({ model: "openai-refusing", prompt: "hi" })],
    ["streamed completions", "/v1/completions", json({ model: "openai-refusing", prompt: "hi", stream: true })],
    ["native messages", "/v1/messages", json({ model: "anthropic-refusing", messages, max_tokens: 5 })],
    [
      "streamed native messages",
      "/v1/messages",
      json({ model: "anthropic-refusing", messages, max_tokens: 5, stream: true }),
    ],
    ["bridged messages", "/v1/messages", json({ model: "openai-refusing", messages, max_tokens: 5 })],
    ["count_tokens", "/v1/messages/count_tokens", json({ model: "anthropic-refusing", messages })],
    ["native responses", "/v1/responses", json({ model: "openai-refusing", input: "hi" })],
    ["streamed native responses", "/v1/responses", json({ model: "openai-refusing", input: "hi", stream: true })],
    ["bridged responses", "/v1/responses", json({ model: "anthropic-refusing", input: "hi" })],
    ["rerank", "/v1/rerank", json({ model: "cohere-refusing", query: "q", documents: ["a"] })],
    ["speech", "/v1/audio/speech", json({ model: "openai-refusing", input: "hi", voice: "alloy" })],
    ["transcriptions", "/v1/audio/transcriptions", multipart({ model: "openai-refusing" })],
    ["image edits", "/v1/images/edits", multipart({ model: "openai-refusing", prompt: "hi" })],
    [
      "batches",
      "/v1/batches?model=openai-refusing",
      json({ input_file_id: "file-x", endpoint: "/v1/chat/completions", completion_window: "24h" }),
    ],
    ["passthrough route", "/scrub-passthrough/v1/chat/completions", json({ model: "m", messages })],
  ];

  test.each(REFUSED_ROUTES)("%s: the refusal arrives with its status and text, without the key", async (_what, path, init) => {
    if (!etcdReachable) return;
    const got = await send(path, init);
    expect(got.status, got.text).toBe(401);
    expect(got.text).toContain(REFUSED);
    expect(got.text).not.toContain(KEY);
    await expectRowsClean(got.requestId, REFUSED);
  });

  test("realtime: a refused handshake reaches the client without the key", async () => {
    if (!etcdReachable) return;
    const { requestId, frames } = await new Promise<{ requestId: string; frames: string[] }>(
      (resolve, reject) => {
        const ws = new WebSocket(`${app!.proxyUrl.replace("http://", "ws://")}/v1/realtime?model=openai-refusing`, {
          headers: { authorization: `Bearer ${CALLER}` },
        });
        let id = "";
        const got: string[] = [];
        ws.on("upgrade", (res) => (id = String(res.headers["x-aisix-request-id"] ?? "")));
        ws.on("message", (data) => got.push(String(data)));
        ws.on("close", (_code, reason) => resolve({ requestId: id, frames: [...got, String(reason)] }));
        ws.on("error", reject);
      },
    );
    expect(frames.join("\n")).toContain(REFUSED);
    expect(frames.join("\n")).not.toContain(KEY);
    await expectRowsClean(requestId, REFUSED);
  });

  const BROKEN_STREAMS: [string, string, RequestInit][] = [
    ["chat", "/v1/chat/completions", json({ model: "openai-failing", messages, stream: true })],
    ["completions", "/v1/completions", json({ model: "openai-failing", prompt: "hi", stream: true })],
    [
      "native messages",
      "/v1/messages",
      json({ model: "anthropic-failing", messages, max_tokens: 5, stream: true }),
    ],
    ["native responses", "/v1/responses", json({ model: "openai-failing", input: "hi", stream: true })],
  ];

  test.each(BROKEN_STREAMS)("%s: an error inside a started stream arrives without the key", async (_what, path, init) => {
    if (!etcdReachable) return;
    const got = await send(path, init);
    expect(got.status).toBe(200);
    expect(got.text).toContain(BROKE_OFF);
    expect(got.text).not.toContain(KEY);
    await expectRowsClean(got.requestId);
  });

  test("the gateway's log never carried the key", async () => {
    if (!etcdReachable) return;
    expect(app!.output()).toContain(REFUSED);
    expect(app!.output()).not.toContain(KEY);
  });
});
