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
  stdin: { contents: 'export { ApiMetricsPanel } from "./components/ApiMetricsPanel.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "api-metrics-panel.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-api-metrics-panel-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { ApiMetricsPanel } = require(entry) as { ApiMetricsPanel: ComponentType<ApiMetricsPanelProps> };
const props: ApiMetricsPanelProps = { scope: "tenant", enabled: true, from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", numberStart: 5 };

test("加载保留完整请求指标标签、期间说明和计数口径，不伪造零值", () => {
  const markup = renderToStaticMarkup(createElement(ApiMetricsPanel, props));
  const text = markup.replace(/<[^>]+>/g, "");
  for (const label of ["最终请求", "成功率", "失败率（已知结果）", "统计周期平均请求速率", "结果已知的请求", "成功请求", "失败请求", "结果未知的请求", "计费尝试", "最终状态未知的尝试"]) assert.ok(text.includes(label), `加载时仍有${label}位置`);
  assert.match(text, /按 UTC 小时汇总/);
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
