import assert from "node:assert/strict";
import { test } from "node:test";
import { isKeyBudgetAmount, keyBudgetAmountInput } from "./keyBudget.ts";

test("新费用额度沿用八位整数、两位小数的元分合同并拒绝零", () => {
  for (const text of ["0.01", "1", "12.34", "99999999.99"]) assert.equal(isKeyBudgetAmount(text), true, text);
  for (const text of ["", "0", "0.00", "-1", "1e3", "1.001", "100000000", "0.000001", "NaN"]) assert.equal(isKeyBudgetAmount(text), false, text);
});

test("编辑已有额度不会将不足一分的金额截断", () => {
  assert.equal(keyBudgetAmountInput(1), "0.000001");
  assert.equal(keyBudgetAmountInput(12_340_001), "12.340001");
  assert.equal(keyBudgetAmountInput("9223372036854775807"), "9223372036854.775807");
  assert.throws(() => keyBudgetAmountInput(Number.MAX_SAFE_INTEGER + 1), RangeError);
});
