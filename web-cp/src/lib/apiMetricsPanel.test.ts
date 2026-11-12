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
import type { ApiMetricsPanelProps } from "../components/ApiMetricsPanel";

// 渲染真实产品组件；SSR 不执行网络 effect，不伪造成功响应来测试加载状态。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { ApiMetricsPanel } from "./components/ApiMetricsPanel.tsx"; export { ApiSuccessSummary } from "./components/ApiSuccessSummary.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "api-metrics-panel.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-api-metrics-panel-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { ApiMetricsPanel, ApiSuccessSummary } = require(entry) as { ApiMetricsPanel: ComponentType<ApiMetricsPanelProps>; ApiSuccessSummary: ComponentType<{ totals?: { requests: number | null; succeeded_requests: number | null; failed_requests: number | null; unknown_outcome_requests: number | null; known_outcome_requests: number | null; unknown_final_attempts: number | null; success_rate: number | null } }> };
const props: ApiMetricsPanelProps = { scope: "tenant", enabled: true, from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", numberStart: 5 };

// 新入口和诊断在加载时也可发现；未知不能被 0% 或假用量替代。
test("统计概览提供 token 入口、P99 与用量覆盖率位置", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, compact: true }));
  const text = markup.replace(/<[^>]+>/g, "");
  for (const label of ["token", "首 token 延迟 P99", "用量记录覆盖率", "查看 token"]) assert.ok(text.includes(label), label);
  assert.doesNotMatch(text, /覆盖率0%|P990 ms/);
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
  assert.match(text, /按北京时间小时汇总/);
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

// 紧凑模式必须直接看到成功率和诊断入口，初始不展开长趋势与完整明细。
test("紧凑首屏直接呈现请求健康与延迟缓存入口，不先铺长分析卡", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, { ...props, compact: true }));
  const text = markup.replace(/<[^>]+>/g, "");
  assert.match(text, /API 请求健康/);
  assert.match(markup, /role="group" aria-label="请求成功率摘要"/);
  assert.match(text, /已知结果样本（分母）/);
  assert.match(text, /2026-11-01 08:00:00 北京时间/);
  assert.match(text, /首 token 延迟/);
  assert.match(text, /查看延迟/);
  assert.match(text, /查看缓存/);
  assert.doesNotMatch(text, /按北京时间小时汇总/);
});

test("收起的摘要属于指标概览，不能冒充完整指标区域", () => {
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
