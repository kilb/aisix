import assert from "node:assert/strict";
import { test } from "node:test";
import { usageRange, usageDateValue, metricViewOf } from "./usageNavigation.ts";

test("分享的精确窗口不因日期控件静默扩张", () => {
 const query = new URLSearchParams("from=2026-11-01T03:15:00Z&to=2026-11-02T03:15:00Z");
 assert.deepEqual(usageRange(query), {from:"2026-11-01T03:15:00Z",to:"2026-11-02T03:15:00Z"});
 assert.equal(usageDateValue(query.get("from")), "2026-11-01");
 assert.equal(usageDateValue("2026-11-01T16:00:00Z", true), "2026-11-01");
});
test("非法和反向范围停止请求，不退回默认历史", () => {
 assert.equal(usageRange(new URLSearchParams("from=bad")), null);
 assert.equal(usageRange(new URLSearchParams("from=2026-11-03&to=2026-11-01")), null);
 assert.equal(metricViewOf("future"), "requests");
 assert.equal(metricViewOf("tokens"), "tokens");
});

test("不存在的自然日拒绝进位，合法闰日仍按北京日界请求", () => {
 for (const date of ["2026-02-31", "2026-02-29", "2026-04-31", "2026-13-01"]) {
  assert.equal(usageRange(new URLSearchParams({ from: date })), null, date);
  assert.equal(usageRange(new URLSearchParams({ to: date })), null, date);
 }
 assert.deepEqual(usageRange(new URLSearchParams("from=2028-02-29&to=2028-02-29")), { from: "2028-02-28T16:00:00.000Z", to: "2028-02-29T16:00:00.000Z" });
});

test("默认范围与非法范围使用不同请求身份，恢复默认可重新读取", async () => {
 const { usageRequestKey } = await import("./usageNavigation.ts");
 const normal = usageRequestKey(new URLSearchParams(), { limit: 25 });
 const invalid = usageRequestKey(new URLSearchParams("from=bad"), { limit: 25 });
 assert.notEqual(normal, invalid);
 assert.equal(usageRequestKey(new URLSearchParams(), { limit: 25 }), normal);
 assert.notEqual(usageRequestKey(new URLSearchParams("from=other-bad"), { limit: 25 }), invalid);
});
