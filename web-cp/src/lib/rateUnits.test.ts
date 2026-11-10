import assert from "node:assert/strict";
import { test } from "node:test";
import { compactRateText, isPerCall, usdRateText } from "./rateUnits.ts";

test("联网搜索按每千次显示，token 价格按每百万 tokens 显示", () => {
  assert.equal(isPerCall("web_search"), true);
  for (const d of ["input", "output", "cache_read", "cache_write", "cache_write_1h", "reasoning", "audio_input", "image_output"]) {
    assert.equal(isPerCall(d), false, d);
  }
  // The card's 10000 per million searches is the published $10 per 1,000.
  assert.equal(usdRateText("web_search", "10000"), "$10 / 千次");
  assert.equal(usdRateText("web_search", "3.5"), "$0.0035 / 千次");
  assert.equal(usdRateText("input", "2.5"), "$2.5 / 百万 tokens");
  assert.equal(compactRateText("web_search", "10000"), "$10 / 千次");
  assert.equal(compactRateText("output", "50"), "$50");
});
