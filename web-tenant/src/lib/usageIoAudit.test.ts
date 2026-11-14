import assert from "node:assert/strict";
import { test } from "node:test";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 渲染调用详情的真实组件，不拦截网络，不构造替代后端。
const require = createRequire(import.meta.url);
const source = new URL("../Usage.tsx", import.meta.url);
const compiled = await build({
  stdin: { loader: "tsx", contents: `${await readFile(source, "utf8")}\nexport { FeeCalculation as AuditCallDetail };`, resolveDir: fileURLToPath(new URL(".", source)), sourcefile: fileURLToPath(source) },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const component = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, component, component.exports);
const { AuditCallDetail } = component.exports as { AuditCallDetail: ComponentType<Record<string, unknown>> };

const audit = {
  version: 1, boundary: "client_exchange", request_body_bytes: 140, response_body_bytes: 95, response_complete: true,
  input_text: { utf8_bytes: 8, unicode_scalars: 3, complete: true },
  output_text: { utf8_bytes: 8, unicode_scalars: 3, complete: true },
};
const base = {
  event_id: "audit-call", status: "succeeded", charge_micro_cny: 0, adjusted_micro_cny: null,
  cny_per_usd_e4: null, discount_bps: null, lines: [], usage_estimated: false,
  usage: { prompt_tokens: 100, cached_prompt_tokens: 80, cache_read_tokens: 30, cache_creation_tokens: 7, completion_tokens: 11, reasoning_tokens: 4 },
  usage_input_tokens: 137,
  charge: { status: "nothing_to_bill", estimated: false, lines: [], discount_bps: null, calculation: { original_micro_cny: "0", adjusted_micro_cny: "0", final_micro_cny: "0" } },
};
function render(extra: Record<string, unknown> = {}): string {
  const call = { ...base, ...extra };
  return renderToStaticMarkup(createElement(AuditCallDetail, { call: call }));
}
function row(markup: string, field: string): string {
  const match = markup.match(new RegExp(`<div[^>]*data-io-field="${field}"[^>]*>(.*?)</div>`, "s"));
  assert.ok(match, `缺少 ${field} 测量行`);
  return match[1]!.replace(/<[^>]+>/g, "");
}

// 删除审计投影或把缺失默认成 0 会使此检查失败。
test("旧记录保留未知，不把未采集的长度和参考显示为零", () => {
  const markup = render();
  for (const field of ["request-body", "response-body", "input-text", "output-text", "input-reference", "output-reference"]) assert.match(row(markup, field), /未采集|不可用/);
});

// 把文本字节误作字符，或重复加缓存子集、扣除推理都会改变此输出。
test("详情显示独立的文本字节、Unicode 字符、参考和规范化记录用量", () => {
  const markup = render({ io_audit: audit, io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 2, output_tokens: 2 } });
  assert.match(row(markup, "request-body"), /140 字节.*完整/);
  assert.match(row(markup, "response-body"), /95 字节.*完整/);
  assert.match(row(markup, "input-text"), /8 UTF-8 字节.*3 Unicode 字符.*完整/);
  assert.match(row(markup, "output-text"), /8 UTF-8 字节.*3 Unicode 字符.*完整/);
  assert.match(row(markup, "input-reference"), /2.*完整/);
  assert.match(row(markup, "output-reference"), /2.*完整/);
  assert.equal(row(markup, "recorded-input"), "记录输入 Token137");
  assert.equal(row(markup, "recorded-output"), "记录输出 Token11");
});

// 用 truthiness 判断长度或参考会把合法零误作不可用。
test("已知空文本和空 body 明确显示零", () => {
  const empty = { utf8_bytes: 0, unicode_scalars: 0, complete: true };
  const markup = render({ io_audit: { ...audit, request_body_bytes: 0, response_body_bytes: 0, input_text: empty, output_text: empty }, io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 0, output_tokens: 0 }, usage_input_tokens: 0, usage: { completion_tokens: 0 } });
  assert.match(row(markup, "request-body"), /0 字节.*完整/);
  assert.match(row(markup, "response-body"), /0 字节.*完整/);
  assert.match(row(markup, "input-text"), /0 UTF-8 字节.*0 Unicode 字符.*完整/);
  assert.match(row(markup, "input-reference"), /0.*完整/);
  assert.equal(row(markup, "recorded-input"), "记录输入 Token0");
  assert.equal(row(markup, "recorded-output"), "记录输出 Token0");
});

