import assert from "node:assert/strict";
import { test } from "node:test";
import { fmtCnyPerMillion, fmtMicroCny, fmtMicroUsd, fmtUtc, isCnyAmount, periodLabel, todayUtc, utcRange } from "./money.ts";

// `npm run test:unit`.

test("µ¥ 金额精确格式化，不经浮点", () => {
  assert.equal(fmtMicroCny(0), "¥0.00");
  assert.equal(fmtMicroCny(1), "¥0.000001");
  assert.equal(fmtMicroCny(10_000), "¥0.01");
  assert.equal(fmtMicroCny(11_000), "¥0.011");
  assert.equal(fmtMicroCny(-11_000), "-¥0.011");
  assert.equal(fmtMicroCny(1_000_000), "¥1.00");
  assert.equal(fmtMicroCny(123_456_789_012), "¥123,456.789012");
  assert.equal(fmtMicroCny(9_007_199_254_740_991), "¥9,007,199,254.740991");
  assert.equal(fmtMicroCny(-1_234_560_000), "-¥1,234.56");
});

test("µ$ 金额只剩上游成本一处使用，仍精确", () => {
  assert.equal(fmtMicroUsd(0), "$0.00");
  assert.equal(fmtMicroUsd(1), "$0.000001");
  assert.equal(fmtMicroUsd(11_000), "$0.011");
  assert.equal(fmtMicroUsd(-11_000), "-$0.011");
  assert.equal(fmtMicroUsd(123_456_789_012), "$123,456.789012");
  assert.equal(fmtMicroUsd(9_007_199_254_740_991), "$9,007,199,254.740991");
});

test("金额输入只接受人民币元与分", () => {
  for (const ok of ["0.01", "12", "99999999.99"]) assert.ok(isCnyAmount(ok, { allowZero: false }));
  for (const bad of ["0", "0.00", "0.001", "-1", "1e3", "", "1,000", " 1", "100000000"]) {
    assert.ok(!isCnyAmount(bad, { allowZero: false }));
  }
  assert.ok(isCnyAmount("0", { allowZero: true }));
  assert.ok(!isCnyAmount("100000000", { allowZero: true }));
  assert.ok(!isCnyAmount("0.001", { allowZero: true }));
});

test("每百万 token 的单价按原样给出的小数位显示，不足两位补足", () => {
  assert.equal(fmtCnyPerMillion("28.8"), "¥28.80");
  assert.equal(fmtCnyPerMillion("6"), "¥6.00");
  assert.equal(fmtCnyPerMillion("9.216"), "¥9.216");
  assert.equal(fmtCnyPerMillion("0.00000000000001"), "¥0.00000000000001");
  assert.equal(fmtCnyPerMillion("100000000"), "¥100,000,000.00");
  assert.equal(fmtCnyPerMillion("1234.5"), "¥1,234.50");
});

test("UTC 日期区间含首尾两天", () => {
  assert.deepEqual(utcRange("2026-11-01", "2026-11-07"), { from: "2026-11-01T00:00:00Z", to: "2026-11-08T00:00:00Z" });
  assert.equal(todayUtc(new Date("2026-11-07T23:30:00+08:00")), "2026-11-07");
  assert.equal(todayUtc(new Date("2026-11-08T07:30:00+08:00")), "2026-11-07");
});

test("区间的结束日跨月、跨年、闰日都按日历进位", () => {
  assert.equal(utcRange("2026-11-30", "2026-11-30").to, "2026-12-01T00:00:00Z");
  assert.equal(utcRange("2026-12-31", "2026-12-31").to, "2027-01-01T00:00:00Z");
  assert.equal(utcRange("2028-02-28", "2028-02-28").to, "2028-02-29T00:00:00Z");
});

test("时间与周期按 UTC 显示，不随浏览器时区", () => {
  assert.equal(fmtUtc("2026-11-07T13:05:09Z"), "2026-11-07 13:05:09 UTC");
  assert.equal(fmtUtc("2026-11-07T13:05:09.987654+00:00"), "2026-11-07 13:05:09 UTC");
  assert.equal(fmtUtc("2026-11-08T01:05:09+08:00"), "2026-11-07 17:05:09 UTC");
  assert.equal(fmtUtc("not-a-time"), "not-a-time");
  assert.equal(periodLabel("2026-11-07T00:00:00Z", "day"), "2026-11-07");
  assert.equal(periodLabel("2026-11-01T00:00:00Z", "month"), "2026-11");
  assert.equal(periodLabel("2026-11-08T01:00:00+08:00", "day"), "2026-11-07");
});
