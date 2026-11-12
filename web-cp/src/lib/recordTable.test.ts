import assert from "node:assert/strict";
import { test } from "node:test";
import { filterRecords, paginateRecords, recordTableReducer, type RecordTableState } from "./recordTable.ts";

const records = Array.from({ length: 61 }, (_, index) => ({
  id: index + 1,
  name: index === 30 ? "模型 Alpha" : `模型 ${index + 1}`,
  secret: "不能搜索的凭据",
  details: [`子项 ${index + 1}a`, `子项 ${index + 1}b`],
}));

// 若按展开后的子行切页，父记录的两个子项将不能完整留在同页。
test("分页以父记录为单位，保留所有详情且不改原数组", () => {
  const page = paginateRecords(records, 2, 25);
  assert.deepEqual(page.rows.map((row) => row.id), Array.from({ length: 25 }, (_, index) => index + 26));
  assert.deepEqual(page.rows[5]?.details, ["子项 31a", "子项 31b"]);
  assert.deepEqual({ total: page.total, page: page.page, pageCount: page.pageCount, start: page.start, end: page.end }, {
    total: 61, page: 2, pageCount: 3, start: 26, end: 50,
  });
  assert.equal(records.length, 61);
  assert.equal(records[0]?.id, 1);
});

// 若不夹取最后一页，删除记录后会留下一页空白；空表不能宣称第1至25条。
test("删除或过滤后夹取末页，空结果为零范围", () => {
  const last = paginateRecords(records.slice(0, 26), 3, 25);
  assert.deepEqual(last.rows.map((row) => row.id), [26]);
  assert.deepEqual({ page: last.page, start: last.start, end: last.end }, { page: 2, start: 26, end: 26 });
  assert.deepEqual(paginateRecords([], 7, 10), { rows: [], total: 0, page: 1, pageCount: 1, start: 0, end: 0 });
  assert.equal(paginateRecords(records, -3, 50).page, 1);
});

// 若把整行序列化搜索，secret将错误命中；若先分页后筛选，第二页上的Alpha无法找到。
test("搜索只读取显式公开字段，先筛选再分页并忽略首尾空格和大小写", () => {
  const alpha = filterRecords(records, "  ALPHA  ", (row) => [row.name, String(row.id)]);
  assert.deepEqual(alpha.map((row) => row.id), [31]);
  assert.equal(paginateRecords(alpha, 1, 25).rows[0]?.id, 31);
  assert.equal(filterRecords(records, "不能搜索的凭据", (row) => row.name).length, 0);
  assert.equal(filterRecords(records, "", (row) => row.name).length, 61);
});

// 若查询、字段筛选或页大小沿用第3页，即使新结果只有一页也会跳过匹配项。
test("搜索、业务筛选与页大小变化均从第一页开始，筛选不丢搜索词", () => {
  const state: RecordTableState = { query: "alpha", page: 3, pageSize: 25, resetKey: "enabled" };
  assert.deepEqual(recordTableReducer(state, { type: "query", query: "beta" }), { ...state, query: "beta", page: 1 });
  assert.deepEqual(recordTableReducer(state, { type: "pageSize", pageSize: 50 }), { ...state, pageSize: 50, page: 1 });
  assert.deepEqual(recordTableReducer(state, { type: "sync", page: 1, resetKey: "disabled" }), { ...state, page: 1, resetKey: "disabled" });
  assert.equal(recordTableReducer(state, { type: "page", page: 2 }).page, 2);
});
