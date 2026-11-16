import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeCostTotals } from "./totals.ts";

test("全窗口分组聚合保留每组成本与缺失笔数", () => {
  const answer = decodeCostTotals({ items: [
    { cost_micro_cny: 1_200_001, rows: 2, rows_without_cost: 1 },
    { cost_micro_cny: 800_002, rows: 3, rows_without_cost: 2 },
  ] });
  assert.equal(answer.knownCostMicroCny, 2_000_003n);
  assert.equal(answer.rowsWithCost, 5n);
  assert.equal(answer.rowsWithoutCost, 3n);
});

test("只有未知成本的记录不能显示零成本，已知免费记录保留零", () => {
  assert.equal(decodeCostTotals({ items: [{ cost_micro_cny: 0, rows: 0, rows_without_cost: 4 }] }).knownCostMicroCny, null);
  assert.equal(decodeCostTotals({ items: [{ cost_micro_cny: 0, rows: 4, rows_without_cost: 0 }] }).knownCostMicroCny, 0n);
  assert.deepEqual(decodeCostTotals({ items: [] }), { knownCostMicroCny: 0n, rowsWithCost: 0n, rowsWithoutCost: 0n });
});

test("各组金额先转整数再汇总，跨安全整数边界仍精确", () => {
  const answer = decodeCostTotals({ items: [
    { cost_micro_cny: Number.MAX_SAFE_INTEGER, rows: 1, rows_without_cost: 0 },
    { cost_micro_cny: 6, rows: 1, rows_without_cost: 0 },
  ] });
  assert.equal(answer.knownCostMicroCny, 9_007_199_254_740_997n);
});

test("不安全金额或缺失覆盖字段不伪装成完整成本", () => {
  for (const row of [
    { cost_micro_cny: Number.MAX_SAFE_INTEGER + 1, rows: 1, rows_without_cost: 0 },
    { cost_micro_cny: 100, rows: 1 },
    { cost_micro_cny: 100, rows: -1, rows_without_cost: 0 },
  ]) assert.equal(decodeCostTotals({ items: [row] }).knownCostMicroCny, null);
  assert.equal(decodeCostTotals({ items: [{ cost_micro_cny: 100, rows: 1 }] }).rowsWithoutCost, null);
});

test("缺失或无效分组集合须明确失败，不能当空窗口", () => {
  for (const input of [null, {}, { items: null }, { items: [{}] }, { items: [null] }]) {
    if (input && "items" in input && Array.isArray(input.items) && input.items[0] !== null) {
      assert.equal(decodeCostTotals(input).knownCostMicroCny, null);
    } else assert.throws(() => decodeCostTotals(input), /成本统计响应/);
  }
});
