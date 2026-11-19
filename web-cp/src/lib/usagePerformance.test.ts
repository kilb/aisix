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
const markup = (data: Record<string, unknown>) => renderToStaticMarkup(createElement(UsagePerformance, { data }));
const values = (data: Record<string, unknown>) => [...markup(data).matchAll(/<dd>(.*?)<\/dd>/g)].map(match => match[1]);

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

// 旧的首帧后速率说明或模糊标签必须失败；数值仍只来自 API。
test("输出 TPS 明示完整上游耗时口径，独立于输入和首帧时钟", () => {
  const data = { throughput_tps: 60.391978121, attempt_duration_ms: 4388, first_token_latency_ms: 5072, upstream_ttft_ms: 4136, time_per_output_token_ms: 0.95454545, prompt_tokens: 2, cache_read_tokens: 308467, completion_tokens: 265, reasoning_tokens: 120 };
  const rendered = markup(data);
  assert.match(rendered, /<dt>输出 TPS<\/dt>/);
  assert.match(rendered, /title="输出 Token（含推理，不含输入）÷ 完整上游耗时"/);
  assert.doesNotMatch(rendered, /首帧后的生成间隔|1000 \/ TPOT/);
  assert.equal(values(data)[2], "60.39");
  assert.equal(values({ ...data, throughput_tps: 7.125 })[2], "7.13");
  assert.equal(values({ ...data, throughput_tps: null })[2], "—");
});

test("请求与尝试的时钟分别命名，未完成的上游发送显示等到断开时的观测", () => {
  const timings = { first_token_latency_ms: 307_059, attempt_duration_ms: 306_471 };
  assert.match(markup(timings), /<dt>请求首内容<\/dt><dd>307\.06 s<\/dd>/);
  assert.match(markup(timings), /<dt>本次上游耗时<\/dt><dd>306\.47 s<\/dd>/);
  const cancelled = { error_class: "client_disconnected", upstream_timing: { http_send_started_ms: 0 }, attempt_duration_ms: 32_359 };
  assert.match(markup(cancelled), /<dt>尝试开始至断开<\/dt><dd>32\.36 s<\/dd>/);
  assert.equal(values({ ...cancelled, attempt_duration_ms: 0 })[1], "—");
  for (const upstream_timing of [undefined, {}, { http_send_started_ms: 0, response_headers_ms: 0 }, { http_send_started_ms: 0, first_protocol_frame_ms: 100 }]) {
    assert.match(markup({ ...cancelled, upstream_timing }), /<dt>本次上游耗时<\/dt>/);
  }
  assert.match(markup({ ...cancelled, error_class: "upstream_status" }), /<dt>本次上游耗时<\/dt>/);
});
