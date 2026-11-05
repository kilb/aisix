import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  ProxyClient,
  SeedClient,
  spawnApp,
  startMockSls,
  waitConfigPropagation,
  waitForSlsLog,
  type MockSls,
  type SpawnedApp,
} from "../harness/index.js";

// E2E: a streamed request is billed what the upstream reported, whatever the
// caller wrote in `stream_options`.
//
// An OpenAI-protocol upstream sends its usage frame only when asked
// (https://platform.openai.com/docs/api-reference/chat/create#chat_create-stream_options).
// The caller's `include_usage` is a promise about the response the caller
// reads; it is not a say in what the request is billed. A reasoning model's
// visible answer is a small part of what it generates, so a caller who could
// turn the frame off would be billed for the text it can see.

const CALLER = "sk-stream-usage-always-requested";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const CREDENTIAL_REF = "e2e";
const LOGSTORE = "stream-usage-always-requested-store";
const CHAT_MODEL = "usage-always-chat";
const COMPLETION_MODEL = "usage-always-instruct";

// Far more than the few visible tokens of the answer, as a reasoning model's
// count is.
const PROMPT_TOKENS = 11;
const COMPLETION_TOKENS = 500;
const USAGE = {
  prompt_tokens: PROMPT_TOKENS,
  completion_tokens: COMPLETION_TOKENS,
  total_tokens: PROMPT_TOKENS + COMPLETION_TOKENS,
};

function startUpstream(): Server {
  return createServer((req, res) => {
    const parts: Buffer[] = [];
    req.on("data", (c: Buffer) => parts.push(c));
    req.on("end", () => {
      const body = JSON.parse(Buffer.concat(parts).toString("utf8"));
      const wantsUsage = body.stream_options?.include_usage === true;
      const legacy = req.url?.endsWith("/completions") && !req.url.endsWith("/chat/completions");
      const object = legacy ? "text_completion" : "chat.completion.chunk";
      const piece = (choice: Record<string, unknown>) =>
        `data: ${JSON.stringify({ id: "x", object, model: "m", choices: [{ index: 0, ...choice }] })}\n\n`;
      const frames = legacy
        ? [piece({ text: "Paris.", finish_reason: null }), piece({ text: "", finish_reason: "stop" })]
        : [
            piece({ delta: { role: "assistant", content: "Paris." }, finish_reason: null }),
            piece({ delta: {}, finish_reason: "stop" }),
          ];
      if (wantsUsage) {
        frames.push(`data: ${JSON.stringify({ id: "x", object, model: "m", choices: [], usage: USAGE })}\n\n`);
      }
      frames.push("data: [DONE]\n\n");
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.end(frames.join(""));
    });
  });
}

describe("a streamed request is billed from the upstream's usage whatever the caller asked to see", () => {
  let app: SpawnedApp | undefined;
  let upstream: Server | undefined;
  let sls: MockSls | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    upstream = startUpstream();
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const addr = upstream.address();
    if (addr === null || typeof addr === "string") throw new Error("no port");

    sls = await startMockSls();
    app = await spawnApp({
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-ak-id",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-ak-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "stream-usage-always-requested-sls",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "stream-usage-always-requested-proj",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
    });
    const pk = await seed.createProviderKey({
      display_name: "usage-always-pk",
      secret: "sk-mock",
      api_base: `http://127.0.0.1:${addr.port}/v1`,
    });
    for (const display_name of [CHAT_MODEL, COMPLETION_MODEL]) {
      await seed.createModel({
        display_name,
        provider: "openai",
        model_name: "some-model",
        provider_key_id: pk.id,
      });
    }
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: [CHAT_MODEL, COMPLETION_MODEL] });
    const probe = new ProxyClient(app.proxyUrl, CALLER);
    await waitConfigPropagation(async () => (await probe.listModels()).status === 200);
  });

  afterAll(async () => {
    await app?.exit();
    await sls?.close();
    await new Promise<void>((resolve) => (upstream ? upstream.close(() => resolve()) : resolve()));
  });

  const ROUTES = [
    {
      route: "/v1/chat/completions",
      body: { model: CHAT_MODEL, messages: [{ role: "user", content: "Capital of France?" }] },
    },
    { route: "/v1/completions", body: { model: COMPLETION_MODEL, prompt: "Capital of France?" } },
  ];

  async function stream(route: string, body: Record<string, unknown>) {
    const res = await fetch(`${app!.proxyUrl}${route}`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER}`, "content-type": "application/json" },
      body: JSON.stringify({ ...body, stream: true }),
    });
    return {
      status: res.status,
      raw: await res.text(),
      requestId: res.headers.get("x-aisix-request-id") ?? "",
    };
  }

  async function rowOf(requestId: string) {
    return waitForSlsLog(
      sls!,
      LOGSTORE,
      (l) => l.get("request_id") === requestId,
      `usage row for ${requestId}`,
      15_000,
    );
  }

  for (const { route, body } of ROUTES) {
    for (const [label, options] of [
      ["no stream_options", undefined],
      ["include_usage: false", { include_usage: false }],
      ["another option only", { include_obfuscation: false }],
    ] as const) {
      test(`${route} with ${label}: the caller sees no usage, the row has the upstream's`, async (ctx) => {
        if (!etcdReachable || !app) return ctx.skip();
        const out = await stream(route, options ? { ...body, stream_options: options } : body);
        expect(out.status, out.raw).toBe(200);
        expect(out.raw).toContain("Paris.");
        expect(out.raw).toContain("[DONE]");
        expect(out.raw, out.raw).not.toContain('"usage":{');

        const row = await rowOf(out.requestId);
        expect(row.get("prompt_tokens"), JSON.stringify([...row])).toBe(String(PROMPT_TOKENS));
        expect(row.get("completion_tokens"), JSON.stringify([...row])).toBe(String(COMPLETION_TOKENS));
      });
    }

    test(`${route} with include_usage: true: the caller is sent the usage`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const out = await stream(route, { ...body, stream_options: { include_usage: true } });
      expect(out.status, out.raw).toBe(200);
      expect(out.raw).toContain(`"completion_tokens":${COMPLETION_TOKENS}`);

      const row = await rowOf(out.requestId);
      expect(row.get("completion_tokens"), JSON.stringify([...row])).toBe(String(COMPLETION_TOKENS));
    });
  }
});
