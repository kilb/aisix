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
import { metricDelta, scrapeMetrics, type MetricSample } from "../harness/metrics.js";
import { startMockOtlp, type MockOtlp } from "../harness/otlp-mock.js";

// E2E：/v1/responses 和 /v1/completions 的流式请求，要像 chat 和 messages
// 一样在两条 TTFT 序列里各记一次——直方图 `aisix_request_ttft_seconds` 给
// SLO 分位数用，summary `aisix_llm_time_to_first_token_seconds` 是旧面板在读。
// Codex 一类客户端只走 /v1/responses，而且永远是流式；这两个端点一条都不记
// 的话，它们的首字延迟在 Prometheus 里就是一片空白，看上去和“没有流量”无法区分。
//
// 覆盖每一条流式分支：
//   - /v1/responses 原生透传（OpenAI 提供商，字节原样转发）
//   - /v1/responses 桥接（非 OpenAI 提供商，chat SSE 重新编码成 Responses SSE）
//   - /v1/completions 流式
// 以及上面三条在“输出护栏整段扣留（hold-back）”下各自的分支——扣留路径
// 有自己的收尾代码，和实时转发不共用。扣留时调用方要等整段扫描完，但上游的
// 首帧照样在 LEAD_MS 左右到达，TTFT 必须报这个时刻，而不是放行时刻。
//
// 同一个数也要出现在这次请求的用量事件里（`upstream_ttft_ms`）：日志和指标
// 报的是同一次测量，不能一边有一边是空的。
//
// 非流式请求没有“首帧”可言，不应该在 TTFT 序列里留下观测。

const CALLER_PLAINTEXT = "sk-ttft-resp-cmpl-metrics-caller";
const CALLER_KEY_HASH = createHash("sha256")
  .update(CALLER_PLAINTEXT)
  .digest("hex");

const TTFT_HISTOGRAM_COUNT = "aisix_request_ttft_seconds_count";
const TTFT_HISTOGRAM_SUM = "aisix_request_ttft_seconds_sum";
const TTFT_LEGACY_COUNT = "aisix_llm_time_to_first_token_seconds_count";
const E2E_HISTOGRAM_COUNT = "aisix_request_e2e_latency_seconds_count";

/** 上游发出首帧前的等待：正确的 TTFT 一定不小于它。 */
const LEAD_MS = 250;
/**
 * 帧间隔。第二帧最早在 LEAD_MS + GAP_MS 到达，所以“首帧”时刻和任何更晚的
 * 时刻（首个正文、流结束、扣留放行）都被这个上界干净地分开。
 */
const GAP_MS = 300;

/** 原生 Responses SSE：`response.created` 打头，正文，带用量的终止帧。 */
function responsesStream(): string[] {
  return [
    JSON.stringify({ type: "response.created", response: { id: "resp_ttft_metrics" } }),
    JSON.stringify({ type: "response.output_text.delta", delta: "answer " }),
    JSON.stringify({ type: "response.output_text.delta", delta: "here" }),
    JSON.stringify({
      type: "response.completed",
      response: {
        id: "resp_ttft_metrics",
        status: "completed",
        usage: { input_tokens: 6, output_tokens: 2 },
      },
    }),
    "[DONE]",
  ];
}

function chatChunk(delta: unknown, extra: Record<string, unknown> = {}): string {
  return JSON.stringify({
    id: "chatcmpl-ttft-metrics",
    object: "chat.completion.chunk",
    created: 1,
    model: "deepseek-chat",
    choices: [{ index: 0, delta, finish_reason: null }],
    ...extra,
  });
}

/** 桥接路径上游说的是 chat SSE。 */
function chatStream(): string[] {
  return [
    chatChunk({ role: "assistant", content: "" }),
    chatChunk({ content: "answer " }),
    chatChunk({ content: "here" }),
    JSON.stringify({
      id: "chatcmpl-ttft-metrics",
      object: "chat.completion.chunk",
      created: 1,
      model: "deepseek-chat",
      choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
      usage: { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 },
    }),
    "[DONE]",
  ];
}

