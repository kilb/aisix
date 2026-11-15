import assert from "node:assert/strict";
import { test } from "node:test";
import { callDetailHash, callListHash, logValue } from "./callLog.ts";

test("调用日志深链保留服务端筛选且事件编号独立编码", () => {
  const query = new URLSearchParams("outcome=failed&tenant_id=t&before=44&request_id=request%2Fa");
  const target = callDetailHash(query, "event/a?b#c");
  const detail = new URLSearchParams(target.split("?")[1]);
  assert.equal(target.split("?")[0], "#calls");
  assert.equal(detail.get("detail"), "event/a?b#c");
  for (const [key, value] of query) assert.equal(detail.get(key), value);
  assert.equal(query.has("detail"), false);
  assert.equal(callListHash(detail), "#calls?outcome=failed&tenant_id=t&before=44&request_id=request%2Fa");
});

test("返回调用列表仅删除详情标记且支持无筛选入口", () => {
  assert.equal(callListHash(new URLSearchParams("detail=ev")), "#calls");
  assert.equal(callDetailHash(new URLSearchParams("detail=old&event_id=filter"), "new"), "#calls?detail=new&event_id=filter");
});

test("日志缺失、真实零值和布尔值各自明确显示", () => {
  assert.equal(logValue(null), "未记录");
  assert.equal(logValue(undefined), "未记录");
  assert.equal(logValue(""), "未记录");
  assert.equal(logValue(0), "0");
  assert.equal(logValue(false), "否");
  assert.equal(logValue(true), "是");
  assert.equal(logValue("具体报错"), "具体报错");
});
