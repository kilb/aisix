import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { createElement, useState, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { chromium, expect } from "@playwright/test";
import type { RecordTablePagerProps, RecordTableToolbarProps } from "../ui/RecordTable";
import { useRecordTable } from "./recordTable.ts";

// 编译并渲染真实控件；不替换网络或组件，也不生成临时文件。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { RecordTableToolbar, RecordTablePager } from "./ui/RecordTable.tsx"; export { DataTable } from "./ui/DataTable.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "record-table-controls.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { RecordTableToolbar, RecordTablePager, DataTable } = loaded.exports as {
  RecordTableToolbar: ComponentType<RecordTableToolbarProps>;
  RecordTablePager: ComponentType<RecordTablePagerProps>;
  DataTable: ComponentType<{ label?: string; children: ReturnType<typeof createElement> }>;
};
const noChange = () => undefined;
const base: RecordTablePagerProps = { page: 1, pageSize: 25, onPageChange: noChange, onPageSizeChange: noChange };
const textOf = (markup: string) => markup.replace(/<[^>]+>/g, "");

// 可见箭头仍须带原分页名称，禁用与页边界不能因图标化改变。
function assertPageButton(markup: string, label: string, disabled: boolean) {
  const buttons = [...markup.matchAll(/<button[^>]*>[\s\S]*?<\/button>/g)].map(match => match[0]);
  const matched = buttons.filter(button => button.includes(`aria-label="${label}"`));
  assert.equal(matched.length, 1);
  const button = matched[0]!;
  assert.equal(/ disabled=""/.test(button), disabled);
  assert.match(button, /type="button"/);
  assert.match(button, /<svg[^>]*aria-hidden="true"/);
  assert.equal(textOf(button), "");
}

// 缺少label或原生type=search会破坏键盘及读屏搜索入口；业务筛选必须仍在工具栏内。
test("搜索带显式可访问名称并保留业务筛选插槽", () => {
  const markup = renderToStaticMarkup(createElement(RecordTableToolbar, {
    search: { value: "Alpha", onChange: noChange, label: "搜索模型", placeholder: "名称或ID" },
    children: createElement("label", null, "状态", createElement("select", null, createElement("option", null, "全部"))),
  }));
  assert.match(markup, /<label[^>]*>[\s\S]*搜索模型[\s\S]*<input[^>]*type="search"/);
  assert.match(markup, /value="Alpha"/);
  assert.match(textOf(markup), /状态全部/);
});

// 若用已加载数量冒充total或显示跳页框，会暗示未持有cursor的记录可随机跳转。
test("游标页只报告已加载数量，不伪造总数或提供跳页输入", () => {
  const markup = renderToStaticMarkup(createElement(RecordTablePager, { ...base, loadedCount: 25, hasNext: true, label: "历史分页" }));
  assert.match(markup, /aria-label="历史分页"/);
  assert.match(textOf(markup), /已加载 25 条/);
  assert.doesNotMatch(textOf(markup), /共 25 条/);
  assert.doesNotMatch(markup, /<input/);
  assertPageButton(markup, "上一页", true);
  assertPageButton(markup, "下一页", false);
});

// 若total忽略末页边界，下一页不会禁用；页大小选项必须保持真实可选。
test("已知总数给出精确显示范围，末页与空表阻止越界", () => {
  const last = renderToStaticMarkup(createElement(RecordTablePager, { ...base, page: 3, total: 61 }));
  assert.match(textOf(last), /51[–—-]61.*共 61 条/);
  assertPageButton(last, "下一页", true);
  assert.match(last, /role="combobox"[^>]*data-value="25"/);
  const empty = renderToStaticMarkup(createElement(RecordTablePager, { ...base, total: 0 }));
  assert.match(textOf(empty), /0[–—-]0.*共 0 条/);
  assert.equal(empty.match(/<button[^>]*disabled=""/g)?.length, 2);
});

// 若busy仍可翻页或改页大小，会并发沿用过时cursor。
test("请求中锁定页导航与页大小，状态区可被辅助技术播报", () => {
  const markup = renderToStaticMarkup(createElement(RecordTablePager, { ...base, page: 2, hasNext: true, loadedCount: 50, busy: true }));
  assertPageButton(markup, "上一页", true);
  assertPageButton(markup, "下一页", true);
  assert.match(markup.match(/<button[^>]*role="combobox"[^>]*>/)?.[0] ?? "", /disabled=""/);
  assert.match(markup, /role="status"/);
});

