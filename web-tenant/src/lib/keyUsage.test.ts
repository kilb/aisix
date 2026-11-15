import assert from "node:assert/strict";
import test from "node:test";
import { decodeKeyCosts, keyCostNotes, keyMonthRange } from "./keyUsage.ts";
import { fmtMicroCny } from "./money.ts";

const range = { from: "2026-10-31T16:00:00.000Z", to: "2026-11-15T19:30:00.000Z" };

test("month starts at Beijing midnight and ends at the actual current instant", () => {
  assert.deepEqual(keyMonthRange(new Date(range.to)), range);
  assert.deepEqual(keyMonthRange(new Date("2026-12-31T17:00:00Z")), { from: "2026-12-31T16:00:00.000Z", to: "2026-12-31T17:00:00.000Z" });
});

test("batch charges preserve micro-CNY beyond the JavaScript safe integer limit", () => {
  const decoded = decodeKeyCosts({ ...range, items: [{ api_key_id: "key-a", charged_micro_cny: "9007199254740993", unpriced_requests: 0, pending_requests: 0 }] });
  assert.equal(fmtMicroCny(decoded.items.get("key-a")!.chargedMicroCny!), "¥9,007,199,254.740993");
  assert.equal(decoded.items.has("missing-key"), false);
});

test("explicit zero differs from a missing or unsafe amount", () => {
  const decoded = decodeKeyCosts({ ...range, items: [
    { api_key_id: "zero", charged_micro_cny: "0", unpriced_requests: 0, pending_requests: 0 },
    { api_key_id: "absent", unpriced_requests: 1, pending_requests: 1 },
    { api_key_id: "unsafe", charged_micro_cny: 9007199254740992 },
  ] });
  assert.equal(decoded.items.get("zero")?.chargedMicroCny, 0n);
  assert.equal(decoded.items.get("absent")?.chargedMicroCny, null);
  assert.equal(decoded.items.get("unsafe")?.chargedMicroCny, null);
  assert.deepEqual(keyCostNotes(decoded.items.get("absent")!), ["待计价 1 次"]);
  assert.deepEqual(keyCostNotes(decoded.items.get("unsafe")!), ["计价状态未知"]);
});

test("pending rows never disappear behind a zero priced subtotal", () => {
  const decoded = decodeKeyCosts({ ...range, items: [{ api_key_id: "pending", charged_micro_cny: "0", unpriced_requests: 2, pending_requests: 3 }] });
  assert.deepEqual(keyCostNotes(decoded.items.get("pending")!), ["待计价 2 次", "另有 1 次处理中"]);
});

test("incomplete batch identity cannot masquerade as an empty successful month", () => {
  assert.throws(() => decodeKeyCosts({ items: [] }), /费用响应/);
  assert.throws(() => decodeKeyCosts({ ...range, items: [{ charged_micro_cny: "0" }] }), /密钥/);
});
