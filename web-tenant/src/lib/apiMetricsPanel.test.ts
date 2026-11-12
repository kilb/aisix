import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { decodeApiMetrics, type ApiMetricsResponse } from "./apiMetrics.ts";
import type { ApiMetricsPanelProps } from "../components/ApiMetricsPanel";

// 渲染真实产品组件；SSR 不执行网络 effect，不伪造成功响应来测试加载状态。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { ApiMetricsPanel } from "./components/ApiMetricsPanel.tsx"; export { ApiSuccessSummary } from "./components/ApiSuccessSummary.tsx"; export { ApiMetricsOverview } from "./components/ApiMetricsOverview.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "api-metrics-panel.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-api-metrics-panel-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { ApiMetricsPanel, ApiSuccessSummary, ApiMetricsOverview } = require(entry) as { ApiMetricsOverview: ComponentType<{ scope: "tenant" | "admin"; from: string; to: string; data: ApiMetricsResponse | null; error: string | null }>; ApiMetricsPanel: ComponentType<ApiMetricsPanelProps>; ApiSuccessSummary: ComponentType<{ totals?: { requests: number | null; succeeded_requests: number | null; failed_requests: number | null; unknown_outcome_requests: number | null; known_outcome_requests: number | null; unknown_final_attempts: number | null; success_rate: number | null } }> };
const props: ApiMetricsPanelProps = { scope: "tenant", enabled: true, from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", numberStart: 5 };

// 四项摘要必须常显；恢复旧按钮或缺少 P50/样本位置都会使本检查失败。
test("统计概览固定四项 KPI、请求和延迟趋势，不用五按钮替换整块内容", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, compact: true }));
  const text = markup.replace(/<[^>]+>/g, "");
  for (const label of ["请求数", "成功率", "首 token 延迟 P50", "实测样本", "已计价费用", "请求趋势", "首 token 延迟概况", "查看用量分析"]) assert.ok(text.includes(label), label);
  assert.equal((markup.match(/data-metric-kpi=/g) ?? []).length, 4);
  assert.doesNotMatch(markup, /aria-expanded|aria-pressed|role="tablist"/);
  assert.doesNotMatch(text, /覆盖率0%|P500 ms|¥0\.00|收起指标明细|查看 token/);
});

test("概览分析链接保留时间和身份范围", () => {
  for (const [scope, hash] of [["tenant", "usage"], ["admin", "analytics"]] as const) {
    const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, scope, compact: true }));
    const href = markup.match(/href="(#[^"]+)"/)?.[1]?.replaceAll("&amp;", "&");
    assert.ok(href);
    const [page, query] = href.slice(1).split("?");
    assert.equal(page, hash);
    const params = new URLSearchParams(query);
    assert.equal(params.get("from"), props.from);
    assert.equal(params.get("to"), props.to);
  }
});

test("完整分析使用唯一 tablist 与稳定 tabpanel，受控视图改变可见内容", () => {
  for (const view of ["requests", "latency", "tokens", "cache", "cost"] as const) {
    const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, view }));
    assert.equal((markup.match(/role="tablist"/g) ?? []).length, 1);
    assert.equal((markup.match(/role="tab"/g) ?? []).length, 5);
    assert.equal((markup.match(/role="tabpanel"/g) ?? []).length, 1);
    assert.equal((markup.match(/aria-selected="true"/g) ?? []).length, 1);
    assert.match(markup, new RegExp(`role="tab"[^>]*data-view="${view}"[^>]*aria-selected="true"`));
    const activeTab = markup.match(/id="([^"]+)" role="tab"[^>]*aria-selected="true"[^>]*aria-controls="([^"]+)"/);
    assert.ok(activeTab);
    assert.ok(markup.includes(`id="${activeTab[2]}" role="tabpanel" aria-labelledby="${activeTab[1]}"`));
    assert.doesNotMatch(markup, /aria-expanded|aria-pressed/);
  }
});

