import assert from "node:assert/strict";
import { test } from "node:test";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

const require = createRequire(import.meta.url);
const source = new URL("../Usage.tsx", import.meta.url);
const compiled = await build({
  stdin: {
    loader: "tsx", sourcefile: fileURLToPath(source), resolveDir: fileURLToPath(new URL(".", source)),
    contents: `${await readFile(source, "utf8")}\nexport { CallLogRow as RowUnderTest, CallLogDetails as DetailsUnderTest };`,
  },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { RowUnderTest, DetailsUnderTest } = loaded.exports as { RowUnderTest: ComponentType<Record<string, unknown>>; DetailsUnderTest: ComponentType<Record<string, unknown>> };

const base = {
  event_id: "call-1", occurred_at: "2026-11-16T02:00:00Z", request_id: "request-1",
  model: { id: "model-1", name: "a-model" }, api_key: { id: "key-1", label: "a-key", prefix: "sk-" }, member: null,
  status: "succeeded", error_class: null, cache_hit: false,
  first_token_latency_ms: 0, attempt_duration_ms: 2531, throughput_tps: 84.125, latency_ms: 999,
  usage: { prompt_tokens: 1000, cached_prompt_tokens: 80, cache_read_tokens: 130, cache_creation_tokens: 7, completion_tokens: 11, reasoning_tokens: 4 },
  usage_input_tokens: 1137, usage_estimated: false,
  charge: { status: "charged", estimated: false, group: { id: "group-1", display_name: "标准分组" }, discount_bps: 0,
    lines: [{ dimension: "input", quantity: 1000, rate_cny: "1" }],
    calculation: { original_micro_cny: "1000", adjusted_micro_cny: "0", final_micro_cny: "1000", rounding: "ceil_micro_cny" } },
};
function row(extra: Record<string, unknown> = {}): string {
  return renderToStaticMarkup(createElement("table", null, createElement("tbody", null,
    createElement(RowUnderTest, { call: { ...base, ...extra }, onDetails: () => {} }))));
}
const text = (markup: string) => markup.replace(/<[^>]+>/g, "");
function tokenCell(markup: string): string {
  const cell = markup.match(/<td[^>]*data-label="Token 消耗"[^>]*>(.*?)<\/td>/s);
  assert.ok(cell, "调用列表缺少真实 Token 消耗列");
  return text(cell[1]!);
}
function performanceValues(markup: string): string[] {
  const cell = markup.match(/<td[^>]*data-label="响应性能"[^>]*>(.*?)<\/td>/s);
  assert.ok(cell, "调用列表缺少响应性能列");
  return [...cell[1]!.matchAll(/<dd>(.*?)<\/dd>/g)].map(match => match[1]!);
}

// 时间必须是首格，详情入口不能再占用时间列之前的位置。
test("调用列表把独立时间放在首列", () => {
  const first = row().match(/<tbody><tr>(<td.*?<\/td>)/s)?.[1];
  assert.ok(first, "调用列表必须渲染记录行");
  assert.match(first, /data-label="时间"/);
  assert.match(first, /<time dateTime="2026-11-16T02:00:00Z">/);
  assert.match(first, /2026-11-16.*10:00:00/s);
});

// 缺失和非法标记不能因延迟为零或存在首字被推断成非流、流。
test("调用列表只按明确布尔标记显示流和非流，旧记录保持未知", () => {
  const typeCell = (extra: Record<string, unknown>) => {
    const cell = row(extra).match(/<td[^>]*data-label="请求类型"[^>]*>(.*?)<\/td>/s);
    assert.ok(cell, "调用列表必须有请求类型列");
    return text(cell[1]!);
  };
  assert.equal(typeCell({ stream: true }), "流");
  assert.equal(typeCell({ stream: false }), "非流");
  for (const stream of [undefined, null, "false", 0]) {
    assert.equal(typeCell({ stream, first_token_latency_ms: 0 }), "未知");
    assert.equal(typeCell({ stream, first_token_latency_ms: 1200 }), "未知");
  }
});

// 丢失实际用量或把缓存、推理重复相加，都会使这两个独立计数改变。
test("调用列表显示记录输入输出 Token，不能由估算或缓存子项代替", () => {
  const cell = tokenCell(row());
  assert.match(cell, /输入\s*1,137\s*Token/);
  assert.match(cell, /输出\s*11\s*Token/);
  assert.doesNotMatch(cell, /1,000\s*Token|15\s*Token/);
});

// 缺失方向默认为零会隐藏历史数据缺口，真实零则必须保留。
test("调用列表区分缺失用量和真实零", () => {
  assert.match(tokenCell(row({ usage_input_tokens: null, usage: null })), /输入\s*—.*输出\s*—/);
  assert.match(tokenCell(row({ usage_input_tokens: 0, usage: { completion_tokens: 0 } })), /输入\s*0\s*Token.*输出\s*0\s*Token/);
});

// 行组件丢失已有性能字段，或用旧延迟和 Token 重新估算，会改变此输出。
test("调用列表连接已有首字、总用时和有效生成 TPS", () => {
  assert.deepEqual(performanceValues(row()), ["0 ms", "2.53 s", "84.13"]);
  assert.deepEqual(performanceValues(row({ first_token_latency_ms: null, attempt_duration_ms: null, throughput_tps: null })), ["—", "—", "—"]);
});

// 详情触发器必须声明打开对话框，不能再声明可展开的表格行。
test("明细按钮打开有名称的详情对话框", () => {
  const markup = row();
  assert.match(markup, /aria-haspopup="dialog"/);
  assert.match(markup, /aria-label="查看调用详情"/);
  assert.doesNotMatch(markup, /aria-expanded=/);
});

// 抽屉不能丢掉原费用复算，也不能对历史记录假造遥测。
test("调用详情保留费用复算和遥测缺失说明", () => {
  const markup = renderToStaticMarkup(createElement(DetailsUnderTest, { call: base }));
  const content = text(markup);
  for (const label of ["请求信息", "响应性能", "Token 消耗", "费用计算", "计费明细", "最终费用", "按本次最终计价明细复算", "旧记录未采集"]) assert.ok(content.includes(label), label);
  assert.match(content, /1,137/);
  assert.match(content, /¥0\.001/);
});

// 列表或详情漏传来源都会把参考速度冒充实测；两处必须保持一致。
test("含估算用量的列表与详情保留参考 TPS 和独立首字", () => {
  const call = { ...base, usage_estimated: true };
  assert.deepEqual(performanceValues(row({ usage_estimated: true })), ["0 ms", "2.53 s", "≈ 84.13"]);
  const markup = renderToStaticMarkup(createElement(DetailsUnderTest, { call }));
  const section = markup.match(/<section[^>]*aria-label="响应性能"[^>]*>(.*?)<\/section>/s);
  assert.ok(section, "详情响应性能必须存在");
  assert.deepEqual([...section[1]!.matchAll(/<dd>(.*?)<\/dd>/g)].map(match => match[1]!), ["0 ms", "2.53 s", "≈ 84.13"]);
  assert.match(text(markup), /输出\s*11\s*Token/);
});

// 非流式不再整体排除；缺失 TPS 仍由 API 决定，客户端不能补算。
test("详情显示输出 TPS 完整上游口径，不沿用首帧后的限制", () => {
  const content = text(renderToStaticMarkup(createElement(DetailsUnderTest, { call: base })));
  assert.ok(content.includes("输出 TPS"));
  assert.ok(content.includes("完整上游耗时"));
  assert.doesNotMatch(content, /非流式或旧记录|TPS 不使用总用时估算/);
});

function upstreamTimingValue(markup: string, name: string): string {
  const row = markup.match(new RegExp(`<div[^>]*data-upstream-timing-field="${name}"[^>]*>(.*?)</div>`, "s"));
  assert.ok(row, `缺少 ${name} 阶段字段`);
  const dd = row[1]!.match(/<dd[^>]*>(.*?)<\/dd>/s);
  assert.ok(dd, `缺少 ${name} 阶段值`);
  return dd[1]!.replace(/<[^>]+>/g, "");
}

// 新阶段必须接到同一调用详情，不能从已有总耗时或首字补出历史值。
test("调用详情展示同一上游尝试的 HTTP 阶段偏移，缺失保持未知", () => {
  const renderDetails = (extra: Record<string, unknown>) => renderToStaticMarkup(createElement(DetailsUnderTest, { call: { ...base, ...extra } }));
  const recorded = renderDetails({ upstream_timing: { http_send_started_ms: 0, response_headers_ms: 136004, upstream_first_content_ms: 136006, first_content_ms: 136008 } });
  assert.match(recorded, /aria-label="上游 HTTP 阶段"/);
  assert.equal(upstreamTimingValue(recorded, "http_send_started_ms"), "0 ms");
  assert.equal(upstreamTimingValue(recorded, "response_headers_ms"), "136,004 ms");
  assert.equal(upstreamTimingValue(recorded, "upstream_first_content_ms"), "136,006 ms");
  assert.equal(upstreamTimingValue(recorded, "first_content_ms"), "136,008 ms");
  const missing = renderDetails({ first_token_latency_ms: 1000, attempt_duration_ms: 3000 });
  for (const key of ["http_send_started_ms", "response_headers_ms", "first_response_byte_ms", "first_protocol_frame_ms", "first_canonical_chunk_ms", "upstream_first_content_ms", "first_content_ms"]) {
    assert.equal(upstreamTimingValue(missing, key), "—");
  }
});

// 删除本行的值仍保留下一行相同值，跨行查找会错误通过此负控。
test("上游阶段断言不能借用下一行的相同值", () => {
  const markup = renderToStaticMarkup(createElement(DetailsUnderTest, { call: { ...base, upstream_timing: { http_send_started_ms: 0, response_headers_ms: 0 } } }));
  const row = markup.match(/<div[^>]*data-upstream-timing-field="http_send_started_ms"[^>]*>.*?<\/div>/s);
  assert.ok(row, "负控必须找到实际的 HTTP 客户端阶段行");
  const withoutValue = markup.replace(row[0], row[0].replace(/<dd[^>]*>.*?<\/dd>/s, ""));
  assert.notEqual(withoutValue, markup, "负控必须实际移除本行的值");
  assert.equal(upstreamTimingValue(withoutValue, "response_headers_ms"), "0 ms");
  assert.throws(() => upstreamTimingValue(withoutValue, "http_send_started_ms"), /缺少 http_send_started_ms 阶段值/);
  const withoutRow = markup.replace(row[0], "");
  assert.throws(() => upstreamTimingValue(withoutRow, "http_send_started_ms"), /缺少 http_send_started_ms 阶段/);
});

// 隐藏故障转移或把同目标重试算入次数、回退到代表行费用都会改变此输出。
test("合并请求显示服务端故障转移次数与全部尝试最终费用", () => {
  const request_group = { attempt_count: 4, failover_count: 1, retry_count: 2, charge_micro_cny: "9007199254740993", adjusted_micro_cny: "11", complete: true };
  const markup = row({ request_group });
  assert.match(text(markup), /故障转移 1 次/);
  assert.match(text(markup), /同目标重试 2 次/);
  const amount = markup.match(/<td[^>]*data-label="费用"[^>]*>(.*?)<\/td>/s);
  assert.ok(amount);
  assert.equal(text(amount[1]!), "¥9,007,199,254.740993（含调整）");
  assert.match(text(row({ request_group: { ...request_group, charge_micro_cny: null } })), /金额未知/);
});

// 删除任一次记录或将第一条错误覆盖为最终成功文案，链上的独立断言会失败。
test("请求详情保留完整尝试链及每次安全错误和费用", () => {
  const attempts = [
    { ...base, event_id: "attempt-0", attempt_index: 0, attempt_kind: "initial", is_final_attempt: false, status: "retried", error_class: "upstream_unavailable", error_message: "上游暂不可用" },
    { ...base, event_id: "attempt-1", attempt_index: 1, attempt_kind: "retry", is_final_attempt: false, status: "retried", error_class: "rate_limited", error_message: "请求过于频繁" },
    { ...base, event_id: "attempt-2", attempt_index: 2, attempt_kind: "fallback", is_final_attempt: true, status: "succeeded", error_message: null },
  ];
  const markup = renderToStaticMarkup(createElement(DetailsUnderTest, { call: { ...base, attempts,
    request_group: { attempt_count: 3, failover_count: 1, retry_count: 1, charge_micro_cny: "3000", adjusted_micro_cny: "0", complete: true } } }));
  assert.match(markup, /aria-label="故障转移与重试链"/);
  for (const attempt of attempts) {
    assert.match(markup, new RegExp(`data-attempt-event="${attempt.event_id}"`));
    assert.ok(text(markup).includes(attempt.event_id));
  }
  assert.match(text(markup), /已加载 3 \/ 3 次尝试/);
  assert.match(text(markup), /上游暂不可用/);
  assert.match(text(markup), /请求过于频繁/);
  assert.match(text(markup), /故障转移/);
  assert.match(text(markup), /同目标重试/);
});

// 后到达的早期错误不能排在最终成功之后，导致链的展示顺序反转。
test("请求详情按尝试编号展示迟到日志", () => {
  const attempts = [2, 0, 1].map(index => ({ ...base, event_id: `attempt-${index}`, attempt_index: index, attempt_kind: index === 2 ? "fallback" : "retry" }));
  const markup = renderToStaticMarkup(createElement(DetailsUnderTest, { call: { ...base, attempts,
    request_group: { attempt_count: 3, failover_count: 1, retry_count: 1, charge_micro_cny: "3000", adjusted_micro_cny: "0", complete: true } } }));
  assert.deepEqual([...markup.matchAll(/data-attempt-event="([^"]+)"/g)].map(match => match[1]), ["attempt-0", "attempt-1", "attempt-2"]);
});

test("响应性能分开标出下游首协议输出、请求首内容及单次尝试时间", () => {
  const call = { ...base, latency_ms: 8474, first_token_latency_ms: 307059, attempt_duration_ms: 306471 };
  const render = (latency_ms: number | null) => {
    const html = renderToStaticMarkup(createElement(DetailsUnderTest, { call: { ...call, latency_ms } }));
    return text(html.match(/<section[^>]*aria-label="响应性能"[^>]*>(.*?)<\/section>/s)![1]!);
  };
  assert.match(render(8474), /下游响应阶段耗时\s*8\.47 s/);
  assert.match(render(null), /下游响应阶段耗时\s*—/);
  assert.match(render(8474), /请求首内容307\.06 s/);
  assert.match(render(8474), /本次上游耗时306\.47 s/);
});
