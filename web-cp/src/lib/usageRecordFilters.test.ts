import assert from "node:assert/strict";
import { test } from "node:test";
import { EMPTY_USAGE_RECORD_FILTERS, usageRecordFilterParams, usageRecordFilterValues } from "./usageRecordFilters.ts";

// false 必须作为明确筛选保留，未知深链接不能被放宽为全部记录。
test("call filters round trip combined and unknown URL values without broadening scope", () => {
  const values = usageRecordFilterValues(new URLSearchParams("outcome=failed&cache_hit=false&service_tier=fast&operation=responses&final_attempt=false&http_status=503&error_class=upstream_status&secret=hidden"));
  assert.deepEqual(usageRecordFilterParams(values, "admin"), { outcome: "failed", http_status: "503", error_class: "upstream_status", cache_hit: "false", service_tier: "fast", operation: "responses", final_attempt: "false" });
  assert.equal(usageRecordFilterValues(new URLSearchParams("outcome=invalid")).outcome, "invalid");
});

// 租户没有原始 HTTP 状态控件，其他已公开分类仍严格传递到服务端校验。
test("tenant call filters exclude raw HTTP status while retaining public failure scope", () => {
  const values = { ...EMPTY_USAGE_RECORD_FILTERS, outcome: "failed", http_status: "503", error_class: "upstream_unavailable", final_attempt: "true" };
  assert.deepEqual(usageRecordFilterParams(values, "tenant"), { outcome: "failed", error_class: "upstream_unavailable", final_attempt: "true" });
  assert.deepEqual(usageRecordFilterParams({ ...EMPTY_USAGE_RECORD_FILTERS }, "admin"), {});
});
