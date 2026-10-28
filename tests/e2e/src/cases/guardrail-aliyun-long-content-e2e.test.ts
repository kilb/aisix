import { createServer, type Server } from "node:http";
import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  pickFreePort,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：阿里云护栏审得到超过单次上限（2 000 字）的内容。
//
// 阿里云每次调用只收 2 000 字。护栏审的是整段对话，按 system、历史、最新一轮的
// 顺序拼起来；只送前 2 000 字的话，对话一长，最新一轮 —— 调用方这一次真正说的
// 话 —— 就再也审不到；模型回答超过 2 000 字的部分也一样。
//
// 预期：
//   - 最新一条消息无论落在哪里、多长（在调用预算内），都被完整审到；
//   - 最新一条消息本身长到预算装不下：拦下，而不是放行没审过的内容；
//   - 历史再长，一次请求的调用次数有上限，每次调用不超过 2 000 字；
//   - 模型的回答整段都审；
//   - 要脱敏（mask）的内容在长文本里也被改写，其余文字原样、不丢不重；
//   - 请求级字段（tools、user、metadata）再大，也不会顶替掉最新一轮、或把它挤出
//     预算 —— Claude Code 这类客户端每次都带几万字的 tools。
//   - 一组全是这次新送来的内容 —— embeddings 的 input 列表、rerank 的 query 和
//     documents、completions 的 prompt 列表、语音合成的 input 和 instructions、
//     转写的多个 prompt —— 整组都算最新内容：预算内整组都审，超出预算就拦下，
//     不会只审最后一项、把前面的当成历史放过去。

const MARKER = "aliyunlongcontentmarker";
const PHONE = "13800138000";
const PER_CALL_LIMIT = 2_000;

interface MockCall {
  action: string;
  service: string;
  chars: number;
}

