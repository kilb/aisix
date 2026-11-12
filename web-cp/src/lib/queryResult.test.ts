import assert from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";

// 筛选变更时旧金额、游标和错误必须在同一次投影中失效。
test("请求结果与筛选同一身份，慢响应期间不显示旧金额或游标", async () => {
 const { currentQueryResult } = await import("./queryResult.ts");
 const old = { key: "key-a", rows: [{ amount: "9007199254740993" }], next: 42, error: "old-error" };
 assert.equal(currentQueryResult(old, "key-b"), null);
 assert.equal(currentQueryResult(old, "invalid"), null);
 assert.deepEqual(currentQueryResult(old, "key-a"), old);
 const fresh = { key: "key-b", rows: [{ amount: "1" }], next: null, error: null };
 assert.deepEqual(currentQueryResult(fresh, "key-b"), fresh);
 assert.equal(currentQueryResult(fresh, "key-a"), null);
});

test("旧异步回答在新查询下不可见，返回原筛选也不能错用其它查询", async () => {
 const { currentQueryResult } = await import("./queryResult.ts");
 const pending = Promise.resolve({ key: "key-a", rows: ["old"], next: 12, error: null });
 const answer = await pending;
 assert.equal(currentQueryResult(answer, "key-b"), null);
 assert.deepEqual(currentQueryResult(answer, "key-a")?.rows, ["old"]);
});

test("调用记录消费者同步绑定筛选结果与续读游标", () => {
 const source = readFileSync(new URL("../tabs/billing/Charges.tsx", import.meta.url), "utf8");
 assert.match(source, /currentQueryResult\(result, filterKey\)/);
 assert.match(source, /if \([^\n]*!shown[^\n]*\) return;/);
 assert.doesNotMatch(source, /const \[rows, setRows\]/);
});
