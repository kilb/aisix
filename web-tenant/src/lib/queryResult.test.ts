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

test("汇总和调用消费者包含原始范围身份，搜索在当前render直接生效", () => {
 const source = readFileSync(new URL("../Usage.tsx", import.meta.url), "utf8");
 assert.equal((source.match(/currentQueryResult\(result, requestKey\)/g) ?? []).length, 2);
 assert.equal((source.match(/usageRequestKey\(query,/g) ?? []).length, 2);
 assert.equal((source.match(/}, \[requestKey\]\);/g) ?? []).length, 2);
 assert.match(source, /if \([^\n]*!range[^\n]*!shown[^\n]*\) return;/);
 assert.doesNotMatch(source, /table\.setQuery\(searchQuery\)/);
 assert.equal((source.match(/filterRecords\(/g) ?? []).length, 2);
});

test("汇总范围非法时也关闭指标消费者，不向后端发送进位日期", () => {
 const source = readFileSync(new URL("../Usage.tsx", import.meta.url), "utf8");
 assert.match(source, /<ApiMetricsPanel[^>]*enabled=\{range !== null\}/);
});