/** One stand-in for both Aliyun APIs, keyed on `Action`. */
async function startAliyunMock(): Promise<{ baseUrl: string; calls: MockCall[]; close(): Promise<void> }> {
  const calls: MockCall[] = [];
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      const params = new URLSearchParams(raw);
      const action = params.get("Action") ?? "";
      const service = params.get("Service") ?? "";
      let content = "";
      try {
        const sp = JSON.parse(params.get("ServiceParameters") ?? "{}") as { content?: unknown };
        content = typeof sp.content === "string" ? sp.content : "";
      } catch {
        content = "";
      }
      calls.push({ action, service, chars: Array.from(content).length });
      res.writeHead(200, { "content-type": "application/json" });
      if (action === "MultiModalGuard") {
        const suggestion = content.includes(MARKER) ? "block" : content.includes(PHONE) ? "mask" : "pass";
        const detail =
          suggestion === "mask"
            ? {
                Type: "sensitiveData",
                Suggestion: "mask",
                Result: [
                  {
                    Label: "1814",
                    Ext: { Desensitization: content.replaceAll(PHONE, "[PHONE]"), SensitiveData: [PHONE] },
                  },
                ],
              }
            : { Type: "contentModeration", Suggestion: suggestion, Result: [{ Label: "nonLabel" }] };
        res.end(JSON.stringify({ Code: 200, Message: "OK", RequestId: "LC-AIG", Data: { Detail: [detail], Suggestion: suggestion } }));
        return;
      }
      const risky = content.includes(MARKER);
      res.end(
        JSON.stringify({
          Code: 200,
          Message: "OK",
          RequestId: "LC-TMP",
          Data: {
            RiskLevel: risky ? "high" : "none",
            Result: risky ? [{ Label: "inappropriate_oral", Confidence: 100 }] : [{ Label: "nonLabel" }],
          },
        }),
      );
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((r) => server.listen(port, "127.0.0.1", r));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    calls,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

const filler = (n: number) => "the quick brown fox jumps over the lazy dog. ".repeat(Math.ceil(n / 45)).slice(0, n);

const responsesReply = (text: string) => ({
  id: "resp-long",
  object: "response",
  created_at: 0,
  status: "completed",
  model: "gpt-4o-mini",
  output: [
    {
      type: "message",
      id: "msg-long",
      role: "assistant",
      status: "completed",
      content: [{ type: "output_text", text, annotations: [] }],
    },
  ],
  usage: { input_tokens: 5, output_tokens: 5, total_tokens: 10 },
});

const chatReply = (content: string) => ({
  id: "cmpl-long",
  object: "chat.completion",
  created: 0,
  model: "gpt-4o-mini",
  choices: [{ index: 0, message: { role: "assistant", content }, finish_reason: "stop" }],
  usage: { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10 },
});

const completionReply = (text: string) => ({
  id: "cmpl-legacy-long",
  object: "text_completion",
  created: 0,
  model: "gpt-4o-mini",
  choices: [{ index: 0, text, finish_reason: "stop", logprobs: null }],
  usage: { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10 },
});

const EMBEDDINGS_REPLY = {
  object: "list",
  data: [{ object: "embedding", index: 0, embedding: [0.1, 0.2] }],
  model: "gpt-4o-mini",
  usage: { prompt_tokens: 5, total_tokens: 5 },
};

const RERANK_REPLY = {
  id: "rerank-long",
  results: [{ index: 0, relevance_score: 0.5 }],
  meta: { billed_units: { search_units: 1 } },
};

interface Ctx {
  app?: SpawnedApp;
  aliyun?: Awaited<ReturnType<typeof startAliyunMock>>;
  upstreams: Record<string, OpenAiUpstream>;
  ready: boolean;
}

/** Spawn a gateway with one Aliyun guardrail of `kind` and the given models. */
async function setup(
  kind: "aliyun_text_moderation" | "aliyun_ai_guardrail",
  caller: string,
  replies: Record<string, string>,
): Promise<Ctx> {
  const ctx: Ctx = { upstreams: {}, ready: false };
  const etcd = new EtcdClient();
  if (!(await etcd.ping())) return ctx;
  ctx.aliyun = await startAliyunMock();
  ctx.app = await spawnApp();
  const seed = new SeedClient(etcd, ctx.app.etcdPrefix);
  for (const [name, reply] of Object.entries(replies)) {
    const up = await startOpenAiUpstream({
      nonStreamBody: chatReply(reply),
      pathBodies: {
        "/responses": responsesReply(reply),
        "/v1/completions": completionReply(reply),
        "/embeddings": EMBEDDINGS_REPLY,
        "/rerank": RERANK_REPLY,
        "/audio/speech": "RIFF-audio-bytes",
        "/audio/transcriptions": { text: reply },
      },
    });
    ctx.upstreams[name] = up;
    const pk = await seed.createProviderKey({
      display_name: `${name}-pk`,
      provider: "openai",
      adapter: "openai",
      secret: "sk-mock",
      api_base: `${up.baseUrl}/v1`,
    });
    await seed.createModel({ display_name: name, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: pk.id });
  }
  await seed.createGuardrail({
    name: `${kind}-long`,
    enabled: true,
    hook_point: "both",
    fail_open: false,
    kind,
    region: "cn-shanghai",
    endpoint: ctx.aliyun.baseUrl,
    access_key_id: "LTAI_E2E",
    access_key_secret: "e2e-secret",
    ...(kind === "aliyun_text_moderation" ? { risk_level_threshold: "high" } : {}),
  });
  await seed.createApiKey({
    key_hash: createHash("sha256").update(caller).digest("hex"),
    allowed_models: Object.keys(replies),
  });
  const first = Object.keys(replies)[0]!;
  await waitConfigPropagation(async () => {
    const r = await chat(ctx, caller, first, [{ role: "user", content: `probe ${MARKER}` }]);
    return r.status === 422;
  });
  ctx.ready = true;
  return ctx;
}

async function chat(ctx: Ctx, caller: string, model: string, messages: Array<{ role: string; content: string }>) {
  return post(ctx, caller, "/v1/chat/completions", { model, messages });
}

async function post(ctx: Ctx, caller: string, path: string, body: unknown) {
  const res = await fetch(`${ctx.app!.proxyUrl}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${caller}` },
    body: JSON.stringify(body),
  });
  return { status: res.status, text: await res.text() };
}

type Sent = { status: number; text: string };

/** A transcription request carrying `prompts` as repeated `prompt` fields. */
async function transcribe(ctx: Ctx, caller: string, model: string, prompts: string[]): Promise<Sent> {
  const form = new FormData();
  form.set("model", model);
  form.set("file", new Blob([new Uint8Array([0xff, 0xfb, 0x90, 0x00])], { type: "audio/mpeg" }), "a.mp3");
  for (const prompt of prompts) form.append("prompt", prompt);
  const res = await fetch(`${ctx.app!.proxyUrl}/v1/audio/transcriptions`, {
    method: "POST",
    headers: { authorization: `Bearer ${caller}` },
    body: form,
  });
  return { status: res.status, text: await res.text() };
}

/** `first`, then about `chars` characters of other items, 1,000 characters each. */
const listOf = (first: string, chars: number) => [
  first,
  ...Array.from({ length: Math.ceil(chars / 1_000) }, () => filler(1_000)),
];

