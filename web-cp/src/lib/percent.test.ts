import assert from "node:assert/strict";
import { test } from "node:test";
import { discountBps, fmtPercent, markupBps } from "./percent.ts";

test("markupBps reads at most two decimals as integer basis points", () => {
  assert.equal(markupBps("0"), 0);
  assert.equal(markupBps("0.05"), 5);
  assert.equal(markupBps("12.5"), 1250);
  assert.equal(markupBps("100"), 10000);
  assert.equal(markupBps(" 20 "), 2000);
  for (const bad of ["100.001", ".5", "20.", "1e2", "-1", "", "abc", "1,5"]) assert.equal(markupBps(bad), null, bad);
});

test("discountBps is 0 to 100 percent and refuses anything above", () => {
  assert.equal(discountBps("0"), 0);
  assert.equal(discountBps("20"), 2000);
  assert.equal(discountBps("12.5"), 1250);
  assert.equal(discountBps("100"), 10000);
  for (const bad of ["100.01", "101", ".5", "20.", "1e2", "-5", ""]) assert.equal(discountBps(bad), null, bad);
});

test("fmtPercent prints basis points without float arithmetic", () => {
  assert.equal(fmtPercent(0), "0%");
  assert.equal(fmtPercent(5), "0.05%");
  assert.equal(fmtPercent(1250), "12.5%");
  assert.equal(fmtPercent(2000), "20%");
  assert.equal(fmtPercent(10000), "100%");
});
