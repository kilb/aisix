import assert from "node:assert/strict";
import test from "node:test";
import { overviewAccess, exactOverviewAmount, budgetAmounts } from "./overview.ts";

test("概览按角色与组织状态读取统计，不向成员请求账务", () => {
  assert.deepEqual(overviewAccess("active", "member"), { usage: true, budgets: true, billing: false });
  for (const role of ["owner", "admin", "billing"]) {
    assert.deepEqual(overviewAccess("active", role), { usage: true, budgets: true, billing: true });
  }
  for (const status of ["pending", "suspended", "future"]) {
    assert.deepEqual(overviewAccess(status, "owner"), { usage: false, budgets: false, billing: false });
  }
  assert.deepEqual(overviewAccess("active", "future"), { usage: false, budgets: false, billing: false });
});

test("精确余额保留超过安全整数的金额，坏明细不能回退成零", () => {
  assert.equal(exactOverviewAmount({ available_micro_cny: 0, calculation: { available_micro_cny: "900719925474099312345" } }, "available_micro_cny"), "900719925474099312345");
  assert.equal(exactOverviewAmount({ available_micro_cny: 0, calculation: {} }, "available_micro_cny"), null);
  assert.equal(exactOverviewAmount({ available_micro_cny: 0, calculation: null }, "available_micro_cny"), null);
  assert.equal(exactOverviewAmount({ available_micro_cny: 0, calculation: { available_micro_cny: "oops" } }, "available_micro_cny"), null);
  assert.equal(exactOverviewAmount({ available_micro_cny: Number.MAX_SAFE_INTEGER + 1 }, "available_micro_cny"), null);
  assert.equal(exactOverviewAmount({ available_micro_cny: 0 }, "available_micro_cny"), "0");
  assert.equal(exactOverviewAmount({ available_micro_cny: -2500000 }, "available_micro_cny"), "-2500000");
});

test("独立预算不相加，安全零与未知金额有区别", () => {
  assert.deepEqual(budgetAmounts({ limit_micro_cny: 0, spent_micro_cny: 0, remaining_micro_cny: 0 }), { limit: 0n, spent: 0n, remaining: 0n });
  assert.deepEqual(budgetAmounts({ limit_micro_cny: 1000000, spent_micro_cny: 2000000, remaining_micro_cny: -1000000 }), { limit: 1000000n, spent: 2000000n, remaining: -1000000n });
  assert.deepEqual(budgetAmounts({ limit_micro_cny: Number.MAX_SAFE_INTEGER + 1, spent_micro_cny: 1.5, remaining_micro_cny: 0 }), { limit: null, spent: null, remaining: 0n });
});
