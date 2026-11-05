import { createHash, randomUUID } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startMcpUpstream,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForSlsLog,
  type McpUpstream,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// A request a guardrail refused is exported under `content_mode: full` so an
// auditor can read what was refused, and what it was refused for stays out of
// the logstore: a blocked keyword and a value a detector blocks are masked like
// a value a mask rule rewrites, and the rest of the text is kept. Every surface
// that runs the chain is driven, because a gap on one of them is silent.
//
// Where the hit cannot be found again in the text, the row carries no body: a
// verdict from a script or a remote service names no span, a check that failed
// open read nothing, and an uploaded file's content is not in the row at all.

const CALLER = "sk-refused-body";
const CREDENTIAL_REF = "mock";
const LOGSTORE = "refused-body";
const KEEP = "kept-context-3f9a1c";
const BLOCK_WORD = "refusedblockword";
const EMAIL = "dana@example.com";
const CN_ID = "11010519491231002X"; // valid ISO 7064 MOD 11-2 check digit
const TEXT = `${KEEP} ${BLOCK_WORD} write to ${EMAIL} about id ${CN_ID}`;

const BLOCK_ALL = `export function checkInput() { return { action: "block", reason_code: "no-span" }; }`;
const FAULT = `export function checkInput() { throw new Error("no verdict"); }`;

const json = (path: string, body: unknown): Request =>
  new Request(`http://gateway${path}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "anthropic-version": "2023-06-01",
      accept: "application/json, text/event-stream",
    },
    body: JSON.stringify(body),
  });
const multipart = (path: string, fields: Record<string, string>, fileField: string, file = "sentinel-bytes"): Request => {
  const form = new FormData();
  for (const [k, v] of Object.entries(fields)) form.set(k, v);
  form.set(fileField, new Blob([file]), "in.bin");
  return new Request(`http://gateway${path}`, { method: "POST", body: form });
};

