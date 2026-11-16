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

// 新汇算接口的金额必须始终以十进制字符串传递。
test("本利汇算保留超安全整数金额，利润与利润率在相同已知成本总体计算", async () => {
  const { decodeProfitSummary } = await import("./totals.ts");
  const answer = decodeProfitSummary({ totals: {
    charged_micro_cny: "9007199254741001", adjusted_micro_cny: "10",
    known_cost_micro_cny: "3602879701896400", charged_with_cost_micro_cny: "9007199254741001",
    rows_with_cost: "2", rows_without_cost: "0", unpriced_rows: "0", rows: "2",
  } });
  assert.equal(answer.revenueMicroCny, 9007199254741001n);
  assert.equal(answer.profitMicroCny, 5404319552844601n);
  assert.equal(answer.marginPct, "60.00");
});

test("缺失成本与待计价不能变成完整利润，已知部分单列", async () => {
  const { decodeProfitSummary } = await import("./totals.ts");
  for (const [missing, unpriced] of [["1", "0"], ["0", "1"]]) {
    const answer = decodeProfitSummary({ totals: {
      charged_micro_cny: "120", adjusted_micro_cny: "20", known_cost_micro_cny: "40",
      charged_with_cost_micro_cny: "100", rows_with_cost: "1", rows_without_cost: missing,
      unpriced_rows: unpriced, rows: "2",
    } });
    assert.equal(answer.profitMicroCny, null);
    assert.equal(answer.marginPct, null);
    assert.equal(answer.knownProfitMicroCny, 60n);
    assert.equal(answer.knownMarginPct, "60.00");
    assert.equal(answer.revenueMicroCny, 120n);
  }
});

test("零收入的利润率未定义，负利润与半分位四舍五入正确", async () => {
  const { decodeProfitSummary } = await import("./totals.ts");
  const decode = (revenue: string, cost: string) => decodeProfitSummary({ totals: {
    charged_micro_cny: revenue, adjusted_micro_cny: "0", known_cost_micro_cny: cost,
    charged_with_cost_micro_cny: revenue, rows_with_cost: "1", rows_without_cost: "0",
    unpriced_rows: "0", rows: "1",
  } });
  assert.equal(decode("0", "40").marginPct, null);
  assert.equal(decode("100", "125").marginPct, "-25.00");
  assert.equal(decode("32", "31").marginPct, "3.13");
});

test("本利汇算拒绝缺失覆盖和不安全数值，不能静默补零", async () => {
  const { decodeProfitSummary } = await import("./totals.ts");
  const row: Record<string, unknown> = {
    charged_micro_cny: "100", adjusted_micro_cny: "0", known_cost_micro_cny: "40",
    charged_with_cost_micro_cny: "100", rows_with_cost: "1", rows_without_cost: "0",
    unpriced_rows: "0", rows: "1",
  };
  for (const field of Object.keys(row)) {
    const absent = { ...row }; delete absent[field];
    assert.throws(() => decodeProfitSummary({ totals: absent }), /统计响应/);
    assert.throws(() => decodeProfitSummary({ totals: { ...row, [field]: Number.MAX_SAFE_INTEGER + 1 } }), /统计响应/);
  }
});

test("本利汇算窗口支持全时段和超过92天的范围，拒绝倒置范围", async () => {
  const { profitRangeError } = await import("./totals.ts");
  assert.equal(profitRangeError(), null);
  assert.equal(profitRangeError("2020-01-01T00:00:00Z", "2026-01-01T00:00:00Z"), null);
  assert.match(profitRangeError("2026-01-01T00:00:00Z", "2020-01-01T00:00:00Z") ?? "", /晚于/);
  assert.match(profitRangeError("bad", "2026-01-01T00:00:00Z") ?? "", /时间/);
  assert.match(profitRangeError("2020-01-01T00:00:00Z") ?? "", /开始和结束/);
});
