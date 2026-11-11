import assert from "node:assert/strict";
import { test } from "node:test";
import { chargeFromLines, dimensionLabel, lineUsd, rateText, reportedCounters, usageSummary } from "./chargeUsage.ts";

// `npm run test:unit`.

test("按计价引擎的公式由计费行重算售价：一次向上取整，换算率和折扣都在取整之内", () => {
  // 5 input tokens at USD 1000/M and 3 output tokens at USD 2000/M, at 1 CNY per USD: ¥0.011.
  const lines = [
    { dimension: "input", quantity: 5, rate: "1000" },
    { dimension: "output", quantity: 3, rate: "2000" },
  ];
  assert.equal(chargeFromLines(lines, 10_000, 0), 11_000);
  // The engine's pinned case: one token at USD 0.15/M × 7.2 is 1.08 µ¥, so 2 µ¥; at 1000 bps, 0.972 µ¥, so 1 µ¥.
  const one = [{ dimension: "input", quantity: 1, rate: "0.15" }];
  assert.equal(chargeFromLines(one, 72_000, 0), 2);
  assert.equal(chargeFromLines(one, 72_000, 1_000), 1);
  // A free group bills nothing; an unknown rate or an unreadable price cannot be recomputed.
  assert.equal(chargeFromLines(lines, 0, 0), 0);
  assert.equal(chargeFromLines(lines, null, 0), null);
  assert.equal(chargeFromLines([{ dimension: "input", quantity: 1, rate: "abc" }], 10_000, 0), null);
});

test("每一行的美元小计精确到小数，不经浮点", () => {
  assert.equal(lineUsd({ dimension: "input", quantity: 5, rate: "1000" }), "0.005");
  assert.equal(lineUsd({ dimension: "input", quantity: 32_622, rate: "2" }), "0.065244");
  assert.equal(lineUsd({ dimension: "input", quantity: 3, rate: "0.075" }), "0.000000225");
  assert.equal(lineUsd({ dimension: "input", quantity: 1, rate: "x" }), null);
});

test("用量摘要列出输入输出，有缓存读写和联网时才列出", () => {
  assert.equal(usageSummary({ prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 }), "输入 5 · 输出 3");
  assert.equal(
    usageSummary({ prompt_tokens: 1000, cached_prompt_tokens: 800, completion_tokens: 200 }),
    "输入 1,000 · 输出 200 · 缓存读 800",
  );
  assert.equal(
    usageSummary({ prompt_tokens: 12, cache_read_tokens: 4096, cache_creation_tokens: 2048, completion_tokens: 9, web_search_requests: 2 }),
    "输入 12 · 输出 9 · 缓存读 4,096 · 缓存写 2,048 · 联网 2 次",
  );
  assert.equal(usageSummary({}), null);
  assert.equal(usageSummary(null), null);
});

test("上游报告的计数按固定顺序列出，没报告的不列", () => {
  const rows = reportedCounters({ total_tokens: 8, completion_tokens: 3, prompt_tokens: 5 });
  assert.deepEqual(
    rows.map((r) => [r.label, r.value]),
    [
      ["输入", 5],
      ["输出", 3],
      ["上游报告的总数", 8],
    ],
  );
  assert.deepEqual(reportedCounters(undefined), []);
});

test("计费维度与换算率按页面的写法显示", () => {
  assert.equal(dimensionLabel("cache_write_1h"), "缓存写入（1 小时）");
  assert.equal(dimensionLabel("web_search"), "联网搜索（次）");
  assert.equal(dimensionLabel("novel"), "novel");
  assert.equal(rateText(72_000), "7.2");
  assert.equal(rateText(10_000), "1");
});