describe("a refused request's row exports its body with the hits masked", () => {
  let etcdReachable = false;
  let sls: MockSls | undefined;
  let upstream: OpenAiUpstream | undefined;
  let mcp: McpUpstream | undefined;
  let app: SpawnedApp | undefined;
  let fileId = "";

  const OPENAI = "refused-openai";
  const messages = [{ role: "user", content: TEXT }];
  const REFUSED: Array<[string, () => Request]> = [
    ["chat", () => json("/v1/chat/completions", { model: OPENAI, messages })],
    ["chat stream", () => json("/v1/chat/completions", { model: OPENAI, messages, stream: true })],
    ["responses", () => json("/v1/responses", { model: OPENAI, input: TEXT })],
    ["responses stream", () => json("/v1/responses", { model: OPENAI, input: TEXT, stream: true })],
    ["messages", () => json("/v1/messages", { model: OPENAI, max_tokens: 16, messages })],
    ["messages stream", () => json("/v1/messages", { model: OPENAI, max_tokens: 16, messages, stream: true })],
    ["count_tokens", () => json("/v1/messages/count_tokens", { model: "refused-anthropic", messages })],
    ["completions", () => json("/v1/completions", { model: OPENAI, prompt: TEXT })],
    ["completions stream", () => json("/v1/completions", { model: OPENAI, prompt: TEXT, stream: true })],
    ["embeddings", () => json("/v1/embeddings", { model: OPENAI, input: TEXT })],
    ["rerank", () => json("/v1/rerank", { model: "refused-rerank", query: TEXT, documents: ["a"] })],
    ["images", () => json("/v1/images/generations", { model: OPENAI, prompt: TEXT })],
    ["image edits", () => multipart("/v1/images/edits", { model: OPENAI, prompt: TEXT }, "image")],
    ["speech", () => json("/v1/audio/speech", { model: OPENAI, input: TEXT, voice: "alloy" })],
    ["transcription", () => multipart("/v1/audio/transcriptions", { model: OPENAI, prompt: TEXT }, "file")],
    ["videos", () => json("/v1/videos", { model: "refused-video", prompt: TEXT })],
    [
      "batches",
      () =>
        json("/v1/batches", {
          input_file_id: fileId,
          endpoint: "/v1/chat/completions",
          completion_window: "24h",
          metadata: { note: TEXT },
        }),
    ],
    ["fine-tuning", () => json("/v1/fine_tuning/jobs", { model: "gpt-4o-mini", training_file: fileId, suffix: TEXT })],
    ["passthrough", () => json("/refused-route/v1/chat/completions", { model: "m", messages })],
    [
      "a2a",
      () =>
        json("/a2a/refused-agent", {
          jsonrpc: "2.0",
          id: 1,
          method: "message/send",
          params: { message: { role: "user", parts: [{ kind: "text", text: TEXT }] } },
        }),
    ],
    [
      "mcp",
      () =>
        json("/mcp", {
          jsonrpc: "2.0",
          id: 1,
          method: "tools/call",
          params: { name: "refusedmcp__echo", arguments: { text: TEXT } },
        }),
    ],
  ];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    upstream = await startOpenAiUpstream({
      pathBodies: { "/v1/files": { id: "file-refused", object: "file", purpose: "batch" } },
    });
    mcp = await startMcpUpstream("refusedmcp");
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "refused-body",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });

    const modelIds = new Map<string, string>();
    for (const [name, provider, model_name, base, adapter] of [
      [OPENAI, "openai", "gpt-4o-mini", `${upstream.baseUrl}/v1`, undefined],
      ["refused-no-span", "openai", "gpt-4o-mini", `${upstream.baseUrl}/v1`, undefined],
      ["refused-failed-open", "openai", "gpt-4o-mini", `${upstream.baseUrl}/v1`, undefined],
      ["refused-monitored", "openai", "gpt-4o-mini", `${upstream.baseUrl}/v1`, undefined],
      ["refused-rerank", "cohere", "rerank-v3.5", `${upstream.baseUrl}/v1`, "openai"],
      ["refused-anthropic", "anthropic", "claude-sonnet-4-5", upstream.baseUrl, undefined],
      ["refused-video", "alibaba", "wan-mock", upstream.baseUrl, undefined],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        provider,
        ...(adapter ? { adapter } : {}),
        api_base: base,
      });
      const model = await seed.createModel({ display_name: name, provider, model_name, provider_key_id: pk.id });
      modelIds.set(name, model.id);
    }
    const routePk = await seed.createProviderKey({
      display_name: "refused-route-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
    });
    await seed.createPassthroughRoute({
      name: "refused-route",
      path_prefix: "/refused-route",
      target_url: upstream.baseUrl,
      provider_key_id: routePk.id,
    });
    await seed.update("a2a_agents", randomUUID(), {
      name: "refused-agent",
      url: "http://127.0.0.1:1/a2a",
      protocol_version: "0.3",
      auth_type: "none",
      enabled: true,
    });
    const mcpId = randomUUID();
    await seed.update("mcp_servers", mcpId, { display_name: "refusedmcp", url: mcp.url, enabled: true });

    for (const guardrail of [
      {
        name: "refused-keyword",
        enabled: true,
        hook_point: "input",
        kind: "keyword",
        patterns: [{ kind: "literal", value: BLOCK_WORD }],
      },
      {
        name: "refused-pii",
        enabled: true,
        hook_point: "input",
        kind: "pii",
        detectors: [
          { type: "email", action: "mask" },
          { type: "china_id_card", action: "block" },
        ],
      },
    ]) {
      const created = await seed.createGuardrail(guardrail);
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: created.id,
        scope_type: "mcp_server",
        scope_id: mcpId,
        priority: 100,
      });
    }
    for (const [model, guardrail] of [
      ["refused-no-span", { name: "refused-script", fail_open: false, script: BLOCK_ALL }],
      ["refused-failed-open", { name: "refused-script-fault", fail_open: true, script: FAULT }],
      ["refused-monitored", { name: "refused-script-monitored", enforcement_mode: "monitor", script: BLOCK_ALL }],
    ] as const) {
      const created = await seed.createGuardrail(
        { enabled: true, hook_point: "input", kind: "custom", timeout_ms: 5000, ...guardrail },
        { attach: false },
      );
      await seed.update("guardrail_attachments", randomUUID(), {
        guardrail_id: created.id,
        scope_type: "model",
        scope_id: modelIds.get(model),
        // Ahead of the keyword row, so it decides first where it decides at all.
        priority: 200,
      });
    }

    await seed.createApiKey({
      key_hash: createHash("sha256").update(CALLER).digest("hex"),
      allowed_models: ["*"],
      allowed_mcp_servers: ["*"],
      allowed_agents: ["*"],
      allowed_routes: ["*"],
    });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });

    const uploaded = await fetch(
      `${app.proxyUrl}/v1/files`,
      await sendInit(multipart("/v1/files", { model: OPENAI, purpose: "batch" }, "file", "a clean file")),
    );
    expect(uploaded.status, "the clean upload the jobs legs reference").toBe(200);
    fileId = ((await uploaded.json()) as { id: string }).id;
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await upstream?.close();
    await mcp?.close();
  });

  async function sendInit(req: Request): Promise<RequestInit> {
    const headers = new Headers(req.headers);
    headers.set("authorization", `Bearer ${CALLER}`);
    return { method: "POST", headers, body: await req.arrayBuffer() };
  }

  /** The caller's answer, and the row it left. */
  async function send(req: Request): Promise<{ status: number; body: string; row: Map<string, string> }> {
    const res = await fetch(`${app!.proxyUrl}${new URL(req.url).pathname}`, await sendInit(req));
    const body = await res.text();
    const requestId = res.headers.get("x-aisix-request-id");
    expect(requestId).toBeTruthy();
    const row = await waitForSlsLog(sls!, LOGSTORE, (r) => r.get("request_id") === requestId, "the refused row");
    return { status: res.status, body, row };
  }

  const expectHitsAbsent = (text: string, label: string) => {
    for (const hit of [BLOCK_WORD, EMAIL, CN_ID]) expect(text, label).not.toContain(hit);
  };

  const expectNoBody = (row: Map<string, string>, label: string) => {
    expect(row.get("guardrail_blocked"), label).toBe("true");
    expect(row.get("input_messages"), label).toBeUndefined();
    expect(row.get("prompt"), label).toBeUndefined();
    const exported = [...row.values()].join("\n");
    expect(exported, label).not.toContain(KEEP);
    expectHitsAbsent(exported, label);
  };

  test.for(REFUSED)("%s: the hits are masked and the rest is kept", { timeout: 30_000 }, async ([label, make], ctx) => {
    if (!etcdReachable) return ctx.skip();
    const sentBefore = upstream!.receivedRequests.length;
    const { status, body, row } = await send(make());
    // An MCP refusal is a tool result, in a 200.
    expect(status, label).toBe(label === "mcp" ? 200 : 422);
    expectHitsAbsent(body, label);
    expect(upstream!.receivedRequests.length, label).toBe(sentBefore);

    expect(row.get("guardrail_blocked"), label).toBe("true");
    const exported = [...row.values()].join("\n");
    expect(exported, label).toContain(KEEP);
    expectHitsAbsent(exported, label);
  });

  // A verdict with no span, on the families that carry a model.
  test.for([
    ["chat", () => json("/v1/chat/completions", { model: "refused-no-span", messages: [{ role: "user", content: KEEP }] })],
    ["responses", () => json("/v1/responses", { model: "refused-no-span", input: KEEP })],
    ["messages", () => json("/v1/messages", { model: "refused-no-span", max_tokens: 16, messages: [{ role: "user", content: KEEP }] })],
    ["completions", () => json("/v1/completions", { model: "refused-no-span", prompt: KEEP })],
    ["embeddings", () => json("/v1/embeddings", { model: "refused-no-span", input: KEEP })],
  ] as Array<[string, () => Request]>)(
    "%s: a verdict that names no span leaves the row without a body",
    { timeout: 30_000 },
    async ([label, make], ctx) => {
      if (!etcdReachable) return ctx.skip();
      const { status, row } = await send(make());
      expect(status, label).toBe(422);
      expectNoBody(row, label);
    },
  );

  test("a check that failed open leaves the row without a body, whatever blocked it", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const { status, row } = await send(
      json("/v1/chat/completions", { model: "refused-failed-open", messages: [{ role: "user", content: TEXT }] }),
    );
    expect(status).toBe(422);
    expectNoBody(row, "failed open");
  });

  test("a monitored row that cannot locate anything does not stand in the way", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const { status, row } = await send(
      json("/v1/chat/completions", { model: "refused-monitored", messages: [{ role: "user", content: TEXT }] }),
    );
    expect(status).toBe(422);
    expect(row.get("input_messages")).toContain(KEEP);
    expectHitsAbsent([...row.values()].join("\n"), "monitored");
  });

  test("a refused upload's row carries no body: the file was what matched", async (ctx) => {
    if (!etcdReachable) return ctx.skip();
    const before = upstream!.receivedRequests.length;
    const { status, row } = await send(
      multipart("/v1/files", { model: OPENAI, purpose: "batch" }, "file", `${KEEP} ${BLOCK_WORD}`),
    );
    expect(status).toBe(422);
    expect(upstream!.receivedRequests.length).toBe(before);
    expectNoBody(row, "upload");
  });
});
