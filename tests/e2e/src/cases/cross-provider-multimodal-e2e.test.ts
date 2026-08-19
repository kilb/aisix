import { createHash } from "node:crypto";
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

// An OpenAI-shape caller dispatched to an Anthropic upstream lost every
// image. The bridge kept only `type: "text"` blocks, so a vision request
// arrived as text alone and the model answered about a picture it was never
// shown — a plausible answer to a question it could not see, with nothing in
// the response to say the image had gone.
//
// This asserts against the bytes the UPSTREAM received, which is the only
// place the truth lives: the caller's own response looks identical either
// way, which is exactly why the bug survived.

const CALLER = "sk-multimodal-e2e";
const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

/** 1x1 PNG. The magic bytes matter — the bridge reads them. */
const PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

describe("cross-provider multimodal dispatch", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = await startOpenAiUpstream({
      nonStreamBody: {
        id: "msg_mm",
        type: "message",
        role: "assistant",
        model: "claude-3-5-haiku-20241022",
        content: [{ type: "text", text: "a red dot" }],
        stop_reason: "end_turn",
        usage: { input_tokens: 5, output_tokens: 3 },
      },
    });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const pk = await seed.createProviderKey({
      display_name: "mm-anthropic-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
      provider: "anthropic",
      adapter: "anthropic",
    });
    await seed.createModel({
      display_name: "mm-claude",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: pk.id,
    });
    // Last, so a gate on this key authenticating implies the rows above.
    await seed.createApiKey({
      key_hash: sha256(CALLER),
      allowed_models: ["mm-claude"],
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  test("an image survives the hop to an Anthropic upstream", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    await waitConfigPropagation(async () => (await chat("ready")).status === 200);

    const before = upstream!.receivedRequests.length;
    expect((await chat(`data:image/png;base64,${PNG_B64}`)).status).toBe(200);
    const sent = upstream!.receivedRequests.at(-1);
    expect(upstream!.receivedRequests.length).toBe(before + 1);

    const body = JSON.parse(sent!.body) as {
      messages: Array<{ content: Array<Record<string, unknown>> }>;
    };
    const blocks = body.messages.at(-1)!.content;

    const image = blocks.find((b) => b.type === "image") as
      | { source?: { type?: string; media_type?: string; data?: string } }
      | undefined;
    expect(
      image,
      `the upstream received no image block — the model would answer about a ` +
        `picture it was never shown: ${JSON.stringify(blocks)}`,
    ).toBeDefined();
    expect(image!.source?.type).toBe("base64");
    expect(image!.source?.media_type).toBe("image/png");
    expect(
      image!.source?.data,
      "the payload must arrive byte-identical, not re-encoded",
    ).toBe(PNG_B64);

    // Order is the other half of the contract: the text around an image is
    // what makes it a question about that image.
    expect(blocks.map((b) => b.type)).toEqual(["text", "image", "text"]);
  }, 60_000);

  test("a mislabelled data URI is corrected from its magic bytes", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    // Declares JPEG, carries the PNG above. Anthropic validates the declared
    // media type against the bytes and rejects a mismatch, so an uncorrected
    // request would fail outright.
    expect((await chat(`data:image/jpeg;base64,${PNG_B64}`)).status).toBe(200);
    const body = JSON.parse(upstream!.receivedRequests.at(-1)!.body) as {
      messages: Array<{ content: Array<Record<string, unknown>> }>;
    };
    const image = body.messages.at(-1)!.content.find((b) => b.type === "image") as {
      source?: { media_type?: string };
    };
    expect(image.source?.media_type).toBe("image/png");
  }, 60_000);

  test("a declared Anthropic server tool reaches the upstream intact", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }

    const res = await fetch(`${app.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "mm-claude",
        messages: [{ role: "user", content: "what is the weather in NYC?" }],
        // Already Anthropic's own shape — the caller is not asking for a
        // translation, and the optional fields must survive.
        tools: [
          {
            type: "web_search_20250305",
            name: "web_search",
            max_uses: 5,
            allowed_domains: ["weather.example"],
          },
        ],
      }),
    });
    await res.text();
    expect(res.status).toBe(200);

    const body = JSON.parse(upstream!.receivedRequests.at(-1)!.body) as {
      tools?: Array<Record<string, unknown>>;
    };
    expect(
      body.tools,
      "the tool declaration was dropped, so the model answers without the " +
        "tool while the caller believes it searched",
    ).toBeDefined();
    expect(body.tools).toEqual([
      {
        type: "web_search_20250305",
        name: "web_search",
        max_uses: 5,
        allowed_domains: ["weather.example"],
      },
    ]);
  }, 60_000);

  async function chat(imageUrl: string): Promise<{ status: number }> {
    const res = await fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "mm-claude",
        messages: [
          {
            role: "user",
            content: [
              { type: "text", text: "what is in" },
              { type: "image_url", image_url: { url: imageUrl } },
              { type: "text", text: "this picture?" },
            ],
          },
        ],
      }),
    });
    await res.text();
    return { status: res.status };
  }
});
