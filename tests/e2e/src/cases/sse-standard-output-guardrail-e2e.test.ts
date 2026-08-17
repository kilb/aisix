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

// WHATWG SSE permits an initial UTF-8 BOM, CR/LF/CRLF line endings, and
// multiple `data` fields joined with a newline. These forms must receive the
// same held-output policy as the common single-line LF encoding. Exact raw
// transport chunks deliberately split fields and delimiters at arbitrary byte
// boundaries; the requests still traverse the real gateway and HTTP upstream.

const CALLER = "sk-sse-standard-guardrail";
const HASH = createHash("sha256").update(CALLER).digest("hex");
const FORBIDDEN = "forbiddenstandardtoken";

function fragment(raw: string): string[] {
  const chunks: string[] = [];
  const widths = [1, 2, 5, 3, 8, 1, 13];
  for (let offset = 0, index = 0; offset < raw.length; index++) {
    const next = offset + widths[index % widths.length]!;
    chunks.push(raw.slice(offset, next));
    offset = next;
  }
  return chunks;
}

const completionsSse =
  "\uFEFFevent: completion\r" +
  'data: {"id":"cmpl-standard","choices":\r' +
  'data: [{"index":0,"text":"forbiddenstandard"}]}\r\r' +
  "event: completion\r" +
  'data: {"id":"cmpl-standard","choices":[{"index":0,"text":"token","finish_reason":"stop"}]}\r\r' +
  "data: [DONE]\r\r";

const messagesSse =
  "\uFEFFevent: content_block_start\r" +
  'data: {"type":"content_block_start","index":0,\r' +
  'data: "content_block":{"type":"text","text":""}}\r\r' +
  "event: content_block_delta\r" +
  'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"forbiddenstandard"}}\r\r' +
  "event: content_block_delta\r" +
  'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"token"}}\r\r' +
  'event: message_stop\rdata: {"type":"message_stop"}\r\r';

const responsesSse =
  "\uFEFFevent: response.created\r" +
  'data: {"type":"response.created",\r' +
  'data: "response":{"id":"resp-standard"}}\r\r' +
  "event: response.output_text.delta\r" +
  'data: {"type":"response.output_text.delta","item_id":"m","delta":"forbiddenstandard"}\r\r' +
  "event: response.output_text.delta\r" +
  'data: {"type":"response.output_text.delta","item_id":"m","delta":"token"}\r\r' +
  "data: [DONE]\r\r";

describe("standards-compliant SSE output guardrails", () => {
  let app: SpawnedApp | undefined;
  let completions: OpenAiUpstream | undefined;
  let messages: OpenAiUpstream | undefined;
  let responses: OpenAiUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    completions = await startOpenAiUpstream({
      rawSseChunks: fragment(completionsSse),
    });
    messages = await startOpenAiUpstream({ rawSseChunks: fragment(messagesSse) });
    responses = await startOpenAiUpstream({ rawSseChunks: fragment(responsesSse) });
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    for (const [name, provider, apiBase] of [
      ["standard-completions", "openai", `${completions.baseUrl}/v1`],
      ["standard-messages", "anthropic", messages.baseUrl],
      ["standard-responses", "openai", `${responses.baseUrl}/v1`],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        api_base: apiBase,
      });
      await seed.createModel({
        display_name: name,
        provider,
        model_name: "gpt-4o-mini",
        provider_key_id: pk.id,
      });
    }
    await seed.createGuardrail({
      name: "standard-sse-output",
      enabled: true,
      hook_point: "output",
      kind: "keyword",
      patterns: [{ kind: "literal", value: FORBIDDEN }],
    });
    // Last revision: successful authentication is a barrier for all models
    // and the guardrail above, without exercising the behavior under test.
    await seed.createApiKey({
      key_hash: HASH,
      allowed_models: [
        "standard-completions",
        "standard-messages",
        "standard-responses",
      ],
    });
    await waitConfigPropagation(async () => {
      const result = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      if (result.status === 401) return false;
      if (result.status !== 200) {
        throw new Error(`model propagation probe returned ${result.status}`);
      }
      const body = (await result.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((model) => model.id));
      return [
        "standard-completions",
        "standard-messages",
        "standard-responses",
      ].every((model) => ids.has(model));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all([
      completions?.close(),
      messages?.close(),
      responses?.close(),
    ]);
  });

  test("legacy completions blocks without releasing raw SSE bytes", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const marker = "standard-completions-contract-marker";
    const baseline = completions!.receivedRequests.length;
    const result = await fetch(`${app.proxyUrl}/v1/completions`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "standard-completions",
        prompt: marker,
        stream: true,
      }),
    });
    expect(result.status).toBe(422);
    const body = await result.text();
    expect(body).toContain("content_filter");
    expect(body).not.toContain("forbiddenstandard");
    expect(body).not.toContain("cmpl-standard");
    expect(completions!.receivedRequests.length).toBe(baseline + 1);
    expect(completions!.receivedRequests[baseline]?.path).toBe(
      "/v1/completions",
    );
    expect(completions!.receivedRequests[baseline]?.body).toContain(marker);
  });

  test("Messages blocks in-band without releasing raw SSE bytes", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const marker = "standard-messages-contract-marker";
    const baseline = messages!.receivedRequests.length;
    const result = await fetch(`${app.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: {
        "x-api-key": CALLER,
        "anthropic-version": "2023-06-01",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "standard-messages",
        messages: [{ role: "user", content: marker }],
        max_tokens: 16,
        stream: true,
      }),
    });
    expect(result.status).toBe(200);
    const body = await result.text();
    expect(body).toContain("content_filter");
    expect(body).not.toContain("forbiddenstandard");
    expect(body).not.toContain("content_block_start");
    expect(body).not.toContain("content_block_delta");
    expect(body).not.toContain("message_stop");
    expect(body).not.toContain('"text":"token"');
    expect(messages!.receivedRequests.length).toBe(baseline + 1);
    expect(messages!.receivedRequests[baseline]?.path).toBe("/v1/messages");
    expect(messages!.receivedRequests[baseline]?.body).toContain(marker);
  });

  test("Responses blocks without releasing raw SSE bytes", async (ctx) => {
    if (!etcdReachable || !app) {
      ctx.skip();
      return;
    }
    const marker = "standard-responses-contract-marker";
    const baseline = responses!.receivedRequests.length;
    const result = await fetch(`${app.proxyUrl}/v1/responses`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${CALLER}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({
        model: "standard-responses",
        input: marker,
        stream: true,
      }),
    });
    expect(result.status).toBe(422);
    const body = await result.text();
    expect(body).toContain("content_filter");
    expect(body).not.toContain("forbiddenstandard");
    expect(body).not.toContain("resp-standard");
    expect(responses!.receivedRequests.length).toBe(baseline + 1);
    expect(responses!.receivedRequests[baseline]?.path).toBe("/v1/responses");
    expect(responses!.receivedRequests[baseline]?.body).toContain(marker);
  });
});