// 成功率是 2/3，构成条却必须按全部 4 个请求画成 50/25/25。
test("最终结果构成直接包含未知计数，不能沿用已知结果成功率作宽度", () => {
  const markup = renderToStaticMarkup(createElement(ApiSuccessSummary, { totals: { requests: 4, succeeded_requests: 2, failed_requests: 1, unknown_outcome_requests: 1, known_outcome_requests: 3, unknown_final_attempts: 0, success_rate: 2 / 3 } }));
  assert.match(markup, /aria-label="最终请求构成：成功 2，失败 1，未知 1，共 4"/);
  assert.match(markup, /style="width:50%"/);
  assert.equal((markup.match(/style="width:25%"/g) ?? []).length, 2);
  const loading = renderToStaticMarkup(createElement(ApiSuccessSummary, {}));
  assert.doesNotMatch(loading, /style="width:0%"/);
});

test("加载保留完整请求指标标签、期间说明和计数口径，不伪造零值", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, props));
  const text = markup.replace(/<[^>]+>/g, "");
  for (const label of ["最终请求", "成功率", "失败率（已知结果）", "统计周期平均请求速率", "结果已知的请求", "成功请求", "失败请求", "结果未知的请求", "计费尝试", "最终状态未知的尝试"]) assert.ok(text.includes(label), `加载时仍有${label}位置`);
  assert.match(text, /按小时汇总/);
  assert.equal((text.match(/UTC\+8/g) ?? []).length, 1);
  assert.match(text, /请求速率是整个查询周期的平均值，不是瞬时 RPS/);
  assert.match(text, /请求数按最终记录统计/);
  assert.doesNotMatch(text, /0%|0 次\/秒|¥0\.00/);
  assert.match(markup, /role="region" aria-label="完整 API 指标"/);
});

test("范围不合法也保留指标框架和原因，不能变成较短的错误卡", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, to: "2027-11-01T00:00:00Z" }));
  const text = markup.replace(/<[^>]+>/g, "");
  assert.match(text, /最多查询 92 天/);
  assert.match(text, /失败率（已知结果）/);
  assert.match(text, /最终状态未知的尝试/);
  assert.match(text, /请求数按最终记录统计/);
});

test("无访问范围时不渲染上一身份的指标面板", () => {
  assert.equal(renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, enabled: false })), "");
});

// 4个最终请求中只有3个结果已知；未知结果不能压低成功率或被算成失败。
test("首屏成功率摘要并列成功失败未知，并公开已知结果分母", () => {
  const markup = renderToStaticMarkup(createElement(ApiSuccessSummary, { totals: { requests: 4, succeeded_requests: 2, failed_requests: 1, unknown_outcome_requests: 1, known_outcome_requests: 3, unknown_final_attempts: 0, success_rate: 2 / 3 } }));
  const text = markup.replace(/<[^>]+>/g, "");
  assert.match(text, /成功率66\.7%/);
  assert.match(text, /成功2/);
  assert.match(text, /失败1/);
  assert.match(text, /未知1/);
  assert.match(text, /已知结果样本（分母）3/);
  assert.match(text, /最终请求4/);
  assert.doesNotMatch(text, /50%|75%/);
});

test("首屏摘要在加载和未知时保留全部指标位置，不用零伪装数据", () => {
  const markup = renderToStaticMarkup(createElement(ApiSuccessSummary, {}));
  const text = markup.replace(/<[^>]+>/g, "");
  for (const label of ["成功率", "成功", "失败", "未知", "已知结果样本（分母）", "最终请求"]) assert.ok(text.includes(label));
  assert.doesNotMatch(text, /0%|[成功失败未知]0/);
  assert.match(text, /—/);
});

test("紧凑概览保留已知结果分母、未知计数和北京时间", () => {
  const text = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, compact: true })).replace(/<[^>]+>/g, "");
  for (const label of ["已知结果样本（分母）", "成功", "失败", "未知", "2026-11-01 08:00:00", "无实测值", "待计价尝试"]) assert.ok(text.includes(label), label);
  assert.equal((text.match(/UTC\+8/g) ?? []).length, 1);
});

