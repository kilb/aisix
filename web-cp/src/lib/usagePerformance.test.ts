import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 渲染真实展示组件；零、缺失与独立时钟的预期不依赖内部读取方式。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { UsagePerformance } from "./components/UsagePerformance.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "usage-performance.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { UsagePerformance } = loaded.exports as { UsagePerformance: ComponentType<{ data: Record<string, unknown> }> };
const values = (data: Record<string, unknown>) => [...renderToStaticMarkup(createElement(UsagePerformance, { data })).matchAll(/<dd>(.*?)<\/dd>/g)].map(match => match[1]);

test("实测零首字保留，完整耗时和独立TPS分别展示", () => {
  assert.deepEqual(values({ first_token_latency_ms: 0, attempt_duration_ms: 2531, throughput_tps: 84.125, latency_ms: 999 }), ["0 ms", "2.53 s", "84.13"]);
});
test("三项未知不能借响应阶段耗时或Token数量凑出指标", () => {
  assert.deepEqual(values({ latency_ms: 990, completion_tokens: 100, first_token_latency_ms: null, attempt_duration_ms: null, throughput_tps: null }), ["—", "—", "—"]);
  assert.deepEqual(values({}), ["—", "—", "—"]);
});
test("不可比较的非正或非法吞吐量不显示为实测速度", () => {
  for (const throughput_tps of [0, -1, Infinity, NaN]) assert.equal(values({ throughput_tps })[2], "—");
});

// 删掉来源标记或连首字一起隐藏，会使这组结果改变。
test("含估算用量保留参考 TPS，首字独立且未知不补数", () => {
  assert.deepEqual(values({ first_token_latency_ms: 0, attempt_duration_ms: 2531, throughput_tps: 84.125, usage_estimated: true }), ["0 ms", "2.53 s", "≈ 84.13"]);
  for (const usage_estimated of [false, null, undefined]) assert.equal(values({ throughput_tps: 84.125, usage_estimated })[2], "84.13");
  assert.deepEqual(values({ first_token_latency_ms: 0, throughput_tps: null, usage_estimated: true }), ["0 ms", "—", "—"]);
});
