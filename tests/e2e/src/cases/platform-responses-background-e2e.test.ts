import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: in platform mode a Responses request cannot run in the background.
//
// With `background: true` the upstream runs the response after the request
// returns and charges for it then; the caller reads the result by polling
// `GET /v1/responses/{id}` (https://developers.openai.com/api/docs/guides/background).
// The gateway serves no such poll, so it never sees what that response cost,
// and the platform would pay the upstream for an answer no usage row bills.
// So the request is refused, as an invalid request naming the field, before
// any upstream hears of it, buffered or streamed. A request that does not
// ask for the background is served as usual, its `background: false` passed
// on as sent.

const hash = (plaintext: string) => createHash("sha256").update(plaintext).digest("hex");
const CALLER_KEY = "sk-platform-background-e2e-caller";
const PLATFORM_SECRET_ENV = "P0_E2E_PLATFORM_ID_KEY";
// 32 random bytes as hex digits, the form the gateway requires.
const PLATFORM_SECRET = "5f0b8f7d2c1e4a6b9d3c7e1f2a4b6c8d0e2f4a6b8c0d2e4f6a8b0c2d4e6f8a0b";
const MODEL = "bg-model";

type Json = Record<string, unknown>;

interface Upstream {
  base: string;
  /** Every body `/v1/responses` received, in order. */
  responses: Json[];
  close(): Promise<void>;
}

async function startUpstream(): Promise<Upstream> {
  const responses: Json[] = [];
  const server: Server = createServer((req, res) => {
    res.on("error", () => {});
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = (req.url ?? "/").split("?")[0];
      if (path !== "/v1/responses") {
        res.statusCode = 404;
        res.end(JSON.stringify({ error: { message: `no route for ${path}` } }));
        return;
      }
      const body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Json;
      responses.push(body);
      const response = {
        id: `resp_${randomUUID().replaceAll("-", "")}`,
        object: "response",
        created_at: 0,
        model: String(body.model ?? ""),
        status: "completed",
        output: [
          {
            type: "message",
            id: "msg_1",
            role: "assistant",
            status: "completed",
            content: [{ type: "output_text", text: "served", annotations: [] }],
          },
        ],
        usage: { input_tokens: 5, output_tokens: 1, total_tokens: 6 },
      };
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(response));
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    base: `http://127.0.0.1:${port}`,
    responses,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

describe("platform mode refuses a Responses request run in the background", () => {
  let etcdReachable = false;
  let upstream: Upstream | undefined;
  let app: SpawnedApp | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startUpstream();
    app = await spawnApp({
      extraEnv: {
        AISIX_PROXY__PLATFORM__ENABLED: "true",
        AISIX_PROXY__PLATFORM__ID_KEY_ENV: PLATFORM_SECRET_ENV,
        [PLATFORM_SECRET_ENV]: PLATFORM_SECRET,
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "bg-pk-openai",
      secret: "sk-upstream-openai",
      api_base: `${upstream.base}/v1`,
    });
    await seed.createModel({
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // The key last: once it authenticates, the model is in the snapshot.
    await seed.createApiKey({ key_hash: hash(CALLER_KEY), allowed_models: [MODEL] });
    const proxy = new ProxyClient(app.proxyUrl, CALLER_KEY);
    await waitConfigPropagation(async () => (await proxy.listModels()).status === 200);
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  const post = async (body: Json) => {
    const res = await fetch(`${app!.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_KEY}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  };

  for (const stream of [false, true]) {
    test(`a ${stream ? "streamed" : "buffered"} background request is refused before the upstream`, async (ctx) => {
      if (!etcdReachable) ctx.skip();
      const before = upstream!.responses.length;
      const body: Json = { model: MODEL, input: "run this later", background: true, store: true };
      if (stream) body.stream = true;
      const res = await post(body);
      expect(res.status, res.text).toBe(400);
      const error = (JSON.parse(res.text) as { error: Json }).error;
      expect(error.type).toBe("invalid_request_error");
      expect(error.code).toBe("invalid_request");
      expect(error.param).toBe("background");
      expect(upstream!.responses.length, "the upstream must never hear of it").toBe(before);
    });
  }

  test("a request that does not ask for the background is served, its flag passed on", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const before = upstream!.responses.length;
    const res = await post({ model: MODEL, input: "run this now", background: false });
    expect(res.status, res.text).toBe(200);
    expect(res.text).toContain("served");
    expect(upstream!.responses.length).toBe(before + 1);
    expect(upstream!.responses.at(-1)?.background).toBe(false);
  });
});