function completionChunk(text: string, extra: Record<string, unknown> = {}): string {
  return JSON.stringify({
    id: "cmpl-ttft-metrics",
    object: "text_completion",
    created: 1,
    model: "gpt-3.5-turbo-instruct",
    choices: [{ index: 0, text, finish_reason: null, logprobs: null }],
    ...extra,
  });
}

/** 旧版 completions SSE。 */
function completionsStream(): string[] {
  return [
    completionChunk("answer "),
    completionChunk("here"),
    JSON.stringify({
      id: "cmpl-ttft-metrics",
      object: "text_completion",
      created: 1,
      model: "gpt-3.5-turbo-instruct",
      choices: [{ index: 0, text: "", finish_reason: "stop", logprobs: null }],
      usage: { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 },
    }),
    "[DONE]",
  ];
}

/**
 * 第二帧的正文：一帧就超过下面扣留缓冲的上限，缓冲在它到达时（LEAD_MS + GAP_MS）
 * 溢出、按 fail-open 放行。不含邮箱，打码检测器不会动它。
 */
const OVERFLOW_TEXT = "a".repeat(2_048);

/** 原生 Responses SSE，第二帧撑爆扣留缓冲。 */
function overflowingResponsesStream(): string[] {
  return [
    JSON.stringify({ type: "response.created", response: { id: "resp_ttft_metrics" } }),
    JSON.stringify({ type: "response.output_text.delta", delta: OVERFLOW_TEXT }),
    JSON.stringify({
      type: "response.completed",
      response: {
        id: "resp_ttft_metrics",
        status: "completed",
        usage: { input_tokens: 6, output_tokens: 2 },
      },
    }),
    "[DONE]",
  ];
}

/** 桥接路径的 chat SSE，第二帧撑爆扣留缓冲。 */
function overflowingChatStream(): string[] {
  return [
    chatChunk({ role: "assistant", content: "" }),
    chatChunk({ content: OVERFLOW_TEXT }),
    JSON.stringify({
      id: "chatcmpl-ttft-metrics",
      object: "chat.completion.chunk",
      created: 1,
      model: "deepseek-chat",
      choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
      usage: { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 },
    }),
    "[DONE]",
  ];
}

/** 旧版 completions SSE，第二帧撑爆扣留缓冲。 */
function overflowingCompletionsStream(): string[] {
  return [
    completionChunk("answer "),
    completionChunk(OVERFLOW_TEXT),
    JSON.stringify({
      id: "cmpl-ttft-metrics",
      object: "text_completion",
      created: 1,
      model: "gpt-3.5-turbo-instruct",
      choices: [{ index: 0, text: "", finish_reason: "stop", logprobs: null }],
      usage: { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 },
    }),
    "[DONE]",
  ];
}

type Provider = "openai" | "deepseek";

/**
 * 每个 describe 一个网关进程：etcd 种子、接用量事件的 OTLP 接收端、调用方 key、
 * 要收尾关闭的上游。
 */
function gatewayFixture(extraSeed?: (seed: SeedClient) => Promise<void>) {
  const fx = {
    etcdReachable: false,
    app: undefined as SpawnedApp | undefined,
    seed: undefined as SeedClient | undefined,
    otlp: undefined as MockOtlp | undefined,
    upstreams: [] as OpenAiUpstream[],
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    fx.etcdReachable = await etcd.ping();
    if (!fx.etcdReachable) return;
    fx.app = await spawnApp();
    fx.seed = new SeedClient(etcd, fx.app.etcdPrefix);
    fx.otlp = await startMockOtlp();
    await fx.seed.createObservabilityExporter({
      name: "ttft-metrics-otlp",
      enabled: true,
      kind: "otlp_http",
      endpoint: fx.otlp.url,
    });
    if (extraSeed) await extraSeed(fx.seed);
    await fx.seed.createApiKey({
      key_hash: CALLER_KEY_HASH,
      allowed_models: ["*"],
    });
  });

  afterAll(async () => {
    await fx.app?.exit();
    await Promise.all(fx.upstreams.map((u) => u.close()));
    await fx.otlp?.close();
  });

  return fx;
}