// 将未结束的观测当作完整交换，或把缺失输出补零，会使此检查失败。
test("部分流量与部分文本保留前缀数量及部分状态", () => {
  const markup = render({ io_audit: { ...audit, response_complete: false, input_text: { ...audit.input_text, complete: false }, output_text: null }, io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 2 } });
  assert.match(row(markup, "response-body"), /95 字节.*部分/);
  assert.match(row(markup, "input-text"), /8 UTF-8 字节.*部分/);
  assert.match(row(markup, "input-reference"), /2.*部分/);
  assert.match(row(markup, "output-text"), /未采集/);
  assert.match(row(markup, "output-reference"), /不可用/);
});

// 将网关估算标成上游报告，会误导用量来源。
test("估算来源和客户端文本范围明确，不宣称完整计费输入", () => {
  const markup = render({ io_audit: audit, usage_estimated: true });
  const text = markup.replace(/<[^>]+>/g, "");
  assert.match(row(markup, "usage-source"), /包含网关估算/);
  assert.doesNotMatch(text, /上游报告的用量/);
  for (const limit of ["客户端", "工具定义", "结构描述", "多模态", "隐藏推理", "存储上下文", "不用于计费"]) assert.ok(text.includes(limit), limit);
});

// 给 WebSocket 的 body 填入合成 HTTP 流量会使此检查失败。
test("逻辑文本可用而 HTTP body 未采集时不补造流量", () => {
  const markup = render({ io_audit: { ...audit, request_body_bytes: null, response_body_bytes: null } });
  assert.match(row(markup, "request-body"), /未采集/);
  assert.match(row(markup, "response-body"), /未采集/);
  assert.match(row(markup, "input-text"), /8 UTF-8 字节/);
});

// 文案保持九项测量和来源信息，说明面向用户而非暴露内部槽位。
test("输入输出详情使用清晰大小标签并保留三项范围说明", () => {
  const markup = render({ io_audit: audit });
  assert.equal((markup.match(/data-io-field=/g) ?? []).length, 9);
  assert.equal(row(markup, "request-body"), "客户端请求正文140 字节 · 完整");
  assert.equal(row(markup, "response-body"), "客户端响应正文95 字节 · 完整");
  const text = markup.replace(/<[^>]+>/g, "");
  for (const limit of ["客户端可见文本", "不代表完整计费输入", "未校准", "UTF-8 字节数 ÷ 4 向上取整", "不用于计费", "压缩后", "不保证客户端收到"]) assert.ok(text.includes(limit), limit);
});

// 将历史客户端边界重标成上游，或只检查 version 而忽略 boundary，会使检查失败。
test("仅精确支持的版本与边界组合展示对应口径", () => {
  const cases = [
    { version: 1, boundary: "client_exchange", prefix: "客户端" },
    { version: 2, boundary: "upstream_attempt", prefix: "上游" },
  ];
  for (const supported of cases) {
    const markup = render({ io_audit: { ...audit, ...supported, request_complete: true } });
    assert.equal(row(markup, "request-body"), `${supported.prefix}请求正文140 字节 · 完整`);
    assert.equal(row(markup, "response-body"), `${supported.prefix}响应正文95 字节 · 完整`);
    assert.equal(row(markup, "input-text"), `${supported.prefix}输入文本8 UTF-8 字节 · 3 Unicode 字符 · 完整`);
    assert.equal(row(markup, "output-text"), `${supported.prefix}输出文本8 UTF-8 字节 · 3 Unicode 字符 · 完整`);
  }
});

