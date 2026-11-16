import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 真实 SSR 不执行 effect 或发送请求；改回旧标题/TPOT方法会使下面的语义断言失败。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { DirectModelMonitoring } from "./components/DirectModelMonitoring.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "direct-model-monitoring-ssr.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { DirectModelMonitoring } = loaded.exports as { DirectModelMonitoring: ComponentType };
const markup = renderToStaticMarkup(createElement(DirectModelMonitoring));
const text = markup.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();

// 恢复 24 小时或名称排序会分别使这两个默认展示断言失败。
test("上游监测默认展示过去一小时", () => {
  const times = [...markup.matchAll(/<input type="datetime-local"[^>]*value="([^"]+)"/g)].map(match => Date.parse(`${match[1]}+08:00`));
  assert.equal(times.length, 2);
  assert.ok(times.every(Number.isFinite));
  assert.equal(times[1]! - times[0]!, 60 * 60 * 1000);
});

test("上游监测默认按失败率从高到低排序", () => {
  const control = markup.match(/<button[^>]*aria-label="监测模型排序"[^>]*>[\s\S]*?<\/button>/)?.[0];
  assert.ok(control, "必须渲染模型排序控件");
  assert.match(control, /data-value="failure"/);
  assert.match(control, />失败率从高到低</);
});

test("监测概览和模型表统一标为输出 TPS", () => {
  assert.match(markup, /<h2[^>]*>输出 TPS<\/h2>/);
  assert.match(markup, /<th[^>]*>输出 TPS<\/th>/);
  assert.doesNotMatch(markup, /生成吞吐 · TPS/);
});

test("输出 TPS 说明采用完整上游耗时且区分汇总和分位口径", () => {
  assert.match(text, /输出 Token（含推理，不含输入）.*完整上游耗时/);
  assert.match(text, /毫秒.*秒/);
  assert.match(text, /有效输出 Token 合计.*有效完整上游耗时合计/);
  assert.match(text, /P50.*P95.*单次.*完整上游耗时/);
  assert.match(text, /不要求流式.*TTFT/);
  assert.match(text, /无有效采样显示 —/);
  assert.match(text, /含网关估算用量的记录不纳入实测输出 TPS/);
  assert.match(text, /调用记录保留 ≈ 参考速度/);
  assert.doesNotMatch(text, /1000\s*\/\s*TPOT|成功流式记录|输出 token 间隔数加权/);
});

test("首帧与客户端首 Token 仍是独立时钟，缺失保留未知", () => {
  assert.match(text, /上游首帧记录首个流式帧，可能只是元数据/);
  assert.match(text, /客户端首 token 记录首次交付生成内容/);
  assert.match(text, /两者不是同一时钟，不能直接相减；缺失记录保留为未知/);
});