type Fixture = ReturnType<typeof gatewayFixture>;

/**
 * `openai` 让 /v1/responses 走原生透传；其它 OpenAI 兼容提供商（这里是
 * `deepseek`）让它走 chat-completions 桥接。/v1/completions 两种都直接转发。
 */
async function createModel(
  fx: Fixture,
  displayName: string,
  upstream: OpenAiUpstream,
  provider: Provider,
  modelName: string,
): Promise<void> {
  const seed = fx.seed!;
  const pk = await seed.createProviderKey({
    display_name: `${displayName}-pk`,
    secret: "sk-mock",
    api_base: `${upstream.baseUrl}/v1`,
  });
  await seed.createModel({
    display_name: displayName,
    provider,
    model_name: modelName,
    provider_key_id: pk.id,
  });
  // 在被测配置之后再种一把探针 key，它能鉴权就说明前面的配置也已生效。
  const canary = `sk-canary-${displayName}-${Date.now()}`;
  await seed.createApiKey({
    key_hash: createHash("sha256").update(canary).digest("hex"),
    allowed_models: ["*"],
  });
  await waitConfigPropagation(async () => {
    const res = await fetch(`${fx.app!.proxyUrl}/v1/models`, {
      headers: { authorization: `Bearer ${canary}` },
    });
    return res.status === 200;
  });
}

async function streamingUpstream(fx: Fixture, events: string[]): Promise<OpenAiUpstream> {
  const upstream = await startOpenAiUpstream({
    streamEvents: events,
    firstEventDelayMs: LEAD_MS,
    eventDelayMs: GAP_MS,
  });
  fx.upstreams.push(upstream);
  return upstream;
}

/**
 * `aisix_request_e2e_latency_seconds_count` 里匹配 `want` 的各条序列之和。
 * 名字里带数字，共享的 `scrapeMetrics` 认不出这一族，所以直接读原始文本。
 */
async function e2eLatencyCount(metricsUrl: string, want: Record<string, string>): Promise<number> {
  const res = await fetch(`${metricsUrl}/metrics`);
  if (!res.ok) throw new Error(`scrape ${metricsUrl}/metrics returned ${res.status}`);
  let total = 0;
  for (const line of (await res.text()).split("\n")) {
    if (!line.startsWith(`${E2E_HISTOGRAM_COUNT}{`)) continue;
    if (!Object.entries(want).every(([k, v]) => line.includes(`${k}="${v}"`))) continue;
    total += Number(line.slice(line.lastIndexOf(" ") + 1));
  }
  return total;
}

interface Observed {
  before: MetricSample[];
  after: MetricSample[];
  /** 这一个请求在 e2e 延迟直方图里留下的观测数。 */
  e2eObservations: number;
  /** 响应头 `x-aisix-request-id`，用来认领这次请求的用量事件。 */
  requestId: string;
  /** 从发出请求到客户端收到第一个响应体字节，调用方视角的等待。 */
  firstByteMs: number;
}

/**
 * 发一个请求、读完响应体，然后轮询 `GET /metrics`，直到这个请求的 e2e 延迟
 * 观测出现且 `until` 成立，或者超时。流式请求的指标在流收尾时落盘，和响应体
 * 读完之间没有先后保证，所以要轮询；超时后照样返回最后一次抓取，让断言自己
 * 报出缺了什么。
 */