// 越过版本/边界检查泄漏数值或参考，会将不兼容数据伪装为已知测量。
test("版本错配与未知边界不展示测量或参考，仍保留记录用量", () => {
  for (const unsupported of [
    { version: 1, boundary: "upstream_attempt" },
    { version: 2, boundary: "client_exchange" },
    { version: 3, boundary: "upstream_attempt" },
    { version: 2, boundary: "future_boundary" },
  ]) {
    const markup = render({ io_audit: { ...audit, ...unsupported }, io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 2, output_tokens: 2 } });
    for (const field of ["request-body", "response-body", "input-text", "output-text"]) assert.match(row(markup, field), /未采集$/);
    for (const field of ["input-reference", "output-reference"]) assert.match(row(markup, field), /不可用$/);
    assert.doesNotMatch(row(markup, "request-body"), /上游|客户端/);
    assert.match(markup.replace(/<[^>]+>/g, ""), /统计版本或边界暂不支持/);
    assert.equal(row(markup, "recorded-input"), "记录输入 Token137");
  }
});

// 缺少采集不能推断为零，也不能让后端独立参考掩盖没有文本观察的事实。
test("无审计时即使有参考也不补造文本 Token 参考", () => {
  const markup = render({ io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 0, output_tokens: 2 } });
  assert.match(row(markup, "input-reference"), /不可用$/);
  assert.match(row(markup, "output-reference"), /不可用$/);
});

// 将 request_complete 硬编码成完整、把未知补成部分，都会使此检查失败。
test("上游请求完整、部分与未知保持独立状态", () => {
  for (const fixture of [
    { request_complete: true, expected: "完整" },
    { request_complete: false, expected: "部分" },
    { request_complete: null, expected: "完整性未知" },
    { request_complete: undefined, expected: "完整性未知" },
  ]) {
    const markup = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", request_complete: fixture.request_complete } });
    assert.equal(row(markup, "request-body"), `上游请求正文140 字节 · ${fixture.expected}`);
  }
});

// 把部分上游前缀当作完整输出，或把真实零当作缺失，会改变字面结果。
test("上游部分和真实零不因来源变更而丢失", () => {
  const empty = { utf8_bytes: 0, unicode_scalars: 0, complete: true };
  const markup = render({
    io_audit: { ...audit, version: 2, boundary: "upstream_attempt", request_complete: false, request_body_bytes: 0, response_body_bytes: 0, response_complete: false, input_text: empty, output_text: { ...empty, complete: false } },
    io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 0, output_tokens: 0 },
  });
  assert.equal(row(markup, "request-body"), "上游请求正文0 字节 · 部分");
  assert.equal(row(markup, "response-body"), "上游响应正文0 字节 · 部分");
  assert.equal(row(markup, "input-text"), "上游输入文本0 UTF-8 字节 · 0 Unicode 字符 · 完整");
  assert.equal(row(markup, "output-text"), "上游输出文本0 UTF-8 字节 · 0 Unicode 字符 · 部分");
  assert.equal(row(markup, "input-reference"), "输入 Token 参考0 · 完整");
  assert.equal(row(markup, "output-reference"), "输出 Token 参考0 · 部分");
});

// 只显示 response_complete 而凭空补造正文与文本，会使此检查失败。
test("上游未采集值保持未知，不能以零填充", () => {
  const markup = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", request_body_bytes: null, response_body_bytes: null, input_text: null, output_text: null } });
  for (const field of ["request-body", "response-body", "input-text", "output-text"]) assert.match(row(markup, field), /未采集$/);
  for (const field of ["input-reference", "output-reference"]) assert.match(row(markup, field), /不可用$/);
});

// 多次交换不能被误当成一组请求和响应；单次与未知次数不伪造合计。
test("上游多次交换明确显示合计，历史数据不显示上游合计", () => {
  const aggregate = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", exchange_count: 3 } }).replace(/<[^>]+>/g, "");
  assert.match(aggregate, /3 次上游交换合计/);
  for (const extra of [{ version: 2, boundary: "upstream_attempt", exchange_count: 1 }, { version: 2, boundary: "upstream_attempt" }, { exchange_count: 3 }]) {
    assert.doesNotMatch(render({ io_audit: { ...audit, ...extra } }).replace(/<[^>]+>/g, ""), /次上游交换合计/);
  }
});

