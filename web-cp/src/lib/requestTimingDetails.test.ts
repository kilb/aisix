import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 仅静态渲染真实组件，不运行浏览器或替换网络。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { RequestTimingDetails } from "./components/RequestTimingDetails.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "request-timing-details.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { RequestTimingDetails } = loaded.exports as { RequestTimingDetails: ComponentType<{ data?: Record<string, unknown> | null }> };
const render = (data?: Record<string, unknown> | null) => renderToStaticMarkup(createElement(RequestTimingDetails, { data }));
function value(markup: string, name: string): string {
  const row = markup.match(new RegExp(`<div[^>]*data-request-timing-field="${name}"[^>]*>(.*?)</div>`, "s"));
  assert.ok(row, `缺少 ${name} 阶段字段`);
  const dd = row[1]!.match(/<dd[^>]*>(.*?)<\/dd>/s);
  assert.ok(dd);
  return dd[1]!.replace(/<[^>]+>/g, "");
}

test("历史缺失阶段显示未记录，不用零或其他耗时补齐", () => {
  for (const data of [undefined, null]) {
    const markup = render(data);
    for (const key of ["body_read_ms", "json_decode_ms", "pre_handler_ms", "request_body_bytes", "request_body_complete"]) assert.equal(value(markup, key), "未记录");
    assert.match(markup, /此记录未采集请求入口阶段/);
  }
});

test("实测零耗时与零字节明确保留", () => {
  const markup = render({ body_read_ms: 0, json_decode_ms: 0, pre_handler_ms: 0, request_body_bytes: 0, request_body_complete: true });
  for (const key of ["body_read_ms", "json_decode_ms", "pre_handler_ms"]) assert.equal(value(markup, key), "0 ms");
  assert.equal(value(markup, "request_body_bytes"), "已读取 0 字节");
  assert.equal(value(markup, "request_body_complete"), "已读至结束");
});

test("部分读取的窗口与字节仍显示，未执行JSON解析保持未知", () => {
  const markup = render({ body_read_ms: 17, pre_handler_ms: 0, request_body_bytes: 32, request_body_complete: false });
  assert.equal(value(markup, "body_read_ms"), "17 ms");
  assert.equal(value(markup, "json_decode_ms"), "未记录");
  assert.equal(value(markup, "request_body_bytes"), "已读取 32 字节");
  assert.equal(value(markup, "request_body_complete"), "未完成");
});

test("先进入handler再读体的独立窗口不被相加或纠正", () => {
  const markup = render({ body_read_ms: 2500, json_decode_ms: 0, pre_handler_ms: 1, request_body_complete: false });
  assert.equal(value(markup, "body_read_ms"), "2.5 s");
  assert.equal(value(markup, "pre_handler_ms"), "1 ms");
  assert.equal(value(markup, "request_body_bytes"), "未记录");
  assert.match(markup, /部分接口先进入 handler 再读取请求体/);
  assert.match(markup, /不能直接相加/);
  assert.match(markup, /首 Token 延迟从请求到达起计，包含上传和处理时间/);
  assert.match(markup, /上游首帧延迟只对应本次上游尝试/);
});