/**
 * One request per endpoint whose input is a list of content new in this
 * request, sending `items` in order: the embeddings `input` list, rerank
 * `documents` (and a rerank `query` followed by its documents), a completions
 * `prompt` list, a speech `input` followed by its `instructions`, and repeated
 * transcription `prompt` fields.
 */
const listRequests = (ctx: Ctx, caller: string, model: string, items: string[]) =>
  [
    ["/v1/embeddings input", () => post(ctx, caller, "/v1/embeddings", { model, input: items })],
    [
      "/v1/rerank documents",
      () => post(ctx, caller, "/v1/rerank", { model, query: "which one is relevant", documents: items }),
    ],
    [
      "/v1/rerank query",
      () => post(ctx, caller, "/v1/rerank", { model, query: items[0], documents: items.slice(1) }),
    ],
    ["/v1/completions prompt", () => post(ctx, caller, "/v1/completions", { model, prompt: items })],
    [
      "/v1/audio/speech",
      () =>
        post(ctx, caller, "/v1/audio/speech", {
          model,
          voice: "alloy",
          input: items.join("\n"),
          instructions: "speak slowly",
        }),
    ],
    ["/v1/audio/transcriptions prompt", () => transcribe(ctx, caller, model, items)],
  ] as Array<[string, () => Promise<Sent>]>;

/** A tool declaration as large as the ones coding agents send on every call. */
const BIG_TOOL_DESCRIPTION = filler(31_000);

/**
 * One request per endpoint: `content` as the only user turn, a large tool
 * declaration, and (when `withId`) the small identifier field each API
 * offers — the last thing in the body.
 */
const endpointRequests = (model: string, content: string, withId: boolean) =>
  [
    [
      "/v1/chat/completions",
      {
        model,
        messages: [{ role: "user", content }],
        tools: [
          {
            type: "function",
            function: { name: "lookup", description: BIG_TOOL_DESCRIPTION, parameters: { type: "object", properties: {} } },
          },
        ],
        ...(withId ? { user: "u-1" } : {}),
      },
    ],
    [
      "/v1/messages",
      {
        model,
        max_tokens: 16,
        messages: [{ role: "user", content }],
        tools: [{ name: "lookup", description: BIG_TOOL_DESCRIPTION, input_schema: { type: "object", properties: {} } }],
        ...(withId ? { metadata: { user_id: "u-1" } } : {}),
      },
    ],
    [
      "/v1/responses",
      {
        model,
        input: content,
        tools: [
          { type: "function", name: "lookup", description: BIG_TOOL_DESCRIPTION, parameters: { type: "object", properties: {} } },
        ],
        ...(withId ? { user: "u-1" } : {}),
      },
    ],
  ] as const;

async function teardown(ctx: Ctx) {
  await ctx.app?.exit();
  for (const up of Object.values(ctx.upstreams)) await up.close();
  await ctx.aliyun?.close();
}

/** A conversation whose earlier turns total about `chars` characters. */
const history = (chars: number) => {
  const turns: Array<{ role: string; content: string }> = [{ role: "system", content: "You are a helpful assistant." }];
  for (let left = chars; left > 0; left -= 1_500) {
    turns.push({ role: "user", content: filler(Math.min(750, left)) });
    turns.push({ role: "assistant", content: filler(Math.min(750, Math.max(left - 750, 1))) });
  }
  return turns;
};