async function sendAndScrape(
  fx: Fixture,
  path: string,
  body: Record<string, unknown>,
  e2eWant: Record<string, string>,
  until: (before: MetricSample[], after: MetricSample[]) => boolean,
): Promise<Observed> {
  const app = fx.app!;
  const before = await scrapeMetrics(app.metricsUrl);
  const e2eBefore = await e2eLatencyCount(app.metricsUrl, e2eWant);
  const sentAt = Date.now();
  const res = await fetch(`${app.proxyUrl}${path}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${CALLER_PLAINTEXT}`,
      "content-type": "application/json",
    },
    body: JSON.stringify(body),
  });
  let firstByteMs = Number.NaN;
  let text = "";
  const decoder = new TextDecoder();
  for await (const chunk of res.body ?? []) {
    if (Number.isNaN(firstByteMs) && chunk.length > 0) firstByteMs = Date.now() - sentAt;
    text += decoder.decode(chunk, { stream: true });
  }
  expect(res.status, text).toBe(200);
  const requestId = res.headers.get("x-aisix-request-id") ?? "";
  expect(requestId, "x-aisix-request-id").not.toBe("");

  const deadline = Date.now() + 10_000;
  for (;;) {
    const after = await scrapeMetrics(app.metricsUrl);
    const e2eObservations = (await e2eLatencyCount(app.metricsUrl, e2eWant)) - e2eBefore;
    if ((e2eObservations >= 1 && until(before, after)) || Date.now() >= deadline) {
      return { before, after, e2eObservations, requestId, firstByteMs };
    }
    await new Promise((r) => setTimeout(r, 100));
  }
}