// 若hook没有把条件变化同步到页码，第二次render将保留第3页；真实hook必须参与检查。
test("真实hook默认25条，查询与resetKey变化立即重置页并保持源记录数量", () => {
  const source = Array.from({ length: 61 }, (_, index) => ({ name: index === 30 ? "Alpha" : `模型 ${index + 1}` }));
  const snapshots: { stage: number; page: number; count: number; filtered: number; total: number }[] = [];
  function Journey() {
    const [stage, setStage] = useState(0);
    const view = useRecordTable({ items: source, searchText: (row) => row.name, resetKey: stage >= 5 ? "disabled" : "enabled" });
    snapshots.push({ stage, page: view.page, count: view.rows.length, filtered: view.filteredCount, total: view.total });
    if (stage === 0) { view.setPage(3); setStage(1); }
    if (stage === 1) { view.setQuery("模型"); setStage(2); }
    if (stage === 2) { view.setQuery("Alpha"); setStage(3); }
    if (stage === 3) { view.setQuery(""); view.setPage(3); setStage(4); }
    if (stage === 4) setStage(5);
    return createElement("span", null, view.rows[0]?.name);
  }
  renderToStaticMarkup(createElement(Journey));
  assert.deepEqual(snapshots[0], { stage: 0, page: 1, count: 25, filtered: 61, total: 61 });
  assert.ok(snapshots.some((value) => value.stage === 1 && value.page === 3 && value.count === 11));
  assert.ok(snapshots.some((value) => value.stage === 2 && value.page === 1 && value.filtered === 60 && value.total === 61));
  assert.ok(snapshots.some((value) => value.stage === 3 && value.page === 1 && value.filtered === 1 && value.total === 61));
  assert.ok(snapshots.some((value) => value.stage === 4 && value.page === 3));
  assert.deepEqual(snapshots.at(-1), { stage: 5, page: 1, count: 25, filtered: 61, total: 61 });
});

// 若移除region/tabIndex，键盘无法滚动窄屏表格；原生table与表名也必须留下。
test("表格保留语义和名称，局部滚动区域可被键盘聚焦", () => {
  const markup = renderToStaticMarkup(createElement(DataTable, { label: "模型列表", children: createElement("tbody", null, createElement("tr", null, createElement("td", null, "Alpha"))) }));
  assert.match(markup, /role="region" aria-label="模型列表表格滚动区域" tabindex="0"/);
  assert.match(markup, /<table[^>]*aria-label="模型列表"/);
  assert.match(markup, /<tbody><tr><td>Alpha<\/td><\/tr><\/tbody>/);
});

// 删除任何页大小选项、丢失实际值或忙时漏锁触发器都会使连续交互失败。
test("页大小菜单实际提供10/25/50并把原值传给回调，忙时不能更改", { timeout: 30_000 }, async () => {
  const script = await build({
    stdin: { loader: "tsx", contents: `import { createRoot } from "react-dom/client"; import { useState } from "react"; import { RecordTablePager } from "./ui/RecordTable"; function Journey() { const [size,setSize]=useState(25); const [busy,setBusy]=useState(false); return <><RecordTablePager page={1} pageSize={size} total={61} onPageChange={()=>{}} onPageSizeChange={setSize} busy={busy}/><output aria-label="实际页大小">{size}</output><button onClick={()=>setBusy(true)}>锁定请求</button></>; } createRoot(document.getElementById("root")).render(<Journey/>);`, resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "record-table-size.tsx" },
    bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false, loader: { ".css": "empty" }, define: { "process.env.NODE_ENV": '"test"' },
  });
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    await page.setContent('<div id="root"></div>'); await page.addScriptTag({ content: script.outputFiles[0]!.text });
    const control = page.getByRole("combobox", { name: "记录分页每页条数", exact: true });
    for (const value of ["10", "25", "50"]) {
      await control.click();
      await expect(page.getByRole("option")).toHaveCount(3);
      await page.getByRole("option", { name: value, exact: true }).click();
      await expect(control).toHaveAttribute("data-value", value);
      await expect(page.getByRole("status", { name: "实际页大小", exact: true })).toHaveText(value);
    }
    await page.getByRole("button", { name: "锁定请求", exact: true }).click();
    await expect(control).toBeDisabled();
  } finally { await browser.close(); }
});
