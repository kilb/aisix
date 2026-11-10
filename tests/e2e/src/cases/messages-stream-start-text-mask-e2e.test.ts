import { createHash, randomUUID } from "node:crypto";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";

// E2E (R-071): a streamed /v1/messages answer held back for a masking
// guardrail whose text block OPENS with text of its own.
//
// Anthropic's `content_block_start` carries the block, and a text block may
// start non-empty; its deltas then continue the same text. Here the address
// is split between the two: "mail alice@exam" in the start event and
// "ple.com now" in the delta, so only the text read as one piece contains it.
// A remote masking guardrail (Bedrock ANONYMIZE) must rewrite that one piece
// and the answer must reach the caller masked — not refused as a failed
// write-back, and never with the address.

const CALLER = "sk-start-text-mask-caller";
const EMAIL = "alice@example.com";
const EMAIL_RE = /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g;
const hash = (s: string) => createHash("sha256").update(s).digest("hex");

async function listen(server: Server): Promise<string> {
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
}

/** ApplyGuardrail stand-in: every content block with an address comes back
 * with it replaced by `{EMAIL}`, one output per input block. */
function startMockBedrock(): Server {
  return createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      let texts: string[] = [];
      try {
        const body = JSON.parse(raw) as { content?: Array<{ text?: { text?: string } }> };
        texts = (body.content ?? []).map((c) => c.text?.text ?? "");
      } catch {
        texts = [];
      }
      const payload = texts.some((t) => t.replace(EMAIL_RE, "") !== t)
        ? {
            action: "GUARDRAIL_INTERVENED",
            outputs: texts.map((t) => ({ text: t.replace(EMAIL_RE, "{EMAIL}") })),
            assessments: [
              {
                sensitiveInformationPolicy: {
                  piiEntities: [{ match: EMAIL, type: "EMAIL", action: "ANONYMIZED" }],
                  regexes: [],
                },
              },
            ],
          }
        : { action: "NONE", outputs: [] };
      res.statusCode = 200;
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(payload));
    });
  });
}

/** An Anthropic upstream streaming one text block that opens non-empty. */
function startMockAnthropic(): Server {
  return createServer((req, res) => {
    req.on("data", () => {});
    req.on("end", () => {
      const id = `msg_${randomUUID()}`;
      const events: Array<[string, unknown]> = [
        [
          "message_start",
          {
            type: "message_start",
            message: {
              id,
              type: "message",
              role: "assistant",
              model: "claude-3-5-haiku-20241022",
              content: [],
              stop_reason: null,
              usage: { input_tokens: 7, output_tokens: 1 },
            },
          },
        ],
        [
          "content_block_start",
          {
            type: "content_block_start",
            index: 0,
            content_block: { type: "text", text: "mail alice@exam" },
          },
        ],
        [
          "content_block_delta",
          { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "ple.com now" } },
        ],
        ["content_block_stop", { type: "content_block_stop", index: 0 }],
        [
          "message_delta",
          { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 6 } },
        ],
        ["message_stop", { type: "message_stop" }],
      ];
      res.statusCode = 200;
      res.setHeader("content-type", "text/event-stream");
      for (const [event, data] of events) res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
      res.end();
    });
  });
}

describe("a held Messages stream whose text block opens non-empty is masked, not refused", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let bedrock: Server | undefined;
  let upstream: Server | undefined;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    bedrock = startMockBedrock();
    const bedrockUrl = await listen(bedrock);
    upstream = startMockAnthropic();
    const upstreamUrl = await listen(upstream);
    app = await spawnApp({ extra: { bedrock_endpoint_url: bedrockUrl } });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "start-text-anthropic",
      provider: "anthropic",
      adapter: "anthropic",
      secret: "sk-ant-mock",
      api_base: upstreamUrl,
    });
    const model = await seed.createModel({
      display_name: "start-text-mask",
      provider: "anthropic",
      model_name: "claude-3-5-haiku-20241022",
      provider_key_id: pk.id,
    });
    const rule = await seed.createGuardrail(
      {
        name: "start-text-bedrock-mask",
        enabled: true,
        hook_point: "output",
        fail_open: false,
        kind: "bedrock",
        guardrail_id: "starttextgr001",
        guardrail_version: "DRAFT",
        region: "us-east-1",
        aws_credentials: {
          kind: "static",
          access_key_id: "AKIDSTARTTEXTMASK001",
          secret_access_key: "secret-start-text-mask",
        },
        latency_mode: { kind: "serial" },
      },
      { attach: false },
    );
    await seed.attachGuardrailToModel(rule.id as string, model.id as string);
    // The key last: once it authenticates, the rest is in the snapshot.
    await seed.createApiKey({ key_hash: hash(CALLER), allowed_models: ["start-text-mask"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      await res.text();
      return res.status === 200;
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    for (const server of [bedrock, upstream]) {
      server?.closeAllConnections();
      await new Promise<void>((resolve) => (server ? server.close(() => resolve()) : resolve()));
    }
  });

  test("the address split across the start text and a delta reaches the caller masked", async (ctx) => {
    if (!etcdReachable) ctx.skip();
    const res = await fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "start-text-mask",
        max_tokens: 64,
        stream: true,
        messages: [{ role: "user", content: "who do I mail?" }],
      }),
    });
    const body = await res.text();
    expect(res.status, body).toBe(200);
    expect(body, "the address reached the caller").not.toContain("alice@exam");

    // The text the caller reads: the start text, then every delta.
    let text = "";
    let errored = false;
    for (const frame of body.split("\n\n")) {
      const data = frame
        .split("\n")
        .find((line) => line.startsWith("data:"))
        ?.slice(5)
        .trim();
      if (!data) continue;
      const event = JSON.parse(data) as {
        type?: string;
        content_block?: { text?: string };
        delta?: { text?: string };
      };
      if (event.type === "error") errored = true;
      if (event.type === "content_block_start") text += event.content_block?.text ?? "";
      if (event.type === "content_block_delta") text += event.delta?.text ?? "";
    }
    expect(errored, `the answer was refused: ${body}`).toBe(false);
    expect(text).toBe("mail {EMAIL} now");
  });
});