// 未知算法不能沿用 UTF-8/4 的解释，造成参考来源错配。
test("未知参考算法不显示数值", () => {
  const markup = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt" }, io_token_reference: { method: "future_method", input_tokens: 2, output_tokens: 2 } });
  assert.equal(row(markup, "input-reference"), "输入 Token 参考不可用");
  assert.equal(row(markup, "output-reference"), "输出 Token 参考不可用");
});

// 上游边界必须使用自身正文和已观测文本，不能沿用客户端交付或压缩网络口径。
test("上游说明区分正文、可解码文本及未校准参考", () => {
  const text = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", transport: "http_entity" }, usage_estimated: true }).replace(/<[^>]+>/g, "");
  for (const limit of ["本次上游尝试", "本次已观测的可解码内容", "不代表完整计费用量", "JSON", "SSE", "应用层", "解压后", "压缩后网络流量", "未校准", "不用于计费", "工具", "结构描述", "媒体", "隐藏推理", "托管上下文"]) assert.ok(text.includes(limit), limit);
  assert.doesNotMatch(text, /仅统计客户端可见文本|不保证客户端收到|上游报告的用量|未包含工具定义/);
});

// WebSocket、混合和未知传输不能被统一解释成解压后的 HTTP 正文。
test("上游传输范围跟随元数据，未知值不冒充 HTTP 正文", () => {
  for (const fixture of [
    { transport: "http_entity", includes: "HTTP 应用层正文", excludes: "上游传输类型未记录或暂不支持" },
    { transport: "websocket_payload", includes: "WebSocket 消息载荷", excludes: "响应按解压后计" },
    { transport: "mixed", includes: "HTTP 正文与 WebSocket 消息载荷混合合计", excludes: "上游传输类型未记录或暂不支持" },
    { transport: "future_transport", includes: "上游传输类型未记录或暂不支持", excludes: "响应按解压后计" },
    { transport: null, includes: "上游传输类型未记录或暂不支持", excludes: "响应按解压后计" },
    { transport: undefined, includes: "上游传输类型未记录或暂不支持", excludes: "响应按解压后计" },
  ]) {
    const text = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", transport: fixture.transport } }).replace(/<[^>]+>/g, "");
    assert.ok(text.includes(fixture.includes), String(fixture.transport));
    assert.ok(!text.includes(fixture.excludes), String(fixture.transport));
    assert.ok(!text.includes("future_transport"));
  }
});

// 多次交换的字节合计不能伪装成单次 Token 对照，也不能改变已记录用量。
test("多次上游交换说明缺少单次 Token 对照，单次不受影响", () => {
  const multiple = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", exchange_count: 3 }, io_token_reference: null });
  assert.match(multiple.replace(/<[^>]+>/g, ""), /包含多次上游交换，暂不提供单次 Token 对照。/);
  assert.equal(row(multiple, "request-body"), "上游请求正文140 字节 · 完整性未知");
  assert.equal(row(multiple, "response-body"), "上游响应正文95 字节 · 完整");
  assert.equal(row(multiple, "input-text"), "上游输入文本8 UTF-8 字节 · 3 Unicode 字符 · 完整");
  assert.equal(row(multiple, "input-reference"), "输入 Token 参考不可用");
  assert.equal(row(multiple, "output-reference"), "输出 Token 参考不可用");
  assert.equal(row(multiple, "recorded-input"), "记录输入 Token137");
  assert.equal(row(multiple, "recorded-output"), "记录输出 Token11");
  for (const exchange_count of [1, null, undefined]) {
    const single = render({ io_audit: { ...audit, version: 2, boundary: "upstream_attempt", exchange_count }, io_token_reference: { method: "utf8_bytes_div4_v1", input_tokens: 2, output_tokens: 2 } });
    assert.doesNotMatch(single.replace(/<[^>]+>/g, ""), /包含多次上游交换，暂不提供单次 Token 对照。/);
    assert.equal(row(single, "input-reference"), "输入 Token 参考2 · 完整");
    assert.equal(row(single, "output-reference"), "输出 Token 参考2 · 完整");
    assert.equal(row(single, "recorded-input"), "记录输入 Token137");
    assert.equal(row(single, "recorded-output"), "记录输出 Token11");
  }
});
