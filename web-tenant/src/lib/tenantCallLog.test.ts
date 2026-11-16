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

// 新阶段必须接到同一调用详情，不能从已有总耗时或首字补出历史值。
test("调用详情展示同一上游尝试的 HTTP 阶段偏移，缺失保持未知", () => {
  const renderDetails = (extra: Record<string, unknown>) => renderToStaticMarkup(createElement(DetailsUnderTest, { call: { ...base, ...extra } }));
  const recorded = renderDetails({ upstream_timing: { http_send_started_ms: 0, response_headers_ms: 136004, first_content_ms: 136008 } });
  assert.match(recorded, /aria-label="上游 HTTP 阶段"/);
  assert.match(recorded, /data-upstream-timing-field="http_send_started_ms"[^>]*>.*?<dd[^>]*>0 ms<\/dd>/s);
  assert.match(recorded, /data-upstream-timing-field="response_headers_ms"[^>]*>.*?<dd[^>]*>136,004 ms<\/dd>/s);
  const missing = renderDetails({ first_token_latency_ms: 1000, attempt_duration_ms: 3000 });
  for (const key of ["http_send_started_ms", "response_headers_ms", "first_response_byte_ms", "first_protocol_frame_ms", "first_canonical_chunk_ms", "first_content_ms"]) {
    assert.match(missing, new RegExp(`data-upstream-timing-field="${key}"[^>]*>.*?<dd[^>]*>—<\\/dd>`, "s"));
  }
});
