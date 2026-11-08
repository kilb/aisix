import assert from "node:assert/strict";
import { test } from "node:test";
import { checkLabel, dropLabel, fmtCount, isPositive } from "./reconcile.ts";

test("checks and drop reasons are labelled, and unknown ones show raw", () => {
  assert.equal(checkLabel("running_balance"), "逐笔余额");
  assert.equal(checkLabel("future_check"), "future_check");
  assert.equal(dropLabel("sink_full"), "队列已满");
  assert.equal(dropLabel("send_unconfirmed"), "未确认送达（可能已入账）");
  assert.equal(dropLabel("new_reason"), "new_reason");
});

test("counts format exactly past 2^53, up to i64::MAX and u64::MAX", () => {
  assert.equal(fmtCount(0), "0");
  assert.equal(fmtCount(1234567), "1,234,567");
  assert.equal(fmtCount("9223372036854775807"), "9,223,372,036,854,775,807");
  assert.equal(fmtCount("18446744073709551615"), "18,446,744,073,709,551,615");
  assert.equal(fmtCount("not a number"), "not a number");
});

test("a positive count is told from zero at any size", () => {
  assert.equal(isPositive(0), false);
  assert.equal(isPositive("0"), false);
  assert.equal(isPositive("9223372036854775807"), true);
});
