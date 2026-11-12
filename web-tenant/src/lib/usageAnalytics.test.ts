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
type CardProps = { rows: UsageSummaryRow[] | null; error: string | null; rangeLabel: string; number: number; className: string };
const cards = createRequire(import.meta.url)(entry) as Record<string, ComponentType<CardProps>>;

function render(name: string, rows: UsageSummaryRow[] | null, error: string | null = null): string {
  return renderToStaticMarkup(createElement(cards[name]!, { rows, error, rangeLabel: "所选 UTC 区间", number: 1, className: "" }));
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
