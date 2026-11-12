import assert from "node:assert/strict";
import { test } from "node:test";
import {
  fmtCnyPerMillion,
  fmtMicroCny,
  isCnyAmount,
  microCnyToInput,
  perMillionOfPerThousand,
  perThousandOfPerMillion,
} from "./money.ts";

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

test("金额输入只接受人民币元与分", () => {
  for (const ok of ["0.01", "12", "99999999.99"]) assert.ok(isCnyAmount(ok, { allowZero: false }));
  for (const bad of ["0", "0.00", "0.001", "-1", "1e3", "", "1,000", " 1", "100000000"]) {
    assert.ok(!isCnyAmount(bad, { allowZero: false }));
  }
  assert.ok(isCnyAmount("0", { allowZero: true }));
  assert.ok(!isCnyAmount("100000000", { allowZero: true }));
  assert.ok(!isCnyAmount("0.001", { allowZero: true }));
});

test("µ¥ 金额转成输入框里的元，精确到分，负数的符号在最前", () => {
  assert.equal(microCnyToInput(1_500_000), "1.50");
  assert.equal(microCnyToInput(0), "0.00");
  assert.equal(microCnyToInput(10_000), "0.01");
  assert.equal(microCnyToInput(-500_000), "-0.50");
  assert.equal(microCnyToInput(-12_340_000), "-12.34");
  assert.equal(microCnyToInput(99_999_999_990_000), "99999999.99");
  // Below the fen is not an amount anyone typed: it is dropped, never rounded up.
  assert.equal(microCnyToInput(19_999), "0.01");
  for (const micro of [1_500_000, 10_000, 99_999_999_990_000]) {
    assert.ok(isCnyAmount(microCnyToInput(micro), { allowZero: false }));
  }
});

test("每百万 token 的单价按原样给出的小数位显示，不足两位补足", () => {
  assert.equal(fmtCnyPerMillion("28.8"), "¥28.80");
  assert.equal(fmtCnyPerMillion("6"), "¥6.00");
  assert.equal(fmtCnyPerMillion("9.216"), "¥9.216");
  assert.equal(fmtCnyPerMillion("0.00000000000001"), "¥0.00000000000001");
  assert.equal(fmtCnyPerMillion("100000000"), "¥100,000,000.00");
  assert.equal(fmtCnyPerMillion("1234.5"), "¥1,234.50");
});

test("每千次与每百万单位的价格互相换算是精确的十进制运算", () => {
  // Typed per thousand, stored per million: the web search price is "$10 per 1,000 searches".
  assert.equal(perMillionOfPerThousand("10"), "10000");
  assert.equal(perMillionOfPerThousand("0.0035"), "3.5");
  assert.equal(perMillionOfPerThousand("0.000000001"), "0.000001");
  assert.equal(perMillionOfPerThousand("12.5"), "12500");
  assert.equal(perMillionOfPerThousand("0"), "0");
  assert.equal(perMillionOfPerThousand("007.10"), "7100");
  // Shown per thousand from what a card stores.
  assert.equal(perThousandOfPerMillion("10000"), "10");
  assert.equal(perThousandOfPerMillion("3.5"), "0.0035");
  assert.equal(perThousandOfPerMillion("0.000001"), "0.000000001");
  assert.equal(perThousandOfPerMillion("72000.00000000000001"), "72.00000000000000001");
  assert.equal(perThousandOfPerMillion("0"), "0");
  // Round trips, both ways, never through a float (0.1 + 0.2 would show here).
  for (const perThousand of ["10", "0.0035", "0.3", "999.999999999", "1000"]) {
    assert.equal(perThousandOfPerMillion(perMillionOfPerThousand(perThousand)!), perThousand);
  }
  for (const perMillion of ["10000", "3.5", "0.000001", "300", "0.3"]) {
    assert.equal(perMillionOfPerThousand(perThousandOfPerMillion(perMillion)), perMillion);
  }
  // What a card cannot hold exactly, or is no decimal, is refused rather than rounded.
  for (const bad of ["", "0.0000000001", "-1", "1e3", "1,5", " 1", ".5", "5.", "abc"]) {
    assert.equal(perMillionOfPerThousand(bad), null, bad);
  }
  // A stored text that is no decimal is shown as it came.
  assert.equal(perThousandOfPerMillion("n/a"), "n/a");
});