test("固定摘要属于指标概览，不能冒充完整指标区域", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, compact: true }));
  assert.match(markup, /role="group" aria-label="API 指标概览"/);
  assert.doesNotMatch(markup, /role="region" aria-label="完整 API 指标"/);
});

test("完整指标区域唯一且不包含同名摘要指标", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, props));
  assert.equal(markup.match(/role="region" aria-label="完整 API 指标"/g)?.length, 1);
  assert.match(markup, /API 指标 · 请求/);
  assert.doesNotMatch(markup, /aria-label="请求成功率摘要"/);
});

test("零请求摘要显示真实零计数，但没有已知结果时不伪造零成功率", () => {
  const markup = renderToStaticMarkup(createElement(ApiSuccessSummary, { totals: { requests: 0, succeeded_requests: 0, failed_requests: 0, unknown_outcome_requests: 0, known_outcome_requests: 0, unknown_final_attempts: 0, success_rate: null } }));
  const text = markup.replace(/<[^>]+>/g, "");
  assert.match(text, /成功率—/);
  assert.match(text, /成功0/);
  assert.match(text, /失败0/);
  assert.match(text, /未知0/);
  assert.match(text, /已知结果样本（分母）0/);
  assert.doesNotMatch(text, /0%/);
});

// 金额经 number 会丢掉最后一微元，P99 代替 P50 也会直接使此例失败。
test("四项摘要投影精确金额、P50、真实样本和已知结果分母", () => {
  const data = decodeApiMetrics({ from: props.from, to: props.to, bucket: "hour", series: [], models: [], totals: {
    requests: 4, billing_attempts: 6, succeeded_requests: 2, failed_requests: 1,
    known_outcome_requests: 3, unknown_outcome_requests: 1, success_rate: 2 / 3,
    charged_micro_cny: "9007199254740993", unpriced_attempts: 2,
    first_token_latency_ms: { p50: 125, p99: 999, sample_count: 2, missing_count: 2 },
  } });
  const text = renderToStaticMarkup(createElement(ApiMetricsOverview, { scope: "tenant", from: props.from, to: props.to, data, error: null })).replace(/<[^>]+>/g, "");
  for (const expected of ["¥9,007,199,254.740993", "成功率66.7%", "成功 2 · 失败 1 · 未知 1", "已知结果样本（分母）3", "首 token 延迟 P50125 ms", "实测样本 2 · 无实测值 2", "待计价尝试 2"]) assert.ok(text.includes(expected), expected);
  assert.doesNotMatch(text, /999 ms|50%|75%/);
});

test("概览区分未知费用与真实零，缺样本不补零", () => {
  const data = decodeApiMetrics({ from: props.from, to: props.to, bucket: "hour", series: [], models: [], totals: {} });
  const render = (value: ApiMetricsResponse) => renderToStaticMarkup(createElement(ApiMetricsOverview, { scope: "tenant", from: props.from, to: props.to, data: value, error: null })).replace(/<[^>]+>/g, "");
  const unknown = render(data);
  assert.match(unknown, /已计价费用未知/);
  assert.match(unknown, /首 token 延迟 P50—/);
  assert.doesNotMatch(unknown, /¥0\.00|0 ms|0%/);
  assert.match(render({ ...data, totals: { ...data.totals, charged_micro_cny: 0n } }), /已计价费用¥0\.00/);
});

test("缓存分析明确列出 token 与响应缓存的独立分母", () => {
  const text = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, view: "cache" })).replace(/<[^>]+>/g, "");
  assert.match(text, /（输入内缓存子集 \+ 额外缓存读取）÷（原始输入 \+ 额外缓存读取 \+ 额外缓存创建）/);
  assert.match(text, /命中 ÷（命中 \+ 未命中）；绕过、未启用与未知不进入分母/);
  assert.doesNotMatch(text, /命中率0%/);
});
