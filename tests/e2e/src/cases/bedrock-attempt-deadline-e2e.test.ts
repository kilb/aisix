import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a Bedrock model's `timeout` bounds one non-streamed attempt as a
// whole. An attempt that first downloads an image the request names and then
// calls Converse, or that embeds several texts with one Titan call each,
// spends one budget across those phases; each phase fitting inside the
// budget on its own does not stretch the attempt past it.
//
// The model `timeout` is the "end-to-end timeout in milliseconds for
// non-streaming upstream calls" (the Model resource schema); a timed-out
// attempt is answered 504.

const CALLER_PLAINTEXT = "sk-bedrock-attempt-deadline";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");

// Each phase takes most of the model's budget, and two of them together
// take well over it.
const PHASE_MS = 700;
const TIMEOUT_MS = 1000;

const PNG = Buffer.from("iVBORw0KGgo=", "base64");

const CONVERSE_REPLY = {
  output: { message: { role: "assistant", content: [{ text: "ok" }] } },
  stopReason: "end_turn",
  usage: { inputTokens: 7, outputTokens: 1, totalTokens: 8 },
  metrics: { latencyMs: 1 },
};
const TITAN_REPLY = { embedding: [0.25, 0.5], inputTextTokenCount: 4 };

function slowServer(answer: (path: string) => [string, Buffer | string]): Promise<Server> {
  const server = createServer((req, res) => {
    req.resume();
    req.on("end", () => {
      setTimeout(() => {
        const [type, body] = answer(req.url ?? "");
        res.writeHead(200, { "content-type": type });
        res.end(body);
      }, PHASE_MS);
    });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

function portOf(server: Server): number {
  const addr = server.address();
  if (addr === null || typeof addr === "string") throw new Error("no port");
  return addr.port;
}

describe("Bedrock: one deadline per non-streamed attempt", () => {
  let app: SpawnedApp | undefined;
  let bedrock: Server | undefined;
  let media: Server | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    bedrock = await slowServer((path) =>
      path.includes("titan-embed")
        ? ["application/json", JSON.stringify(TITAN_REPLY)]
        : ["application/json", JSON.stringify(CONVERSE_REPLY)],
    );
    media = await slowServer(() => ["image/png", PNG]);

    app = await spawnApp({
      extra: {
        outbound: { ssrf_protection: { allowed_hosts: [`localhost:${portOf(media)}`] } },
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "attempt-deadline-bedrock-pk",
      provider: "bedrock",
      adapter: "bedrock",
      secret: JSON.stringify({
        access_key_id: "AKIA-attempt-deadline",
        secret_access_key: "sk-attempt-deadline",
        region: "us-east-1",
      }),
      api_base: `http://127.0.0.1:${portOf(bedrock)}`,
    });
    for (const [alias, name] of [
      ["deadline-nova", "amazon.nova-lite-v1:0"],
      ["deadline-titan", "amazon.titan-embed-text-v1"],
    ]) {
      await seed.createModel({
        display_name: alias,
        provider: "bedrock",
        model_name: name,
        provider_key_id: pk.id,
        timeout: TIMEOUT_MS,
        retries: 0,
      });
    }
    // Seeded last, so this key authenticating implies the whole seed set
    // has reached the gateway's snapshot.
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      await res.text();
      return res.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    for (const server of [bedrock, media]) {
      await new Promise<void>((resolve) => (server ? server.close(() => resolve()) : resolve()));
    }
  });

  async function timed(path: string, body: unknown) {
    const started = Date.now();
    const res = await fetch(`${app!.proxyUrl}${path}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    return { status: res.status, text, elapsed: Date.now() - started };
  }

  test("an image download and the Converse call share the model's timeout", async (ctx) => {
    if (!etcdReachable || !app || !media) return ctx.skip();
    const res = await timed("/v1/chat/completions", {
      model: "deadline-nova",
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: "what is this?" },
            { type: "image_url", image_url: { url: `http://localhost:${portOf(media)}/slow.png` } },
          ],
        },
      ],
    });
    expect(res.status, res.text).toBe(504);
    expect(res.elapsed).toBeLessThan(2 * PHASE_MS);
  });

  test("Titan's one call per text shares the model's timeout", async (ctx) => {
    if (!etcdReachable || !app) return ctx.skip();
    const res = await timed("/v1/embeddings", {
      model: "deadline-titan",
      input: ["first", "second"],
    });
    expect(res.status, res.text).toBe(504);
    expect(res.elapsed).toBeLessThan(2 * PHASE_MS);
  });
});
