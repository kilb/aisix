import assert from "node:assert/strict";
import { test } from "node:test";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, Fragment } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { useRecordTable as RecordHook } from "./recordTable";

// SSR只渲染真实分页组件与本地记录，既不执行网络effect也不替换API响应。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { RecordTools, RecordPager } from "./components/RecordTools.tsx"; export { useRecordTable } from "./lib/recordTable.ts";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "operational-record-tools.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
type Components = typeof import("../components/RecordTools") & { useRecordTable: typeof RecordHook };
const module = { exports: {} } as { exports: Components };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, module, module.exports);
const { RecordTools, RecordPager, useRecordTable } = module.exports;

function ParentRecords() {
  const items = Array.from({ length: 60 }, (_, index) => ({ id: index + 1, name: `记录-${index + 1}` }));
  const table = useRecordTable({ items, searchText: (row) => row.name });
  return createElement(Fragment, null,
    createElement(RecordTools, { table, label: "普通记录" }),
    createElement("table", null, createElement("tbody", null, table.rows.map((row) => createElement(Fragment, { key: row.id }, createElement("tr", { "data-parent": row.id }, createElement("td", null, row.name)), createElement("tr", { "data-detail": row.id }, createElement("td", null, `明细-${row.id}`)))))),
    createElement(RecordPager, { table, label: "普通记录" }),
    createElement("p", null, `完整汇总 ${items.length}`),
  );
}

test("普通列表默认25个父记录，每个展开详情留在同页，完整汇总保留60", () => {
  const markup = renderToStaticMarkup(createElement(ParentRecords));
  assert.equal((markup.match(/data-parent=/g) ?? []).length, 25);
  assert.equal((markup.match(/data-detail=/g) ?? []).length, 25);
  assert.match(markup, /data-parent="25"/);
  assert.doesNotMatch(markup, /data-parent="26"|明细-26/);
  assert.match(markup, /共 60 条/);
  assert.match(markup, /完整汇总 60/);
});

function CursorRecords() {
  const table = useRecordTable({ items: [{ id: "loaded" }], searchText: (row) => row.id });
  return createElement(Fragment, null, createElement(RecordTools, { table, label: "游标记录", loaded: true }), createElement(RecordPager, { table, label: "游标记录", hasMore: true, onMore: async () => {} }));
}

test("游标列表声明只搜索已加载记录，总量未知时不伪造共计", () => {
  const markup = renderToStaticMarkup(createElement(CursorRecords));
  assert.match(markup, /在已加载记录中搜索/);
  assert.match(markup, /已加载 1 条/);
  assert.doesNotMatch(markup, /共 1 条|共 1 页/);
  assert.match(markup, /aria-label="游标记录"/);
  assert.doesNotMatch(markup, /disabled=""[^>]*>下一页/);
});
