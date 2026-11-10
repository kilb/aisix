import assert from "node:assert/strict";
import test from "node:test";
import type { SyncItem } from "./api.ts";
import { byState, cardLines, syncRequest, syncStateText } from "./library.ts";

test("每种同步状态都有中文说法，未知的状态原样显示", () => {
  assert.equal(syncStateText("create"), "将新增");
  assert.equal(syncStateText("update"), "将更新");
  assert.equal(syncStateText("same"), "一致");
  assert.equal(syncStateText("flagged"), "需手动录入");
  assert.equal(syncStateText("unmatched"), "无匹配");
  assert.equal(syncStateText("ambiguous"), "多个匹配");
  assert.equal(syncStateText("retired"), "retired");
  assert.equal(syncStateText("constructor"), "constructor");
});

test("价目卡的每一项都列出：标准档在前，其余档位与长上下文分段各占一行", () => {
  assert.deepEqual(cardLines(null), []);
  assert.deepEqual(
    cardLines({
      priority: { base: { input: "5", output: "20" } },
      default: {
        base: { input: "2.5", output: "10", cache_read: "1.25", storage: "3" },
        long_context: [{ above_input_tokens: 200000, rates: { input: "5", output: "15" } }],
      },
      batch: { base: { input: "1.25", output: "5" } },
    }),
    [
      "标准：输入 $2.5 · 输出 $10 · 缓存读 $1.25 · storage $3",
      "标准 · 输入超过 200000 tokens：输入 $5 · 输出 $15",
      "批量：输入 $1.25 · 输出 $5",
      "priority：输入 $5 · 输出 $20",
    ],
  );
});

function item(model_id: string, state: SyncItem["state"], card = true): SyncItem {
  return {
    model_id,
    name: model_id,
    state,
    matched_by: state === "unmatched" || state === "ambiguous" ? null : "exact",
    entry:
      state === "unmatched" || state === "ambiguous"
        ? null
        : {
            provider: "p",
            model: `${model_id}-entry`,
            name: null,
            aliases: [],
            card: card ? { default: { base: { input: "1", output: "2" } } } : null,
            flags: [],
            pricing_url: null,
            prices_checked: null,
          },
    candidates: [],
    current: null,
  };
}

test("预览按状态排列：先列会写入的，再列需要人处理的，一致的最后", () => {
  const sorted = byState([
    item("s", "same"),
    item("u", "unmatched"),
    item("c1", "create"),
    item("a", "ambiguous"),
    item("x", "update"),
    item("f", "flagged"),
    item("c2", "create"),
  ]);
  assert.deepEqual(
    sorted.map((i) => i.model_id),
    ["c1", "c2", "x", "a", "f", "u", "s"],
  );
});

test("同步只发送将新增与将更新的模型，带上所见的条目与价目卡，且不超过上限", () => {
  const items = [
    item("c1", "create"),
    item("s", "same"),
    item("a", "ambiguous"),
    item("f", "flagged"),
    item("x", "update"),
    item("c2", "create"),
  ];
  assert.deepEqual(syncRequest(items, 100), [
    { model_id: "c1", provider: "p", model: "c1-entry", card: { default: { base: { input: "1", output: "2" } } } },
    { model_id: "x", provider: "p", model: "x-entry", card: { default: { base: { input: "1", output: "2" } } } },
    { model_id: "c2", provider: "p", model: "c2-entry", card: { default: { base: { input: "1", output: "2" } } } },
  ]);
  assert.deepEqual(
    syncRequest(items, 2).map((i) => i.model_id),
    ["c1", "x"],
  );
});