describe("Aliyun text moderation checks content past the per-call limit", () => {
  const CALLER = "sk-aliyun-long-tmp";
  let ctx: Ctx = { upstreams: {}, ready: false };

  beforeAll(async () => {
    ctx = await setup("aliyun_text_moderation", CALLER, {
      "lc-tmp-clean": "a short and clean reply",
      "lc-tmp-long-risky-reply": `${filler(6_000)} ${MARKER}`,
    });
  }, 60_000);
  afterAll(async () => teardown(ctx));

  const calls = () => ctx.aliyun!.calls;

  test("the newest message is checked however long the history before it", async (t) => {
    if (!ctx.ready) return t.skip();
    const r = await chat(ctx, CALLER, "lc-tmp-clean", [...history(5_000), { role: "user", content: `now ${MARKER} please` }]);
    expect(r.status).toBe(422);
  });

  test("a phrase anywhere in a long newest message is found, cuts included", async (t) => {
    if (!ctx.ready) return t.skip();
    const length = 6_000;
    for (let at = 0; at < length; at += 241) {
      const body = filler(length);
      const content = body.slice(0, at) + MARKER + body.slice(at);
      const r = await chat(ctx, CALLER, "lc-tmp-clean", [{ role: "user", content }]);
      expect(r.status, `phrase at char ${at}`).toBe(422);
    }
  });

  test("a newest message too long for the call budget is refused, not passed unchecked", async (t) => {
    if (!ctx.ready) return t.skip();
    const before = calls().length;
    const r = await chat(ctx, CALLER, "lc-tmp-clean", [{ role: "user", content: filler(60_000) }]);
    expect(r.status).toBe(422);
    expect(calls().length - before).toBeLessThanOrEqual(16);
  });

  test("a very long history stays within a bounded number of calls", async (t) => {
    if (!ctx.ready) return t.skip();
    const before = calls().length;
    const r = await chat(ctx, CALLER, "lc-tmp-clean", [...history(100_000), { role: "user", content: "and one more question" }]);
    expect(r.status, r.text.slice(0, 200)).toBe(200);
    const inputCalls = calls().slice(before).filter((c) => c.service === "llm_query_moderation");
    expect(inputCalls.length).toBeGreaterThan(1);
    expect(inputCalls.length).toBeLessThanOrEqual(16);
  });

  test("the whole of a long reply is checked", async (t) => {
    if (!ctx.ready) return t.skip();
    const r = await chat(ctx, CALLER, "lc-tmp-long-risky-reply", [{ role: "user", content: "write a long essay" }]);
    expect(r.status).toBe(422);
    expect(r.text).not.toContain(MARKER);
  });

  test("large tools and a trailing identifier do not take the newest turn's place", async (t) => {
    if (!ctx.ready) return t.skip();
    for (const [path, body] of endpointRequests("lc-tmp-clean", `please ${MARKER} now`, true)) {
      const r = await post(ctx, CALLER, path, body);
      expect(r.status, `${path}: ${r.text.slice(0, 200)}`).toBe(422);
    }
  });

  test("/v1/completions: `user` and `suffix` do not take the prompt's place", async (t) => {
    if (!ctx.ready) return t.skip();
    const prompt = `${MARKER} ${filler(40_000)}`;
    for (const extra of [{ user: "u-1" }, { suffix: "x" }]) {
      const r = await post(ctx, CALLER, "/v1/completions", { model: "lc-tmp-clean", prompt, ...extra });
      expect(r.status, `${JSON.stringify(extra)}: ${r.text.slice(0, 200)}`).toBe(422);
    }
  });

  test("a list of new content within the budget is checked whole", async (t) => {
    if (!ctx.ready) return t.skip();
    for (const [label, send] of listRequests(ctx, CALLER, "lc-tmp-clean", listOf("a clean first item", 20_000))) {
      const r = await send();
      expect.soft(r.status, `clean ${label}: ${r.text.slice(0, 200)}`).toBe(200);
    }
    for (const [label, send] of listRequests(ctx, CALLER, "lc-tmp-clean", listOf(`first ${MARKER} item`, 20_000))) {
      const r = await send();
      expect.soft(r.status, `phrase in the first item, ${label}: ${r.text.slice(0, 200)}`).toBe(422);
    }
  });

  test("a list of new content too long for the budget is refused, not checked from its end only", async (t) => {
    if (!ctx.ready) return t.skip();
    for (const [label, send] of listRequests(ctx, CALLER, "lc-tmp-clean", listOf(`first ${MARKER} item`, 40_000))) {
      const before = calls().length;
      const r = await send();
      expect.soft(r.status, `${label}: ${r.text.slice(0, 200)}`).toBe(422);
      expect.soft(calls().length - before, label).toBeLessThanOrEqual(16);
    }
  });

  test("large tools alone do not get a clean request refused", async (t) => {
    if (!ctx.ready) return t.skip();
    for (const [path, body] of endpointRequests("lc-tmp-clean", "what is the weather today", false)) {
      const r = await post(ctx, CALLER, path, body);
      expect(r.status, `${path}: ${r.text.slice(0, 200)}`).toBe(200);
    }
  });

  test("no call exceeds the documented per-call limit", async (t) => {
    if (!ctx.ready) return t.skip();
    expect(calls().length).toBeGreaterThan(0);
    expect(Math.max(...calls().map((c) => c.chars))).toBeLessThanOrEqual(PER_CALL_LIMIT);
  });
});

