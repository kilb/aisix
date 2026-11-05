import { createHash } from "node:crypto";
import { gzipSync } from "node:zlib";
import { WebSocket as WsClient, WebSocketServer } from "ws";
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

// E2E: a blocking input rule is not a matter of how the caller spells the
// request. The same words, in a shape the upstream reads and the rule's plain
// spelling does not match, are refused, and the provider hears nothing.

const KEY = "sk-guardrail-request-shapes-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");
const WORD = "forbiddenphrase";

describe("input rule: request shapes", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let realtimeUpstream: WebSocketServer | undefined;
  const realtimeFrames: string[] = [];
  let etcdReachable = false;

  const post = (path: string, body: unknown): Promise<Response> =>
    fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      pathBodies: {
        "/v1/embeddings": {
          object: "list",
          model: "m",
          data: [{ object: "embedding", index: 0, embedding: [0.1] }],
          usage: { prompt_tokens: 1, total_tokens: 1 },
        },
        "/v1/chat/completions": {
          id: "c",
          object: "chat.completion",
          created: 1,
          model: "m",
          choices: [
            { index: 0, finish_reason: "stop", message: { role: "assistant", content: "ok" } },
          ],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        },
        "/submit": { accepted: true },
      },
    });
    realtimeUpstream = new WebSocketServer({ port: 0, host: "127.0.0.1" });
    await new Promise((resolve) => realtimeUpstream!.once("listening", resolve));
    realtimeUpstream.on("connection", (socket) => {
      socket.on("message", (data) => realtimeFrames.push(data.toString()));
    });
    const realtimePort = (realtimeUpstream.address() as { port: number }).port;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "shapes-pk",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: "shapes-model",
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    const realtimePk = await seed.createProviderKey({
      display_name: "shapes-realtime-pk",
      secret: "sk-mock",
      api_base: `http://127.0.0.1:${realtimePort}/v1`,
    });
    await seed.createModel({
      display_name: "shapes-realtime",
      provider: "openai",
      model_name: "gpt-realtime",
      provider_key_id: realtimePk.id,
    });
    await seed.createPassthroughRoute({
      name: "shapes-route",
      path_prefix: "/shapes",
      target_url: upstream.baseUrl,
      credential_mode: "inject",
      provider_key_id: pk.id,
    });
    await seed.createApiKey({
      key_hash: sha256(KEY),
      allowed_models: ["*"],
      allowed_routes: ["*"],
    });
    await seed.createGuardrail({
      name: "shapes-rule",
      enabled: true,
      hook_point: "input",
      fail_open: false,
      kind: "keyword",
      patterns: [{ kind: "literal", value: WORD }],
    });
    await waitConfigPropagation(async () => {
      try {
        const r = await post("/v1/chat/completions", {
          model: "shapes-model",
          messages: [{ role: "user", content: WORD }],
        });
        await r.text();
        return r.status === 422;
      } catch {
        return false;
      }
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
    await new Promise((resolve) => (realtimeUpstream ? realtimeUpstream.close(resolve) : resolve(null)));
  });

  const refused = async (send: () => Promise<Response>, label: string) => {
    const before = upstream!.receivedRequests.length;
    const r = await send();
    expect(r.status, label).toBe(422);
    await r.text();
    expect(upstream!.receivedRequests.length, label).toBe(before);
  };

  test("token ids on /v1/embeddings", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    for (const input of [[1, 2, 3], [[1, 2], [3]]]) {
      await refused(
        () => post("/v1/embeddings", { model: "shapes-model", input }),
        JSON.stringify(input),
      );
    }
    const clean = await post("/v1/embeddings", { model: "shapes-model", input: "hello" });
    expect(clean.status).toBe(200);
    await clean.text();
  });

  test("a chat content part that is not plain text", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const shapes: unknown[] = [
      [
        { role: "assistant", content: [{ type: "refusal", refusal: WORD }] },
        { role: "user", content: "hi" },
      ],
      [{ role: "user", content: [{ type: "input_text", text: WORD }] }],
      [{ role: "user", content: [{ type: "text", text: [WORD] }] }],
    ];
    for (const messages of shapes) {
      await refused(
        () => post("/v1/chat/completions", { model: "shapes-model", messages }),
        JSON.stringify(messages),
      );
    }
    const clean = await post("/v1/chat/completions", {
      model: "shapes-model",
      messages: [
        { role: "assistant", content: [{ type: "refusal", refusal: "cannot help" }] },
        { role: "user", content: "hi" },
      ],
    });
    expect(clean.status).toBe(200);
    await clean.text();
  });

  test("a passthrough body the upstream reads decoded", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const plain = JSON.stringify({ messages: [{ role: "user", content: `${WORD} ${"filler ".repeat(80)}` }] });
    const send = (headers: Record<string, string>, body: Uint8Array | string) => () =>
      fetch(`${app!.proxyUrl}/shapes/submit`, {
        method: "POST",
        headers: { authorization: `Bearer ${KEY}`, ...headers },
        body,
      });
    await refused(
      send({ "content-type": "application/json", "content-encoding": "gzip" }, gzipSync(plain)),
      "gzip",
    );
    await refused(
      send(
        { "content-type": "application/x-www-form-urlencoded" },
        `prompt=%${WORD.charCodeAt(0).toString(16)}${WORD.slice(1)}`,
      ),
      "form",
    );
    const clean = await send(
      { "content-type": "application/x-www-form-urlencoded" },
      "prompt=hello%20there",
    )();
    expect(clean.status).toBe(200);
    await clean.text();
  });

  test("a realtime frame", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const item = (text: string) =>
      `{"type":"conversation.item.create","item":{"type":"message","role":"user","content":[{"type":"input_text","text":"${text}"}]}}`;
    const open = () =>
      new Promise<WsClient>((resolve, reject) => {
        const ws = new WsClient(
          `${app!.proxyUrl.replace("http://", "ws://")}/v1/realtime?model=shapes-realtime`,
          { headers: { authorization: `Bearer ${KEY}` } },
        );
        ws.once("open", () => resolve(ws));
        ws.once("error", reject);
      });
    const refusal = async (data: string | Buffer, binary: boolean) => {
      const ws = await open();
      const answer = new Promise<string>((resolve) => {
        ws.once("message", (reply) => resolve(reply.toString()));
      });
      ws.send(data, { binary });
      const reply = await answer;
      ws.close();
      return reply;
    };
    const escaped = `\\u${WORD.charCodeAt(0).toString(16).padStart(4, "0")}${WORD.slice(1)}`;
    expect(await refusal(item(WORD), false)).toContain("content_filtered");
    expect(await refusal(item(escaped), false)).toContain("content_filtered");
    expect(await refusal(Buffer.from(item(WORD)), true)).toContain("content_filtered");
    expect(realtimeFrames.filter((frame) => frame.includes(WORD.slice(1)))).toEqual([]);

    const ws = await open();
    ws.send(item("hello"));
    await waitConfigPropagation(async () => realtimeFrames.length > 0);
    ws.close();
    expect(realtimeFrames[0]).toContain("hello");
  });
});
