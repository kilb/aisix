import assert from "node:assert/strict";
import test from "node:test";
import type { SyncItem } from "./api.ts";
import {
  byState,
  cardLines,
  longContextChange,
  longContextLines,
  longContextText,
  syncRequest,
  syncStateText,
} from "./library.ts";
import type { RateCard, SyncCandidate } from "./api.ts";

test("价格卡的联网搜索按每千次列出，token 价格不带单位", () => {
  assert.deepEqual(
    cardLines({
      default: { base: { cache_read: "1", input: "10", output: "50", web_search: "10000" } },
      batch: { base: { input: "5", output: "25", web_search: "10000" } },
    }),
    ["标准：缓存读 $1 · 输入 $10 · 输出 $50 · 联网搜索 $10 / 千次", "批量：输入 $5 · 输出 $25 · 联网搜索 $10 / 千次"],
  );
});

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
            dropped: [],
            pricing_url: null,
            prices_checked: null,
          },
    candidates: [],
    binding: null,
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

test("长上下文分段按档位与门槛逐条列出，写成一行给候选标签用", () => {
  const card: RateCard = {
    batch: { base: { input: "1", output: "2" } },
    default: {
      base: { input: "3", output: "15" },
      long_context: [
        { above_input_tokens: 200000, rates: { input: "6", output: "22.5" } },
        { above_input_tokens: 272000, rates: { input: "8", output: "30" } },
      ],
    },
  };
  const lines = longContextLines(card);
  assert.deepEqual(
    lines.map((l) => `${l.tier}@${l.above_input_tokens}:${l.input}/${l.output}`),
    ["default@200000:6/22.5", "default@272000:8/30"],
  );
  assert.deepEqual(
    lines.map(longContextText),
    // The default tier is unnamed, as every card line leaves it.
    [">200K 输入：$6 / $22.5 每百万", ">272K 输入：$8 / $30 每百万"],
  );
  // A band of another tier is its own line, named by its tier.
  assert.deepEqual(longContextLines(card).filter((l) => l.tier === "batch"), []);
  assert.equal(
    longContextText({ tier: "batch", above_input_tokens: 100000, input: "0.5", output: "1" }),
    "批量 >100K 输入：$0.5 / $1 每百万",
  );
  // No card, or one with no band: nothing to mark.
  assert.deepEqual(longContextLines(null), []);
  assert.deepEqual(longContextLines({ default: { base: { input: "1", output: "2" } } }), []);
});

test("基准价缺长上下文档，或价格库条目没有它时，预览说出同步会怎么改", () => {
  const withBand: RateCard = {
    default: { base: { input: "3", output: "15" }, long_context: [{ above_input_tokens: 200000, rates: { input: "6", output: "22.5" } }] },
  };
  const without: RateCard = { default: { base: { input: "3", output: "15" } } };
  assert.equal(longContextChange(withBand, null), "当前基准价缺少长上下文档，同步后将补上");
  assert.equal(longContextChange(withBand, without), "当前基准价缺少长上下文档，同步后将补上");
  assert.equal(longContextChange(without, withBand), "价格库条目没有长上下文档，同步将去掉它");
  // Both carry one, or neither does: nothing to say.
  assert.equal(longContextChange(withBand, withBand), null);
  assert.equal(longContextChange(without, without), null);
  assert.equal(longContextChange(null, null), null);
});

test("多匹配的模型只按选定的候选发送，没选就不发送", () => {
  const card = { default: { base: { input: "1", output: "2" } } };
  const candidate: SyncCandidate = {
    provider: "cohere",
    model: "command-r",
    name: "Command R",
    aliases: [],
    card,
    flags: [],
    dropped: [],
    pricing_url: null,
    prices_checked: null,
    input: "0.15",
    output: "0.6",
    state: "create",
  };
  const ambiguous: SyncItem = { ...item("a", "ambiguous"), candidates: [candidate] };
  assert.deepEqual(syncRequest([ambiguous], 10), []);
  assert.deepEqual(syncRequest([ambiguous], 10, { a: candidate }), [
    { model_id: "a", provider: "cohere", model: "command-r", card },
  ]);
  // A candidate that would only confirm what holds is never sent.
  const same: SyncCandidate = { ...candidate, state: "same" };
  assert.deepEqual(syncRequest([ambiguous], 10, { a: same }), []);
});