/** 等这次请求的用量事件经 OTLP 导出过来；超时就报出来，而不是静默跳过。 */
async function usageSpan(fx: Fixture, requestId: string): Promise<Record<string, unknown>> {
  const deadline = Date.now() + 10_000;
  for (;;) {
    const hit = fx.otlp!.spans.find((s) => s.attributes["aisix.request_id"] === requestId);
    if (hit) return hit.attributes;
    if (Date.now() >= deadline) {
      throw new Error(
        `no usage span for request_id=${requestId} (parse failures: ${fx.otlp!.parseFailures.length})`,
      );
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

/**
 * 一次流式请求：两条 TTFT 序列各一次观测，数值是上游首帧的时刻，用量事件里
 * 报的也是同一个时刻。
 */
async function expectStreamTtft(
  fx: Fixture,
  path: "/v1/responses" | "/v1/completions",
  model: string,
  body: Record<string, unknown>,
  heldBack = false,
  releasedAtMs = LEAD_MS + 2 * GAP_MS,
): Promise<void> {
  const want = { endpoint: path, model };
  const histogramWant = { ...want, streaming: "true" };
  const { before, after, e2eObservations, requestId, firstByteMs } = await sendAndScrape(
    fx,
    path,
    { model, stream: true, ...body },
    histogramWant,
    (b, a) =>
      metricDelta(b, a, TTFT_HISTOGRAM_COUNT, histogramWant) >= 1 &&
      metricDelta(b, a, TTFT_LEGACY_COUNT, want) >= 1,
  );

  // 先确认请求本身被观测到了：e2e 直方图有一次流式观测。否则下面 TTFT 的 0
  // 说明不了任何事。
  expect(e2eObservations, "e2e latency observations (the request itself was observed)").toBe(1);
  if (heldBack) {
    // 场景本身成立：调用方直到上游几乎发完才收到第一个字节，确实走的是扣留路径。
    // 这样下面“TTFT 停在首帧”的上界才说明 TTFT 没有被放行时刻顶替。
    expect(firstByteMs, "caller's first byte waits for the held stream").toBeGreaterThanOrEqual(
      releasedAtMs,
    );
  }
  expect(
    metricDelta(before, after, TTFT_HISTOGRAM_COUNT, histogramWant),
    "TTFT histogram observations",
  ).toBe(1);
  expect(
    metricDelta(before, after, TTFT_LEGACY_COUNT, want),
    "legacy TTFT summary observations",
  ).toBe(1);

  // 观测到的是上游首帧：不早于上游开口前的等待，也早于第二帧最早到达的时刻。
  const ttftSeconds = metricDelta(before, after, TTFT_HISTOGRAM_SUM, histogramWant);
  expect(ttftSeconds, "TTFT is at least the upstream's lead time").toBeGreaterThanOrEqual(
    (LEAD_MS - 20) / 1000,
  );
  expect(ttftSeconds, "TTFT stops on the first frame, not later").toBeLessThan(
    (LEAD_MS + GAP_MS) / 1000,
  );

  // 用量事件是同一次测量的另一个出口。
  const ttftMs = Number((await usageSpan(fx, requestId))["aisix.upstream_ttft_ms"]);
  expect(ttftMs, "usage event upstream_ttft_ms is at least the lead time").toBeGreaterThanOrEqual(
    LEAD_MS - 20,
  );
  expect(ttftMs, "usage event upstream_ttft_ms stops on the first frame").toBeLessThan(
    LEAD_MS + GAP_MS,
  );
}

describe("TTFT metrics on /v1/responses and /v1/completions streams", () => {
  const fx = gatewayFixture();

  test("native /v1/responses streaming lands in both TTFT series", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-resp-native";
    const upstream = await streamingUpstream(fx, responsesStream());
    await createModel(fx, model, upstream, "openai", "gpt-4o-mini");
    await expectStreamTtft(fx, "/v1/responses", model, { input: "hello" });
  });

  test("bridged /v1/responses streaming lands in both TTFT series", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-resp-bridge";
    const upstream = await streamingUpstream(fx, chatStream());
    await createModel(fx, model, upstream, "deepseek", "deepseek-chat");
    await expectStreamTtft(fx, "/v1/responses", model, { input: "hello" });
  });

  test("/v1/completions streaming lands in both TTFT series", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-cmpl";
    const upstream = await streamingUpstream(fx, completionsStream());
    await createModel(fx, model, upstream, "openai", "gpt-3.5-turbo-instruct");
    await expectStreamTtft(fx, "/v1/completions", model, { prompt: "hello" });
  });

  test("non-streaming /v1/responses and /v1/completions record no TTFT", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const upstream = await startOpenAiUpstream({
      pathBodies: {
        "/responses": {
          id: "resp_ttft_metrics_plain",
          object: "response",
          status: "completed",
          output: [
            {
              type: "message",
              role: "assistant",
              content: [{ type: "output_text", text: "plain answer" }],
            },
          ],
          usage: { input_tokens: 6, output_tokens: 2 },
        },
        "/completions": {
          id: "cmpl-ttft-metrics-plain",
          object: "text_completion",
          created: 1,
          model: "gpt-3.5-turbo-instruct",
          choices: [{ index: 0, text: "plain answer", finish_reason: "stop", logprobs: null }],
          usage: { prompt_tokens: 6, completion_tokens: 2, total_tokens: 8 },
        },
      },
    });
    fx.upstreams.push(upstream);
    const model = "ttft-m-plain";
    await createModel(fx, model, upstream, "openai", "gpt-4o-mini");

    for (const [path, body] of [
      ["/v1/responses", { input: "hello" }],
      ["/v1/completions", { prompt: "hello" }],
    ] as const) {
      const want = { endpoint: path, model };
      const { before, after, e2eObservations } = await sendAndScrape(
        fx,
        path,
        { model, ...body },
        { ...want, streaming: "false" },
        () => true,
      );
      expect(
        e2eObservations,
        `${path}: e2e latency observations (the request itself was observed)`,
      ).toBe(1);
      expect(
        metricDelta(before, after, TTFT_HISTOGRAM_COUNT, want),
        `${path}: TTFT histogram observations`,
      ).toBe(0);
      expect(
        metricDelta(before, after, TTFT_LEGACY_COUNT, want),
        `${path}: legacy TTFT summary observations`,
      ).toBe(0);
    }
  });
});

