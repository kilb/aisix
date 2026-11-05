import { createHash } from "node:crypto";
import { WebSocket as WsClient, WebSocketServer, type WebSocket as WsSocket } from "ws";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// A rule that can block an output has to judge it before the caller has it. A
// realtime answer arrives as many delta frames, so a phrase across two of them
// reaches a caller frame by frame unless the answer is judged whole.
const CALLER_PLAINTEXT = "sk-realtime-hold-PLAINTEXT";
const CALLER_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const FORBIDDEN = "realtimeholdsentinel";
const HEAD = FORBIDDEN.slice(0, 8);
const TAIL = FORBIDDEN.slice(8);
const USAGE = { input_tokens: 2, output_tokens: 3 };

const SCRIPTS: Record<string, unknown[]> = {
  "rt-split-text": [
    { type: "response.output_text.delta", item_id: "i1", delta: `say ${HEAD}` },
    { type: "response.output_text.delta", item_id: "i1", delta: `${TAIL} now` },
    { type: "response.output_text.done", item_id: "i1", text: `say ${FORBIDDEN} now` },
    { type: "response.done", response: { usage: USAGE } },
  ],
  "rt-split-transcript": [
    { type: "response.output_audio_transcript.delta", item_id: "i1", delta: `say ${HEAD}` },
    { type: "response.output_audio_transcript.delta", item_id: "i1", delta: `${TAIL} now` },
    { type: "response.output_audio_transcript.done", item_id: "i1" },
    { type: "response.done", response: { usage: USAGE } },
  ],
  "rt-split-arguments": [
    { type: "response.function_call_arguments.delta", item_id: "i1", delta: `{"a":"${HEAD}` },
    { type: "response.function_call_arguments.delta", item_id: "i1", delta: `${TAIL}"}` },
    { type: "response.function_call_arguments.done", item_id: "i1" },
    { type: "response.done", response: { usage: USAGE } },
  ],
  "rt-clean": [
    { type: "response.output_audio_transcript.delta", item_id: "i1", delta: "hel" },
    { type: "response.output_audio.delta", item_id: "i1", delta: "AAAA" },
    { type: "response.output_audio_transcript.delta", item_id: "i1", delta: "lo" },
    { type: "response.output_audio_transcript.done", item_id: "i1", transcript: "hello" },
    { type: "response.done", response: { usage: USAGE } },
  ],
  "rt-interrupted": [
    { type: "response.output_text.delta", item_id: "i1", delta: "partial" },
    { type: "response.done", response: { status: "cancelled", usage: USAGE } },
  ],
  "rt-big": [
    { type: "response.output_text.delta", item_id: "i1", delta: "x".repeat(3 * 1024 * 1024) },
    { type: "response.output_text.done", item_id: "i1" },
    { type: "response.done", response: { usage: USAGE } },
  ],
};

interface Scripted {
  port: number;
  close(): Promise<void>;
}

/** Plays a model's script as soon as the client speaks. */
async function startScriptedUpstream(script: unknown[]): Promise<Scripted> {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  wss.on("connection", (socket: WsSocket) => {
    socket.once("message", () => {
      for (const frame of script) socket.send(JSON.stringify(frame));
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

interface Session {
  frames: Array<Record<string, unknown>>;
  closeCode?: number;
}

describe("a realtime answer is judged whole before it reaches the caller", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  const upstreams: Scripted[] = [];

  /** Everything a client receives, until the answer or the session ends. */
  function play(model: string): Promise<Session> {
    const url = `${app!.proxyUrl.replace("http://", "ws://")}/v1/realtime?model=${model}`;
    return new Promise((resolve, reject) => {
      const session: Session = { frames: [] };
      const ws = new WsClient(url, { headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` } });
      const timer = setTimeout(() => {
        ws.terminate();
        resolve(session);
      }, 10_000);
      ws.on("open", () => ws.send(JSON.stringify({ type: "response.create" })));
      ws.on("message", (data) => {
        const frame = JSON.parse(data.toString()) as Record<string, unknown>;
        session.frames.push(frame);
        if (frame.type === "response.done") {
          clearTimeout(timer);
          ws.close();
          resolve(session);
        }
      });
      ws.on("close", (code) => {
        clearTimeout(timer);
        session.closeCode = code;
        resolve(session);
      });
      ws.on("error", (err) => {
        clearTimeout(timer);
        reject(err);
      });
    });
  }

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createGuardrail({
      name: "rt-output-block",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN }],
    });
    for (const [model, script] of Object.entries(SCRIPTS)) {
      const upstream = await startScriptedUpstream(script);
      upstreams.push(upstream);
      const pk = await seed.createProviderKey({
        display_name: `${model}-pk`,
        secret: "sk-upstream",
        api_base: `http://127.0.0.1:${upstream.port}/v1`,
      });
      await seed.createModel({
        display_name: model,
        provider: "openai",
        model_name: "gpt-realtime-mock",
        provider_key_id: pk.id,
      });
    }
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
    await Promise.all(upstreams.map((u) => u.close()));
  });

  for (const model of ["rt-split-text", "rt-split-transcript", "rt-split-arguments"]) {
    test(`${model}: none of a refused answer goes out`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const session = await play(model);
      expect(JSON.stringify(session.frames)).not.toContain(HEAD);
      expect(session.frames.map((f) => f.type)).toEqual(["error"]);
      expect((session.frames[0].error as { code: string }).code).toBe("content_filtered");
      expect(session.closeCode).toBe(1011);
    });
  }

  test("a clean answer arrives whole and in order, and its audio does not wait", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { frames } = await play("rt-clean");
    expect(frames.map((f) => f.type)).toContain("response.output_audio.delta");
    const transcript = frames.filter((f) => f.type === "response.output_audio_transcript.delta");
    expect(transcript.map((f) => f.delta).join("")).toBe("hello");
    const types = frames.map((f) => f.type);
    expect(types.at(-2)).toBe("response.output_audio_transcript.done");
    expect(types.at(-1)).toBe("response.done");
    expect(types.indexOf("response.output_audio_transcript.done")).toBeGreaterThan(
      types.lastIndexOf("response.output_audio_transcript.delta"),
    );
  });

  test("an answer the upstream ended early still reaches the caller", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const { frames } = await play("rt-interrupted");
    expect(frames.map((f) => f.type)).toEqual(["response.output_text.delta", "response.done"]);
  });

  test("past the rule's buffer the fail-closed rule refuses rather than forwards", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const session = await play("rt-big");
    expect(session.frames.map((f) => f.type)).toEqual(["error"]);
    expect(session.closeCode).toBe(1011);
  });
});
