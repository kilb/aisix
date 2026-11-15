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
import type { UsageSummaryRow } from "./api";
import { summarizeUsage, type UsageChartSummary } from "./chartFormat.ts";

// 直接渲染产品卡片，校验合并期间与金额展示，不替换网络或聚合实现。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: {
    contents: 'export * from "./components/UsageAnalytics.tsx";',
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "usage-analytics-component.ts",
  },
  bundle: true,
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  write: false,
  plugins: [{
    name: "single-react-runtime",
    setup(builder) {
      builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true }));
    },
  }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-usage-analytics-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
type CardProps = { rows: UsageSummaryRow[] | null; summary: UsageChartSummary | null; error: string | null; rangeLabel: string; number: number; className: string };
const cards = createRequire(import.meta.url)(entry) as Record<string, ComponentType<CardProps>>;

function render(name: string, rows: UsageSummaryRow[] | null, error: string | null = null, summary = rows === null ? null : summarizeUsage(rows)): string {
  return renderToStaticMarkup(createElement(cards[name]!, { rows, summary, error, rangeLabel: "所选 UTC 区间", number: 1, className: "" }));
}

function text(markup: string): string {
  return markup.replace(/<[^>]+>/g, "");
}

function row(name: string, charged: number, requests = 1): UsageSummaryRow {
  return { period_start: "2026-10-01T00:00:00Z", model: { id: name, name }, requests, charged_micro_cny: charged, estimated_requests: 0, quantities: {} };
}

test("费用卡与排行保留超安全整数合计的每位精度", () => {
  const rows = [row("A", Number.MAX_SAFE_INTEGER), row("B", Number.MAX_SAFE_INTEGER - 1)];
  assert.match(text(render("UsageCostCard", rows)), /¥18,014,398,509\.481981/);
  const models = text(render("UsageModelsCard", rows));
  assert.match(models, /¥9,007,199,254\.740991 · 50\.0%/);
  assert.doesNotMatch(models, /481984/);
});

test("跨期other加入总计与其他分组，两个趋势都保留不可绘制说明", () => {
  const other = { ...row("other", 7000000, 7), model: { id: null, name: "other" } };
  const rows = [row("A", 3000000, 3), other];
  const requests = render("UsageRequestCard", rows);
  const costTrend = render("UsageCostTrendCard", rows);
  assert.match(text(requests), /10/);
  assert.match(text(requests), /部分期间明细已合并，无法完整绘制趋势/);
  assert.match(text(costTrend), /合并费用仍计入总额/);
  assert.doesNotMatch(requests + costTrend, /<svg/);
  const models = text(render("UsageModelsCard", rows));
  assert.match(models, /1可识别模型/);
  assert.match(models, /其他/);
  assert.match(models, /¥7\.00 · 70\.0%/);
});

test("完整期间数据绘制真实趋势，未知期间不会补成零", () => {
  const rows = [row("A", 3000000, 3), { ...row("A", 7000000, 7), period_start: "2026-10-03T00:00:00Z" }];
  const requests = render("UsageRequestCard", rows);
  const costTrend = render("UsageCostTrendCard", rows);
  assert.match(requests, /<svg[^>]+role="img"/);
  assert.match(costTrend, /<svg[^>]+role="img"/);
  assert.match(text(requests), /10/);
  assert.match(text(requests), /缺失期间未填零/);
  assert.doesNotMatch(requests, /2026-10-02/);
});

test("不安全费用显示未知，不显示假零或模型占比", () => {
  const rows = [row("A", Number.MAX_SAFE_INTEGER + 1)];
  assert.match(text(render("UsageCostCard", rows)), /未知/);
  const models = text(render("UsageModelsCard", rows));
  assert.match(models, /部分模型费用未知/);
  assert.doesNotMatch(models, /0\.0%|¥0\.00/);
});

