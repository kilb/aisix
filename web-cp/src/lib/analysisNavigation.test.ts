import assert from "node:assert/strict";
import { test } from "node:test";
import { metricViewOf, chargesQuery } from "./analysisNavigation.ts";

test("分析指标白名单不改变原筛选", () => {
  assert.equal(metricViewOf("cache"), "cache");
  assert.equal(metricViewOf("unknown"), "requests");
});

test("调用筛选保留服务端支持的时间条件，不扩大权限或改变分页", () => {
  assert.deepEqual(chargesQuery(new URLSearchParams("tenant_id=t&api_key_id=k&model_id=m&status=unpriced&error_code=missing_price&from=2026-01-01&role=admin")), { tenant_id: "t", api_key_id: "k", model_id: "m", status: "unpriced", error_code: "missing_price", from: "2026-01-01" });
  assert.deepEqual(chargesQuery(new URLSearchParams("status=invalid&limit=-1")), {});
});

// 任何组合条件丢失，或把分页/权限字段透传，都会改变用户可见的查询范围。
test("调用记录组合筛选保留 false 与精确时间，拒绝额外权限分页字段", () => {
  const values = { from: "2026-11-11T16:00:00.000Z", to: "2026-11-12T16:00:00.000Z", member_id: "member-a", provider_key_id: "account-b", request_id: "request-c", outcome: "failed", http_status: "503", error_class: "upstream_status", cache_hit: "false", service_tier: "fast", operation: "responses", final_attempt: "false" };
  assert.deepEqual(chargesQuery(new URLSearchParams({ ...values, role: "admin", limit: "10000", before: "99" })), values);
});

test("计价状态链接保留空档位、空模型和固定计价窗口，不丢分组及排除更正条件", () => {
  const values = { status: "unpriced", error_code: "missing_tier_price", model_id: "", pricing_tier: "", group_id: "", priced_since: "2026-11-09T10:00:00Z", priced_before: "2026-11-10T10:00:00Z", exclude_corrected: "true" };
  assert.deepEqual(chargesQuery(new URLSearchParams(values)), values);
});
