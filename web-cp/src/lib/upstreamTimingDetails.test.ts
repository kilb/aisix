import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 静态渲染真实组件，不运行浏览器或替换网络；缺阶段及错误换算必须使断言失败。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { UpstreamTimingDetails } from "./components/UpstreamTimingDetails.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "upstream-timing-details.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { UpstreamTimingDetails } = loaded.exports as { UpstreamTimingDetails: ComponentType<{ data?: Record<string, unknown> | null }> };
const render = (data?: Record<string, unknown> | null) => renderToStaticMarkup(createElement(UpstreamTimingDetails, { data }));
const fields = ["http_send_started_ms", "response_headers_ms", "first_response_byte_ms", "first_protocol_frame_ms", "first_canonical_chunk_ms", "first_content_ms"];
function value(markup: string, name: string): string {
  const row = markup.match(new RegExp(`<div[^>]*data-upstream-timing-field="${name}"[^>]*>(.*?)</div>`, "s"));
  assert.ok(row, `缺少 ${name} 阶段字段`);
  const dd = row[1]!.match(/<dd[^>]*>(.*?)<\/dd>/s);
  assert.ok(dd);
  return dd[1]!.replace(/<[^>]+>/g, "");
}

test("上游 HTTP 阶段始终列出六项，历史缺失保留未知", () => {
  for (const data of [undefined, null, {}]) {
    const markup = render(data);
    assert.match(markup, /<section[^>]*aria-label="上游 HTTP 阶段"/);
    assert.match(markup, /<h3[^>]*>上游 HTTP 阶段<\/h3>/);
    for (const field of fields) assert.equal(value(markup, field), "—");
  }
});

test("同一次尝试的 offset 保留零及原始整数毫秒，不相减或舍入为秒", () => {
  const markup = render({ http_send_started_ms: 0, response_headers_ms: 2500, first_response_byte_ms: 17, first_protocol_frame_ms: 65536, first_canonical_chunk_ms: 4294967295, first_content_ms: 42 });
  const expected = ["0 ms", "2,500 ms", "17 ms", "65,536 ms", "4,294,967,295 ms", "42 ms"];
  fields.forEach((field, index) => assert.equal(value(markup, field), expected[index]));
  assert.match(markup, /相对同一次上游尝试起点/);
  assert.match(markup, /不计算阶段差值/);
});

test("所有阶段严格拒绝非法 u32，缺失协议与规范化片段不补齐", () => {
  for (const invalid of [undefined, null, "12", true, NaN, Infinity, -Infinity, -1, 0.5, 4294967296]) {
    const markup = render(Object.fromEntries(fields.map(field => [field, invalid])));
    for (const field of fields) assert.equal(value(markup, field), "—", `${field} 不得接受 ${String(invalid)}`);
  }
  const partial = render({ response_headers_ms: 10, first_content_ms: 20 });
  assert.equal(value(partial, "first_protocol_frame_ms"), "—");
  assert.equal(value(partial, "first_canonical_chunk_ms"), "—");
});

test("阶段标签和边界说明不把客户端调用或内容交付描述为网络确认", () => {
  const markup = render({});
  for (const label of ["交给 HTTP 客户端", "收到响应头", "首个响应体字节", "首个协议帧", "首个规范化片段", "首次内容交付"]) assert.match(markup, new RegExp(`<dt[^>]*>${label}</dt>`));
  assert.match(markup, /交给 HTTP 客户端表示开始调用 HTTP 客户端，不代表操作系统实际写包/);
  assert.match(markup, /首次内容交付表示网关交给 HTTP server，不代表客户端已收到或确认/);
});
