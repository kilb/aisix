import assert from "node:assert/strict";
import { test } from "node:test";
import { metricViewOf, chargesQuery } from "./analysisNavigation.ts";

test("分析指标白名单不改变原筛选", () => {
  assert.equal(metricViewOf("cache"), "cache");
  assert.equal(metricViewOf("unknown"), "requests");
});

test("调用筛选只读取接口支持的字段，不伪装支持时间或权限", () => {
  assert.deepEqual(chargesQuery(new URLSearchParams("tenant_id=t&api_key_id=k&model_id=m&status=unpriced&error_code=missing_price&from=2026-01-01&role=admin")), { tenant_id: "t", api_key_id: "k", model_id: "m", status: "unpriced", error_code: "missing_price" });
  assert.deepEqual(chargesQuery(new URLSearchParams("status=invalid&limit=-1")), {});
});
