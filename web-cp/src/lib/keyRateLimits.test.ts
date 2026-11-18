import assert from "node:assert/strict";
import { test } from "node:test";
import { keyLimitText } from "./keyRateLimits.ts";

test("密钥区分生效限额、继承及收紧前的配置", () => {
  const key = { rate_limit: { rpm: 120 }, effective_rate_limit: { concurrency: 5, rpm: 60 } };
  assert.equal(keyLimitText(key, "concurrency"), "5（沿用组织上限）");
  assert.equal(keyLimitText(key, "rpm"), "60（密钥配置 120）");
  assert.equal(keyLimitText({ rate_limit: { rpm: 60 }, effective_rate_limit: { rpm: 60 } }, "rpm"), "60");
  assert.equal(keyLimitText({ rate_limit: {}, effective_rate_limit: {} }, "rpm", { key_rpm_max: 50 }), "不限");
});

test("兼容缺少生效值的响应，组织上限未知时不显示不限", () => {
  const key = { rate_limit: { concurrency: 8 } };
  assert.equal(keyLimitText(key, "concurrency", { key_concurrency_max: 5 }), "5（密钥配置 8）");
  assert.equal(keyLimitText(key, "rpm", { key_rpm_max: 60 }), "60（沿用组织上限）");
  assert.equal(keyLimitText(key, "rpm", {}), "不限");
  assert.equal(keyLimitText(key, "concurrency", {}), "8");
  assert.equal(keyLimitText(key, "rpm", null), "未知");
});
