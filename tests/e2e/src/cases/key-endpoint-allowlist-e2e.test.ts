import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { WebSocket as WsClient, WebSocketServer } from "ws";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// An API key can name the endpoint families it may call. A key limited to
// chat completions and the model list is served there and refused everywhere
// else — embeddings, token counting, a realtime session, a passthrough route
// it is granted — before any upstream is contacted. A key that names no
// families is unrestricted. Every refusal has a twin on the unrestricted key
// that succeeds, so a refusal cannot be a broken fixture in disguise.

const LIMITED = "sk-endpoint-allowlist-e2e-limited";
const OPEN = "sk-endpoint-allowlist-e2e-open";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

const CHAT_BODY = {
  id: "chatcmpl-allowlist",
  object: "chat.completion",
  created: 1,
  model: "gpt-4o-mini",
  choices: [
    { index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" },
  ],
  usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
};

const EMBEDDING_BODY = {
  object: "list",
  data: [{ object: "embedding", index: 0, embedding: [0.25, 0.5] }],
  model: "text-embedding-3-small",
  usage: { prompt_tokens: 2, total_tokens: 2 },
};

interface RealtimeUpstream {
  port: number;
  handshakes: number;
  close(): Promise<void>;
}

/** A realtime upstream that counts handshakes and answers any client event. */
async function startRealtimeUpstream(): Promise<RealtimeUpstream> {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  const upstream = {
    port: 0,
    handshakes: 0,
    close: () =>
      new Promise<void>((resolve, reject) => wss.close((e) => (e ? reject(e) : resolve()))),
  };
  wss.on("connection", (socket) => {
    upstream.handshakes++;
    socket.on("message", () => socket.send(JSON.stringify({ type: "session.updated" })));
  });
  await new Promise<void>((resolve) => wss.on("listening", resolve));
  const addr = wss.address();
  if (addr === null || typeof addr === "string") throw new Error("no port");
  upstream.port = addr.port;
  return upstream;
}

describe("an API key limited to some endpoint families", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let realtime: RealtimeUpstream | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      pathBodies: {
        "/v1/chat/completions": CHAT_BODY,
        "/v1/embeddings": EMBEDDING_BODY,
      },
    });
    realtime = await startRealtimeUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "allowlist-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "allowlist-chat",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createModel({
      display_name: "allowlist-embed",
      provider: "openai",
      model_name: "text-embedding-3-small",
      provider_key_id: pk.id,
    });
    const realtimePk = await seed.createProviderKey({
      display_name: "allowlist-realtime-pk",
      secret: "sk-mock-realtime",
      api_base: `http://127.0.0.1:${realtime.port}/v1`,
    });
    await seed.createModel({
      display_name: "allowlist-realtime",
      provider: "openai",
      model_name: "gpt-realtime-mock",
      provider_key_id: realtimePk.id,
    });
    await seed.createPassthroughRoute({
      name: "allowlist-tunnel",
      path_prefix: "/passthrough/allowlist",
      target_url: `${upstream.baseUrl}/v1`,
      provider_key_id: pk.id,
    });

    // Caller keys last; the limited key is the readiness gate below. Both may
    // use every model and every passthrough route, so nothing but the
    // endpoint list tells them apart.
    await seed.createApiKey({
      key_hash: sha256(OPEN),
      allowed_models: ["*"],
      allowed_routes: ["*"],
    });
    await seed.createApiKey({
      key_hash: sha256(LIMITED),
      allowed_models: ["*"],
      allowed_routes: ["*"],
      allowed_endpoints: ["chat_completions", "models"],
    });
    const probe = new ProxyClient(app.proxyUrl, LIMITED);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await realtime?.close();
  });

  const ready = (ctx: { skip: () => void }) => {
    if (!etcdReachable || !app || !upstream || !realtime) {
      ctx.skip();
      return false;
    }
    return true;
  };

  async function post(
    path: string,
    key: string,
    body: unknown,
    headers: Record<string, string> = { authorization: `Bearer ${key}` },
  ): Promise<{ status: number; text: string }> {
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...headers },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  }

  const upstreamCalls = (path: string) =>
    upstream!.receivedRequests.filter((r) => r.path === path).length;

  const chat = { model: "allowlist-chat", messages: [{ role: "user", content: "hi" }] };
  const embed = { model: "allowlist-embed", input: "hello" };

  function expectRefusal(answer: { status: number; text: string }, label: string) {
    expect(answer.status, `${label}: ${answer.text}`).toBe(403);
    // The refusal does not recite what the key may call.
    expect(answer.text, label).not.toContain("chat_completions");
  }

  test("the limited key is served on the families it lists", async (ctx) => {
    if (!ready(ctx)) return;
    const before = upstreamCalls("/v1/chat/completions");
    const answer = await post("/v1/chat/completions", LIMITED, chat);
    expect(answer.status, answer.text).toBe(200);
    expect(JSON.parse(answer.text).choices[0].message.content).toBe("ok");
    expect(upstreamCalls("/v1/chat/completions")).toBe(before + 1);
  });

  test("embeddings are refused to the limited key and never reach the upstream", async (ctx) => {
    if (!ready(ctx)) return;
    const answer = await post("/v1/embeddings", LIMITED, embed);
    expectRefusal(answer, "embeddings");
    expect(JSON.parse(answer.text).error.type).toBe("permission_denied");
    expect(upstreamCalls("/v1/embeddings")).toBe(0);
  });

  test("the unrestricted key is served embeddings", async (ctx) => {
    if (!ready(ctx)) return;
    const before = upstreamCalls("/v1/embeddings");
    const answer = await post("/v1/embeddings", OPEN, embed);
    expect(answer.status, answer.text).toBe(200);
    expect(JSON.parse(answer.text).data[0].embedding).toEqual([0.25, 0.5]);
    expect(upstreamCalls("/v1/embeddings")).toBe(before + 1);
  });

  test("token counting is refused to the limited key", async (ctx) => {
    if (!ready(ctx)) return;
    const body = { model: "allowlist-chat", messages: [{ role: "user", content: "hi" }] };
    const anthropic = (key: string) => ({ "x-api-key": key, "anthropic-version": "2023-06-01" });
    const refused = await post("/v1/messages/count_tokens", LIMITED, body, anthropic(LIMITED));
    expectRefusal(refused, "count_tokens");
    const open = await post("/v1/messages/count_tokens", OPEN, body, anthropic(OPEN));
    expect(open.status, open.text).not.toBe(403);
  });

  test("a realtime session is refused to the limited key before the upstream is dialled", async (ctx) => {
    if (!ready(ctx)) return;
    const wsUrl = `${app!.proxyUrl.replace("http://", "ws://")}/v1/realtime?model=allowlist-realtime`;
    const dial = (key: string) =>
      new Promise<number | string>((resolve) => {
        const c = new WsClient(wsUrl, { headers: { authorization: `Bearer ${key}` } });
        c.on("open", () => c.send(JSON.stringify({ type: "session.update" })));
        c.on("message", (d) => {
          resolve(JSON.parse(d.toString()).type as string);
          c.terminate();
        });
        c.on("unexpected-response", (_q, res) => resolve(res.statusCode ?? 0));
        c.on("error", (e) => resolve(`error: ${e.message}`));
      });

    const before = realtime!.handshakes;
    expect(await dial(LIMITED)).toBe(403);
    expect(realtime!.handshakes).toBe(before);
    // The same session on the unrestricted key opens and relays.
    expect(await dial(OPEN)).toBe("session.updated");
    expect(realtime!.handshakes).toBe(before + 1);
  });

  test("a granted passthrough route is refused to the limited key", async (ctx) => {
    if (!ready(ctx)) return;
    const path = "/passthrough/allowlist/v1/chat/completions";
    const before = upstreamCalls("/v1/chat/completions");
    expectRefusal(await post(path, LIMITED, chat), "passthrough");
    expect(upstreamCalls("/v1/chat/completions")).toBe(before);

    const open = await post(path, OPEN, chat);
    expect(open.status, open.text).toBe(200);
    expect(upstreamCalls("/v1/chat/completions")).toBe(before + 1);
  });
});
