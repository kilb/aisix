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

// E2E：流式 /v1/messages 上，按窗口审核（window 模式，Azure / 阿里云护栏的默认
// 模式）的输出护栏拦下的内容，一个字也不能先到调用方手里。
//
// 窗口模式的意思是「审过才放」：内容先扣住，审干净的部分才发出去。
// /v1/chat/completions 与 /v1/responses 都这么做；/v1/messages 要是照样实时
// 转发、最后再补一个 error 事件，违规内容早已整段送达 —— Claude Code 这类调用方
// 已经把它显示出来了。
//
// 两条路径都要守住：上游是 Anthropic（原样转发 SSE）与上游是 OpenAI（转成
// Anthropic 事件）。对照：干净的回复照常完整送达。

const CALLER = "sk-msg-window-holdback";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const MARKER = "windowholdbackmarker";

const anthropicEvents = (text: string) => [
  JSON.stringify({
    type: "message_start",
    message: {
      id: "msg_wh",
      type: "message",
      role: "assistant",
      content: [],
      model: "claude-3-5-haiku-20241022",
      stop_reason: null,
      usage: { input_tokens: 5, output_tokens: 1 },
    },
  }),
  JSON.stringify({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: text.slice(0, 12) } }),
  JSON.stringify({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: text.slice(12) } }),
  JSON.stringify({ type: "content_block_stop", index: 0 }),
  JSON.stringify({ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 12 } }),
  JSON.stringify({ type: "message_stop" }),
];

const openAiEvents = (text: string) => [
  JSON.stringify({ id: "c", object: "chat.completion.chunk", model: "gpt-4o-mini", choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] }),
  JSON.stringify({ id: "c", object: "chat.completion.chunk", model: "gpt-4o-mini", choices: [{ index: 0, delta: { content: text.slice(0, 12) }, finish_reason: null }] }),
  JSON.stringify({ id: "c", object: "chat.completion.chunk", model: "gpt-4o-mini", choices: [{ index: 0, delta: { content: text.slice(12) }, finish_reason: null }] }),
  JSON.stringify({ id: "c", object: "chat.completion.chunk", model: "gpt-4o-mini", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] }),
  "[DONE]",
];

const RISKY_TEXT = `here is the ${MARKER} the reply carries, then more words`;
const CLEAN_TEXT = "a perfectly clean reply with enough words to window";

/** Aliyun text moderation stand-in: high risk when the content has the marker. */
async function startModeration(): Promise<{ baseUrl: string; close(): Promise<void> }> {
  const server: Server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c: Buffer) => (raw += c.toString("utf8")));
    req.on("end", () => {
      const params = new URLSearchParams(raw);
      let content = "";
      try {
        const sp = JSON.parse(params.get("ServiceParameters") ?? "{}") as { content?: unknown };
        content = typeof sp.content === "string" ? sp.content : "";
      } catch {
        content = "";
      }
      const risky = content.includes(MARKER);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          Code: 200,
          Message: "OK",
          RequestId: "WH-REQ",
          Data: {
            RiskLevel: risky ? "high" : "none",
            Result: risky ? [{ Label: "inappropriate_oral", Confidence: 100.0 }] : [{ Label: "nonLabel" }],
          },
        }),
      );
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((r) => server.listen(port, "127.0.0.1", r));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

describe("streamed /v1/messages holds window-moderated output back until it scans clean", () => {
  let app: SpawnedApp | undefined;
  let moderation: Awaited<ReturnType<typeof startModeration>> | undefined;
  const upstreams: OpenAiUpstream[] = [];
  let etcdReachable = false;

  const stream = (model: string) =>
    fetch(`${app!.proxyUrl}/v1/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": CALLER },
      body: JSON.stringify({
        model,
        max_tokens: 64,
        stream: true,
        messages: [{ role: "user", content: "tell me something" }],
      }),
    });

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    moderation = await startModeration();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);

    const models: Array<[string, string, string, string[], string]> = [
      ["wh-anthropic-risky", "anthropic", "anthropic", anthropicEvents(RISKY_TEXT), ""],
      ["wh-anthropic-clean", "anthropic", "anthropic", anthropicEvents(CLEAN_TEXT), ""],
      ["wh-openai-risky", "openai", "openai", openAiEvents(RISKY_TEXT), "/v1"],
      ["wh-openai-clean", "openai", "openai", openAiEvents(CLEAN_TEXT), "/v1"],
    ];
    for (const [name, provider, adapter, events, suffix] of models) {
      const up = await startOpenAiUpstream({ streamEvents: events, eventDelayMs: 20 });
      upstreams.push(up);
      const pk = await seed.createProviderKey({
        display_name: `${name}-pk`,
        provider,
        adapter,
        secret: "sk-mock",
        api_base: `${up.baseUrl}${suffix}`,
      });
      await seed.createModel({
        display_name: name,
        provider,
        model_name: provider === "anthropic" ? "claude-3-5-haiku-20241022" : "gpt-4o-mini",
        provider_key_id: pk.id,
      });
    }
    await seed.createGuardrail({
      name: "wh-aliyun-window",
      enabled: true,
      hook_point: "output",
      fail_open: false,
      kind: "aliyun_text_moderation",
      region: "cn-shanghai",
      endpoint: moderation.baseUrl,
      access_key_id: "LTAI_E2E",
      access_key_secret: "e2e-secret",
      risk_level_threshold: "high",
      stream_processing_mode: "window",
      window_size: 16,
      window_overlap_size: 4,
    });
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: models.map(([n]) => n) });

    // 就绪：护栏已生效 —— 有风险的回复不再整段干净地结束。
    await waitConfigPropagation(async () => {
      const r = await stream("wh-anthropic-risky");
      const wire = await r.text();
      return r.status === 200 && wire.includes("error");
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    for (const up of upstreams) await up.close();
    await moderation?.close();
  });

  for (const upstream of ["anthropic", "openai"] as const) {
    test(`${upstream} upstream: the flagged reply never reaches the caller`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const res = await stream(`wh-${upstream}-risky`);
      const wire = await res.text();
      expect(wire, "flagged text was forwarded before the verdict").not.toContain(MARKER);
      expect(wire).not.toContain(RISKY_TEXT.slice(0, 12));
      // 调用方知道回复被拦了，而不是看到一次空的正常结束。
      expect(res.status !== 200 || wire.includes("event: error")).toBe(true);
    });

    test(`${upstream} upstream: a clean reply still arrives in full`, async (ctx) => {
      if (!etcdReachable || !app) return ctx.skip();
      const res = await stream(`wh-${upstream}-clean`);
      expect(res.status).toBe(200);
      const wire = await res.text();
      const text = wire
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => {
          try {
            return JSON.parse(l.slice(5)) as { type?: string; delta?: { type?: string; text?: string } };
          } catch {
            return {};
          }
        })
        .filter((e) => e.type === "content_block_delta" && e.delta?.type === "text_delta")
        .map((e) => e.delta?.text ?? "")
        .join("");
      expect(text).toBe(CLEAN_TEXT);
      expect(wire).not.toContain("event: error");
    });
  }
});
