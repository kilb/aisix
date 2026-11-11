import assert from "node:assert/strict";
import test from "node:test";
import { discountedRate, feeMicroCny } from "./formula.ts";
import { fmtMicroCny } from "./money.ts";

test("整笔折扣后只向上取整一次，不能逐行或折前提前取整", () => {
  assert.equal(feeMicroCny([
    { quantity: 1, rate_cny: "0.4" },
    { quantity: 1, rate_cny: "0.4" },
  ], 1000), "1");
  assert.equal(feeMicroCny([{ quantity: 1, rate_cny: "1.08" }], 1000), "1");
  assert.equal(feeMicroCny([
    { quantity: 5, rate_cny: "1000" },
    { quantity: 3, rate_cny: "2000" },
  ], 1000), "9900");
});

test("不同十进制精度和超出JS安全整数的结果仍准确", () => {
  assert.equal(feeMicroCny([
    { quantity: 1, rate_cny: "9007199254740992.1" },
    { quantity: 1, rate_cny: "0.0000000001" },
  ], 0), "9007199254740993");
  assert.equal(discountedRate("0.0000000001", 3333), "0.00000000006667");
  assert.equal(fmtMicroCny("9007199254740993"), "¥9,007,199,254.740993");
  assert.equal(fmtMicroCny("-1"), "-¥0.000001");
});

test("真实零值、未知费率、未知数量与非法折扣互不混同", () => {
  assert.equal(feeMicroCny([{ quantity: 100, rate_cny: "2" }], 10000), "0");
  assert.equal(feeMicroCny([{ quantity: 100, rate_cny: "0" }], 0), "0");
  assert.equal(feeMicroCny([{ quantity: 1, rate_cny: null }], 0), null);
  assert.equal(feeMicroCny([{ quantity: Number.MAX_SAFE_INTEGER + 1, rate_cny: "1" }], 0), null);
  assert.equal(feeMicroCny([{ quantity: 1, rate_cny: "1" }], null), null);
  assert.equal(feeMicroCny([{ quantity: 1, rate_cny: "1" }], -1), null);
});
