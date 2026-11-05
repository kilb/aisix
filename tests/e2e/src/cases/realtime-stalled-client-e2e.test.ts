import { createHash } from "node:crypto";
import type { Socket } from "node:net";
import { WebSocket as WsClient, WebSocketServer, type WebSocket as WsSocket } from "ws";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// A realtime client that stays connected but stops reading must not keep its
// session, and with it the key's concurrency slot, for as long as its socket
// stays up. Once the gateway gives up on it, the key can open a new session.
const CALLER_PLAINTEXT = "sk-realtime-stalled-PLAINTEXT";
const CALLER_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const MODEL = "rt-stalled";
/** Far more than any socket buffers between the gateway and the client. */
const FLOOD = { frames: 64, bytes: 1 << 20 };

/** Answers the first client frame with a flood of audio, then idles. */
async function startFloodingUpstream(): Promise<{ port: number; close(): Promise<void> }> {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  const frame = JSON.stringify({
    type: "response.output_audio.delta",
    item_id: "i1",
    delta: "A".repeat(FLOOD.bytes),
  });
  wss.on("connection", (socket: WsSocket) => {
    socket.once("message", () => {
      for (let i = 0; i < FLOOD.frames; i++) socket.send(frame);
    });
  });
  await new Promise<void>((resolve) => wss.on("listening", resolve));
  const addr = wss.address();
  if (addr === null || typeof addr === "string") throw new Error("no port");
  return {
    port: addr.port,
    close: () =>
      new Promise<void>((resolve) => {
        for (const client of wss.clients) client.terminate();
        wss.close(() => resolve());
      }),
  };
}

describe("a realtime client that stops reading", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let upstream: { port: number; close(): Promise<void> } | undefined;
  const clients: WsClient[] = [];

  const url = () => `${app!.proxyUrl.replace("http://", "ws://")}/v1/realtime?model=${MODEL}`;
  const connect = () => {
    const ws = new WsClient(url(), { headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` } });
    clients.push(ws);
    return ws;
  };
  /** Whether a session opens, or the upgrade is refused with a status. */
  const opens = (ws: WsClient) =>
    new Promise<number>((resolve) => {
      ws.once("open", () => resolve(101));
      ws.once("unexpected-response", (_req, res) => resolve(res.statusCode ?? 0));
      ws.once("error", () => resolve(0));
    });

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    upstream = await startFloodingUpstream();
    const pk = await seed.createProviderKey({
      display_name: "rt-stalled-pk",
      secret: "sk-upstream",
      api_base: `http://127.0.0.1:${upstream.port}/v1`,
    });
    await seed.createModel({
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-realtime-mock",
      provider_key_id: pk.id,
      // The session's idle budget.
      stream_timeout: 500,
    });
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: ["*"],
      rate_limit: { concurrency: 1 },
    });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      return res.status === 200;
    });
  });

  afterAll(async () => {
    for (const ws of clients) ws.terminate();
    await app?.exit();
    await upstream?.close();
  });

  test("does not hold its session, so the key's one slot comes back", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const stalled = connect();
    expect(await opens(stalled)).toBe(101);
    // Alive, and never reads again.
    (stalled as unknown as { _socket: Socket })._socket.pause();
    stalled.send(JSON.stringify({ type: "response.create" }));

    // The slot is taken while the session runs.
    await new Promise((resolve) => setTimeout(resolve, 200));
    const second = connect();
    expect(await opens(second)).toBe(429);

    // And given back once the gateway stops waiting on a client that reads
    // nothing, though that client is still connected.
    let reopened = 0;
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      const next = connect();
      reopened = await opens(next);
      if (reopened === 101) break;
      next.terminate();
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    expect(reopened).toBe(101);
    expect(stalled.readyState === WsClient.CLOSED).toBe(false);
  }, 30_000);
});