describe("Aliyun AI guardrail masks content past the per-call limit", () => {
  const CALLER = "sk-aliyun-long-aig";
  let ctx: Ctx = { upstreams: {}, ready: false };

  beforeAll(async () => {
    ctx = await setup("aliyun_ai_guardrail", CALLER, { "lc-aig-clean": "a short and clean reply" });
  }, 60_000);
  afterAll(async () => teardown(ctx));

  const forwardedMessages = () =>
    (JSON.parse(ctx.upstreams["lc-aig-clean"]!.receivedRequests.at(-1)?.body ?? "{}") as {
      messages?: Array<{ role: string; content: unknown }>;
    }).messages ?? [];

  test("a phone number in the newest message is masked after a long history", async (t) => {
    if (!ctx.ready) return t.skip();
    const turns = history(3_000);
    const newest = `please call me back on ${PHONE} tomorrow`;
    const r = await chat(ctx, CALLER, "lc-aig-clean", [...turns, { role: "user", content: newest }]);
    expect(r.status, r.text.slice(0, 200)).toBe(200);
    const sent = forwardedMessages();
    expect(sent.at(-1)?.content).toBe(newest.replace(PHONE, "[PHONE]"));
    // The history reaches the model as written.
    expect(sent.slice(0, -1).map((m) => m.content)).toEqual(turns.map((m) => m.content));
  });

  test("a phone number deep inside one long message is masked, the rest kept exactly", async (t) => {
    if (!ctx.ready) return t.skip();
    const content = `${filler(4_500)} reach me at ${PHONE}. ${filler(300)}`;
    const r = await chat(ctx, CALLER, "lc-aig-clean", [{ role: "user", content }]);
    expect(r.status, r.text.slice(0, 200)).toBe(200);
    expect(forwardedMessages().at(-1)?.content).toBe(content.replace(PHONE, "[PHONE]"));
  });

  test("the whole newest message is checked, not just its last block", async (t) => {
    if (!ctx.ready) return t.skip();
    const before = ctx.upstreams["lc-aig-clean"]!.receivedRequests.length;
    const r = await post(ctx, CALLER, "/v1/messages", {
      model: "lc-aig-clean",
      max_tokens: 16,
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: `${PHONE} ${filler(40_000)}` },
            { type: "text", text: "." },
          ],
        },
      ],
    });
    // Too long to check in full: refused rather than passed unchecked.
    expect(r.status, r.text.slice(0, 200)).toBe(422);
    const sent = ctx.upstreams["lc-aig-clean"]!.receivedRequests.slice(before);
    expect(sent.some((q) => q.body.includes(PHONE))).toBe(false);
  });

  test("a phone number in the newest turn is masked next to large tools", async (t) => {
    if (!ctx.ready) return t.skip();
    const content = `call me on ${PHONE} please`;
    for (const [path, body] of endpointRequests("lc-aig-clean", content, true).slice(0, 2)) {
      const r = await post(ctx, CALLER, path, body);
      expect(r.status, `${path}: ${r.text.slice(0, 200)}`).toBe(200);
      const sent = JSON.stringify(forwardedMessages());
      expect(sent, path).not.toContain(PHONE);
      expect(sent, path).toContain("[PHONE]");
    }
  });

  test("a number in the first item of a list within the budget never reaches the model", async (t) => {
    if (!ctx.ready) return t.skip();
    const up = ctx.upstreams["lc-aig-clean"]!;
    // Endpoints that rewrite a request body send it on masked; the others
    // refuse it.
    const masking = new Set(["/v1/rerank documents", "/v1/rerank query", "/v1/completions prompt", "/v1/audio/speech"]);
    for (const [label, send] of listRequests(ctx, CALLER, "lc-aig-clean", listOf(`call ${PHONE} now`, 20_000))) {
      const before = up.receivedRequests.length;
      const r = await send();
      const sent = up.receivedRequests.slice(before).map((q) => q.body).join("\n");
      expect.soft(sent, label).not.toContain(PHONE);
      if (masking.has(label)) {
        expect.soft(r.status, `${label}: ${r.text.slice(0, 200)}`).toBe(200);
        expect.soft(sent, label).toContain("[PHONE]");
      }
    }
  });

  test("a list of new content too long for the budget is refused, not sent with the number", async (t) => {
    if (!ctx.ready) return t.skip();
    const up = ctx.upstreams["lc-aig-clean"]!;
    for (const [label, send] of listRequests(ctx, CALLER, "lc-aig-clean", listOf(`call ${PHONE} now`, 40_000))) {
      const before = up.receivedRequests.length;
      const r = await send();
      expect.soft(r.status, `${label}: ${r.text.slice(0, 200)}`).toBe(422);
      const sent = up.receivedRequests.slice(before).map((q) => q.body);
      expect.soft(sent.some((body) => body.includes(PHONE)), label).toBe(false);
    }
  });

  test("no call exceeds the documented per-call limit", async (t) => {
    if (!ctx.ready) return t.skip();
    expect(Math.max(...ctx.aliyun!.calls.map((c) => c.chars))).toBeLessThanOrEqual(PER_CALL_LIMIT);
  });
});
