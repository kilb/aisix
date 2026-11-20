import assert from "node:assert/strict";
import { test } from "node:test";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import ts from "typescript";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 执行产品的真实时间区块；固定输入仅用于组件单测，不替换浏览器 API。
const require = createRequire(import.meta.url);
const source = new URL("../components/CallLogDetails.tsx", import.meta.url);
const contents = await readFile(source, "utf8");
const syntax = ts.createSourceFile(fileURLToPath(source), contents, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
let timing: ts.JsxElement | undefined;
function visit(node: ts.Node): void {
  if (ts.isJsxElement(node) && node.openingElement.tagName.getText(syntax) === "section" && node.openingElement.attributes.properties.some(attribute => ts.isJsxAttribute(attribute) && attribute.name.getText(syntax) === "aria-label" && attribute.initializer && ts.isStringLiteral(attribute.initializer) && attribute.initializer.text === "时间与性能")) timing = node;
  ts.forEachChild(node, visit);
}
visit(syntax);
assert.ok(timing, "调用详情必须保留实际时间与性能区块");
const compiled = await build({
  stdin: { loader: "tsx", sourcefile: fileURLToPath(source), resolveDir: fileURLToPath(new URL(".", source)), contents: `${contents}\nexport const TimingUnderTest = ({ event, charge, stored }) => (${timing.getText(syntax)});` },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { TimingUnderTest } = loaded.exports as { TimingUnderTest: ComponentType<Record<string, unknown>> };
const render = (event?: Record<string, unknown>, charge?: Record<string, unknown>) => renderToStaticMarkup(createElement(TimingUnderTest, { event, charge }));
function field(markup: string, label: string): string {
  const match = [...markup.matchAll(/<div><dt(?: [^>]*)?>(.*?)<\/dt><dd>(.*?)<\/dd><\/div>/g)].find(row => row[1] === label);
  assert.ok(match, `时间区块缺少 ${label}`);
  return match[2]!;
}

// 若响应阶段误取首字或上游完整耗时，三个独立数值会错位。
test("响应阶段与首字、完整上游尝试保持独立字段和准确语义", () => {
  const markup = render({ downstream_latency_ms: 3780, first_token_latency_ms: 132821, upstream_latency_ms: 115539 });
  assert.equal(field(markup, "下游响应阶段耗时"), "3.78 s");
  assert.equal(field(markup, "请求首内容"), "132.82 s");
  assert.equal(field(markup, "本次上游耗时"), "115.54 s");
  assert.doesNotMatch(markup, /<dt[^>]*>客户端总耗时<\/dt>/);
  assert.match(markup, /title="[^"]*处理起点[^"]*"/);
  assert.match(markup, /title="[^"]*首次下游协议输出[^"]*元数据[^"]*"/);
  assert.match(markup, /title="[^"]*不代表客户端完整请求耗时[^"]*"/);
});

// 用真假判断会吞掉真实零；用其他时钟兜底则会掩盖未采集。
test("响应阶段的实测零与缺失明确分开且不借其他字段补齐", () => {
  assert.equal(field(render({ downstream_latency_ms: 0, first_token_latency_ms: 990, upstream_latency_ms: 2500 }), "下游响应阶段耗时"), "0 ms");
  for (const downstream_latency_ms of [undefined, null, NaN, "17"]) {
    const markup = render({ downstream_latency_ms, first_token_latency_ms: 990, upstream_latency_ms: 2500 }, { latency_ms: 123, attempt_duration_ms: 345 });
    assert.equal(field(markup, "下游响应阶段耗时"), "—");
    assert.equal(field(markup, "请求首内容"), "990 ms");
  }
  assert.equal(field(render(undefined, { first_token_latency_ms: 0, attempt_duration_ms: 2531, latency_ms: 999 }), "下游响应阶段耗时"), "—");
});

test("客户端断开保留未测耗时的零哨兵，已知观测显示尝试开始至断开", () => {
  const event = { error_class: "client_disconnected", upstream_timing: { http_send_started_ms: 1 } };
  assert.equal(field(render({ ...event, upstream_latency_ms: 0 }), "尝试开始至断开"), "—");
  assert.equal(field(render({ ...event, upstream_latency_ms: 301_149 }), "尝试开始至断开"), "301.15 s");
  assert.equal(field(render({ upstream_latency_ms: 0 }), "本次上游耗时"), "—");
});