// 输出护栏是环境级的，种下去会把同一网关上所有请求都推上扣留路径，所以单开
// 一个网关。打码类 PII 检测器让流式输出策略变成整段扣留：扫描通过前客户端
// 什么也收不到。
describe("TTFT metrics on /v1/responses and /v1/completions streams under a hold-back output guardrail", () => {
  const fx = gatewayFixture(async (seed) => {
    await seed.createGuardrail({
      name: "ttft-metrics-holdback-mask",
      enabled: true,
      hook_point: "output",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
    });
  });

  test("held native /v1/responses stream lands in both TTFT series", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-hb-resp-native";
    const upstream = await streamingUpstream(fx, responsesStream());
    await createModel(fx, model, upstream, "openai", "gpt-4o-mini");
    await expectStreamTtft(fx, "/v1/responses", model, { input: "hello" }, true);
  });

  test("held bridged /v1/responses stream lands in both TTFT series", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-hb-resp-bridge";
    const upstream = await streamingUpstream(fx, chatStream());
    await createModel(fx, model, upstream, "deepseek", "deepseek-chat");
    await expectStreamTtft(fx, "/v1/responses", model, { input: "hello" }, true);
  });

  test("held /v1/completions stream lands in both TTFT series", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-hb-cmpl";
    const upstream = await streamingUpstream(fx, completionsStream());
    await createModel(fx, model, upstream, "openai", "gpt-3.5-turbo-instruct");
    await expectStreamTtft(fx, "/v1/completions", model, { prompt: "hello" }, true);
  });
});

// 扣留缓冲溢出、按 fail-open 放行：缓冲里已有的前缀和后面的实时流一起转发。
// 首帧在 LEAD_MS 到达、被缓冲；放行发生在第二帧把缓冲撑爆的时候
// （LEAD_MS + GAP_MS 之后）。TTFT 报的必须是首帧到达的时刻 —— 放行后重新
// 解析前缀时“看到”首帧的时刻是放行时刻，不是上游开口的时刻。
describe("TTFT metrics on /v1/responses and /v1/completions streams released by a fail-open buffer overflow", () => {
  const fx = gatewayFixture(async (seed) => {
    await seed.createGuardrail({
      name: "ttft-metrics-overflow-fail-open",
      enabled: true,
      hook_point: "output",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
      max_buffer_bytes: 1_024,
      on_buffer_exceeded: "fail_open",
    });
  });

  test("released native /v1/responses stream reports the first frame's arrival", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-of-resp-native";
    const upstream = await streamingUpstream(fx, overflowingResponsesStream());
    await createModel(fx, model, upstream, "openai", "gpt-4o-mini");
    await expectStreamTtft(
      fx,
      "/v1/responses",
      model,
      { input: "hello" },
      true,
      LEAD_MS + GAP_MS,
    );
  });

  test("released bridged /v1/responses stream reports the first frame's arrival", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-of-resp-bridge";
    const upstream = await streamingUpstream(fx, overflowingChatStream());
    await createModel(fx, model, upstream, "deepseek", "deepseek-chat");
    await expectStreamTtft(
      fx,
      "/v1/responses",
      model,
      { input: "hello" },
      true,
      LEAD_MS + GAP_MS,
    );
  });

  test("released /v1/completions stream reports the first frame's arrival", async (ctx) => {
    if (!fx.etcdReachable || !fx.app) {
      ctx.skip();
      return;
    }
    const model = "ttft-m-of-cmpl";
    const upstream = await streamingUpstream(fx, overflowingCompletionsStream());
    await createModel(fx, model, upstream, "openai", "gpt-3.5-turbo-instruct");
    await expectStreamTtft(
      fx,
      "/v1/completions",
      model,
      { prompt: "hello" },
      true,
      LEAD_MS + GAP_MS,
    );
  });
});
