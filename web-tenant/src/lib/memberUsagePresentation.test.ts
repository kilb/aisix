import assert from "node:assert/strict";
import test from "node:test";
import { memberChargeLabel } from "./memberUsagePresentation.ts";

// 把待计价费用当成零、把缺失当成零、或先转为浮点，都会使这些断言失败。
test("成员列表只列精确费用，不列请求数和计费用量", () => {
  assert.equal(memberChargeLabel({ charged_micro_cny: "9007199254740993", pending_requests: 0, unpriced_requests: 0 }), "¥9,007,199,254.740993");
  assert.equal(memberChargeLabel({ charged_micro_cny: "-1000001", pending_requests: 0, unpriced_requests: 0 }), "-¥1.000001");
});

test("待计价不冒充零，已计费部分仍保持原金额", () => {
  assert.equal(memberChargeLabel({ charged_micro_cny: "0", pending_requests: 2, unpriced_requests: 1 }), "待计价");
  assert.equal(memberChargeLabel({ charged_micro_cny: "1234567", pending_requests: 2, unpriced_requests: 1 }), "¥1.234567（部分待计价）");
  assert.equal(memberChargeLabel({ charged_micro_cny: "0", pending_requests: 0, unpriced_requests: 1 }), "待计价");
  assert.equal(memberChargeLabel({ charged_micro_cny: "0", pending_requests: 0, unpriced_requests: 0 }), "¥0.00");
});

test("缺失或无效金额明确标为未知", () => {
  assert.equal(memberChargeLabel(undefined), "暂无费用数据");
  assert.equal(memberChargeLabel({ charged_micro_cny: "not-a-number", pending_requests: 0, unpriced_requests: 0 }), "费用未知");
});
