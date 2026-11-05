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

// Under `content_mode: full` a failure row exports the request's body, and a
// request an enforcing input rule applies to exports only what that rule has
// processed. A chain that calls out reserves its rate limit ahead of the check,
// so a key at its limit is turned away with the rule never having run: that row
// carries no body at all, on every surface, not the text as the caller sent it.

const CALLER = "sk-unscreened-body";
const CREDENTIAL_REF = "mock";
const LOGSTORE = "unscreened-body";
const KEEP = "kept-context-81b2";
const EMAIL = "erin@example.com";
const TEXT = `${KEEP} write to ${EMAIL}`;

const ALLOW = `export function checkInput() { return { action: "none" }; }`;

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
const multipart = (path: string, fields: Record<string, string>, fileField: string): Request => {
  const form = new FormData();
  for (const [k, v] of Object.entries(fields)) form.set(k, v);
  form.set(fileField, new Blob(["a clean file"]), "in.bin");
  return new Request(`http://gateway${path}`, { method: "POST", body: form });
};

describe("a request turned away ahead of its chain exports no unmasked body", () => {
  let etcdReachable = false;
  let sls: MockSls | undefined;
  let upstream: OpenAiUpstream | undefined;
  let mcp: McpUpstream | undefined;
  let app: SpawnedApp | undefined;
  let fileId = "";

  const OPENAI = "unscreened-openai";
  const messages = [{ role: "user", content: TEXT }];
  const SURFACES: Array<[string, () => Request]> = [
    ["chat", () => json("/v1/chat/completions", { model: OPENAI, messages })],
    ["chat stream", () => json("/v1/chat/completions", { model: OPENAI, messages, stream: true })],
    ["responses", () => json("/v1/responses", { model: OPENAI, input: TEXT })],
    ["messages", () => json("/v1/messages", { model: OPENAI, max_tokens: 16, messages })],
    ["count_tokens", () => json("/v1/messages/count_tokens", { model: "unscreened-anthropic", messages })],
    ["completions", () => json("/v1/completions", { model: OPENAI, prompt: TEXT })],
    ["embeddings", () => json("/v1/embeddings", { model: OPENAI, input: TEXT })],
    ["rerank", () => json("/v1/rerank", { model: "unscreened-rerank", query: TEXT, documents: ["a"] })],
    ["images", () => json("/v1/images/generations", { model: OPENAI, prompt: TEXT })],
    ["image edits", () => multipart("/v1/images/edits", { model: OPENAI, prompt: TEXT }, "image")],
    ["speech", () => json("/v1/audio/speech", { model: OPENAI, input: TEXT, voice: "alloy" })],
    ["transcription", () => multipart("/v1/audio/transcriptions", { model: OPENAI, prompt: TEXT }, "file")],
    ["videos", () => json("/v1/videos", { model: "unscreened-video", prompt: TEXT })],
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
    ["passthrough", () => json("/unscreened-route/v1/chat/completions", { model: "m", messages })],
    [
      "a2a",
      () =>
        json("/a2a/unscreened-agent", {
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
          params: { name: "unscreenedmcp__echo", arguments: { text: TEXT } },
        }),
    ],
  ];
  const limitedKey = (label: string) => `${CALLER}-${label.replace(/\W/g, "-")}`;
  const sha256 = (s: string) => createHash("sha256").update(s).digest("hex");

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    sls = await startMockSls();
    upstream = await startOpenAiUpstream({
      pathBodies: { "/v1/files": { id: "file-unscreened", object: "file", purpose: "batch" } },
    });
    mcp = await startMcpUpstream("unscreenedmcp");
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "unscreened-body",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });

    for (const [name, provider, model_name, base, adapter] of [
      [OPENAI, "openai", "gpt-4o-mini", `${upstream.baseUrl}/v1`, undefined],
      ["unscreened-rerank", "cohere", "rerank-v3.5", `${upstream.baseUrl}/v1`, "openai"],
      ["unscreened-anthropic", "anthropic", "claude-sonnet-4-5", upstream.baseUrl, undefined],
      ["unscreened-video", "alibaba", "wan-mock", upstream.baseUrl, undefined],
    ] as const) {
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        secret: "sk-mock",
        provider,
        ...(adapter ? { adapter } : {}),
        api_base: base,
      });
      await seed.createModel({ display_name: name, provider, model_name, provider_key_id: pk.id });
    }
    const routePk = await seed.createProviderKey({
      display_name: "unscreened-route-pk",
      secret: "sk-mock",
      api_base: upstream.baseUrl,
    });
    await seed.createPassthroughRoute({
      name: "unscreened-route",
      path_prefix: "/unscreened-route",
      target_url: upstream.baseUrl,
      provider_key_id: routePk.id,
    });
    await seed.update("a2a_agents", randomUUID(), {
      name: "unscreened-agent",
      url: "http://127.0.0.1:1/a2a",
      protocol_version: "0.3",
      auth_type: "none",
      enabled: true,
    });
    const mcpId = randomUUID();
    await seed.update("mcp_servers", mcpId, { display_name: "unscreenedmcp", url: mcp.url, enabled: true });

    // The mask rule is what the row answers to; the script row is what makes
    // this chain reserve its rate limit ahead of the check.
    for (const guardrail of [
      {
        name: "unscreened-pii",
        enabled: true,
        hook_point: "input",
        kind: "pii",
        detectors: [{ type: "email", action: "mask" }],
      },
      {
        name: "unscreened-script",
        enabled: true,
        hook_point: "input",
        kind: "custom",
        timeout_ms: 5000,
        fail_open: false,
        script: ALLOW,
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

    const access = {
      allowed_models: ["*"],
      allowed_mcp_servers: ["*"],
      allowed_agents: ["*"],
      allowed_routes: ["*"],
    };
    for (const [label] of SURFACES) {
      await seed.createApiKey({ key_hash: sha256(limitedKey(label)), ...access, rate_limit: { rpm: 1 } });
    }
    await seed.createApiKey({ key_hash: sha256(CALLER), ...access });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });

    const uploaded = await fetch(
      `${app.proxyUrl}/v1/files`,
      await init(multipart("/v1/files", { model: OPENAI, purpose: "batch" }, "file"), CALLER),
    );
    expect(uploaded.status, "the clean upload the batches leg references").toBe(200);
    fileId = ((await uploaded.json()) as { id: string }).id;
  }, 120_000);

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await upstream?.close();
    await mcp?.close();
  });

  async function init(req: Request, key: string): Promise<RequestInit> {
    const headers = new Headers(req.headers);
    headers.set("authorization", `Bearer ${key}`);
    return { method: "POST", headers, body: await req.arrayBuffer() };
  }

  test.for(SURFACES)("%s: a 429 from the reservation carries no body", { timeout: 30_000 }, async ([label, make], ctx) => {
    if (!etcdReachable) return ctx.skip();
    // The first call takes the key's slot, whatever it answers. More than two
    // only when the window rolled between them.
    let refused: Response | undefined;
    for (let i = 0; i < 4 && !refused; i++) {
      const req = make();
      const res = await fetch(`${app!.proxyUrl}${new URL(req.url).pathname}`, await init(req, limitedKey(label)));
      if (res.status === 429) refused = res;
      await res.text();
    }
    expect(refused, `${label}: the key's rpm refused a call`).toBeDefined();
    const requestId = refused!.headers.get("x-aisix-request-id");
    expect(requestId, label).toBeTruthy();
    const row = await waitForSlsLog(sls!, LOGSTORE, (r) => r.get("request_id") === requestId, `${label}: the 429 row`);

    expect(row.get("status_code"), label).toBe("429");
    expect(row.get("input_messages"), label).toBeUndefined();
    expect(row.get("prompt"), label).toBeUndefined();
    const exported = [...row.values()].join("\n");
    expect(exported, label).not.toContain(EMAIL);
    expect(exported, label).not.toContain(KEEP);
    // The row is still a full one: it says what the caller was told.
    expect(row.get("response"), label).toBeTruthy();
  });
});