test("未授权或未知模型分组保留费用，但不冒充可识别模型", () => {
  const unknown = { ...row("", 2000000), model: { id: null, name: "" } };
  const models = text(render("UsageModelsCard", [row("A", 1000000), unknown]));
  assert.match(models, /1可识别模型/);
  assert.match(models, /未授权或未知模型/);
  assert.match(models, /¥2\.00/);
});

test("负数调整可排费用，但不能伪装成零占比", () => {
  const models = text(render("UsageModelsCard", [row("A", 2000000), row("B", -1000000)]));
  assert.match(models, /-¥1\.00/);
  assert.match(models, /不计算占比/);
  assert.doesNotMatch(models, /0\.0%|100\.0%/);
});

test("加载、空态和错误仍保留对应卡标题与原因", () => {
  assert.match(text(render("UsageRequestCard", null)), /调用尝试次数/);
  assert.match(text(render("UsageCostCard", [])), /费用.*所选范围内没有用量/);
  const failed = text(render("UsageRequestCard", [], "统计暂不可用"));
  assert.match(failed, /调用尝试次数.*统计暂不可用/);
  assert.doesNotMatch(failed, /¥0\.00/);
});

// 恢复任一卡内的全量聚合会再次读取每条requests，使本检查失败。
test("概览三卡消费同一份精确摘要，不重复遍历原始用量", () => {
  let reads = 0;
  const rows = [row("Beta", 4000000, 4), row("Alpha", 2000000, 2), row("Beta", -1000000, 1)]
    .map((source) => ({ ...source, get requests() { reads += 1; return source.requests; } }));
  const summary = summarizeUsage(rows);
  const requests = render("UsageRequestCard", rows, null, summary);
  const cost = text(render("UsageCostCard", rows, null, summary));
  const models = text(render("UsageModelsCard", rows, null, summary));
  assert.match(requests, /合计7次/);
  assert.match(cost, /¥5\.00/);
  assert.match(models, /Beta.*¥3\.00 · 60\.0%.*Alpha.*¥2\.00 · 40\.0%/);
  assert.equal(reads, rows.length, "单次聚合后，三卡不得重新读取原始requests");
});

test("摘要卡在替换输入后显示新数量和费用，空与加载不显示旧摘要", () => {
  const previous = [row("Old", 9000000, 9)];
  const next = [row("New", 2000000, 2)];
  const summary = summarizeUsage(next);
  assert.match(render("UsageRequestCard", next, null, summary), /合计2次/);
  assert.match(text(render("UsageCostCard", next, null, summary)), /¥2\.00/);
  assert.doesNotMatch(text(render("UsageModelsCard", next, null, summary)), /Old|¥9\.00/);
  for (const name of ["UsageRequestCard", "UsageCostCard", "UsageModelsCard"]) {
    // 即使旧摘要仍由上层持有，rows的未完成/空结果也必须优先。
    const stale = summarizeUsage(previous);
    const loading = render(name, null, null, stale);
    const empty = text(render(name, [], null, stale));
    assert.doesNotMatch(loading, /<svg/);
    assert.match(empty, /所选范围内没有用量/);
    assert.doesNotMatch(text(loading) + empty, /Old|¥9\.00|合计9次/);
  }
});

test("请求趋势按期间先后显示，模型费用按数额排序而不是输入顺序", () => {
  const rows = [
    { ...row("Alpha", 1000000, 1), period_start: "2026-10-03T00:00:00Z" },
    { ...row("Beta", 3000000, 3), period_start: "2026-10-01T00:00:00Z" },
    { ...row("Alpha", 1000000, 1), period_start: "2026-10-02T00:00:00Z" },
  ];
  const requests = text(render("UsageRequestCard", rows));
  const models = text(render("UsageModelsCard", rows));
  const dates = ["2026-10-01", "2026-10-02", "2026-10-03"].map((date) => requests.indexOf(date));
  assert.ok(dates.every((index) => index >= 0));
  assert.ok(dates[0]! < dates[1]! && dates[1]! < dates[2]!);
  assert.match(models, /Beta.*¥3\.00 · 60\.0%.*Alpha.*¥2\.00 · 40\.0%/);
});
