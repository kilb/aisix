import assert from "node:assert/strict";
import test from "node:test";
import { cardDetails, matrixGroups } from "../tabs/pricing/cardDetails.ts";

test("完整价格保留十一维、未来维度、零值与超安全整数小数，且不修改原卡", () => {
  const base = {
    input: "9007199254740993.000001", output: "0", cache_read: "1", cache_write: "2", cache_write_1h: "3",
    reasoning: "4", web_search: "10000", audio_input: "5", audio_output: "6", image_input: "7", image_output: "8", future_meter: "9.000000",
  };
  const card = { default: { base } };
  const before = JSON.stringify(card);
  const rows = cardDetails(card).tiers[0]?.bands[0]?.rates ?? [];
  assert.equal(rows.length, 12);
  assert.deepEqual(rows.map((row) => row.key), Object.keys(base));
  assert.equal(rows.find((row) => row.key === "input")?.amount, "$9007199254740993.000001");
  assert.equal(rows.find((row) => row.key === "output")?.amount, "$0");
  assert.equal(rows.find((row) => row.key === "future_meter")?.label, "future_meter");
  assert.equal(rows.find((row) => row.key === "future_meter")?.amount, "$9.000000");
  assert.equal(JSON.stringify(card), before);
});

test("任意档位和多个长上下文分组完整显示，门槛不经数字转换", () => {
  const view = cardDetails({
    priority: { base: { input: "2" } },
    default: { base: { input: "1" }, long_context: [
      { above_input_tokens: "9007199254740993", rates: { input: "3", audio_output: "4" } },
      { above_input_tokens: "18446744073709551615", rates: { web_search: "3.5" } },
    ] },
    custom_2027: { base: { output: "5" } },
  });
  assert.deepEqual(view.tiers.map((tier) => tier.key), ["default", "custom_2027", "priority"]);
  assert.equal(view.tiers[0]?.bands.length, 3);
  assert.equal(view.tiers[0]?.bands[1]?.label, "长上下文 · 输入 > 9007199254740993 且 ≤ 18446744073709551615 tokens");
  assert.equal(view.tiers[0]?.bands[1]?.rates[1]?.label, "音频输出");
  assert.deepEqual(view.tiers[0]?.bands[2]?.rates[0], { key: "web_search", label: "联网搜索", amount: "$0.0035", unit: "每千次" });
});

test("联网搜索按每千次精确换算，token 保留原每百万价格字符串", () => {
  const rates = cardDetails({ default: { base: { input: "0.0000001", web_search: "9007199254740993.123456" } } }).tiers[0]?.bands[0]?.rates;
  assert.deepEqual(rates, [
    { key: "input", label: "输入", amount: "$0.0000001", unit: "每百万 tokens" },
    { key: "web_search", label: "联网搜索", amount: "$9007199254740.993123456", unit: "每千次" },
  ]);
});

test("缺少价格与形状无效都不伪装成零，错误保留具体路径", () => {
  assert.deepEqual(cardDetails(null), { tiers: [], problem: null });
  assert.deepEqual(cardDetails({}), { tiers: [], problem: null });
  const invalid = cardDetails({ default: { base: { input: 0 } } });
  assert.deepEqual(invalid.tiers, []);
  assert.match(invalid.problem ?? "", /\/default\/base\/input/);
  assert.equal(cardDetails({ default: {} }).tiers[0]?.bands[0]?.rates.length, 0);
});

test("价格矩阵按十一维分组，长上下文独有维度与未来维度都保留，缺价为未知", () => {
  const tier = cardDetails({ default: { base: { input: "0" }, long_context: [{ above_input_tokens: 200000, rates: { audio_output: "9007199254740993.1", future_dimension: "0.000001" } }] } }).tiers[0];
  assert.ok(tier);
  const groups = matrixGroups(tier);
  assert.deepEqual(groups.map((group) => group.label), ["文本与推理", "缓存", "音频", "图像", "联网", "其他计量项"]);
  const rows = groups.flatMap((group) => group.rows);
  assert.equal(rows.length, 12);
  assert.deepEqual(rows.find((row) => row.key === "input")?.amounts, ["$0", null]);
  assert.deepEqual(rows.find((row) => row.key === "audio_output")?.amounts, [null, "$9007199254740993.1"]);
  assert.deepEqual(rows.find((row) => row.key === "future_dimension")?.amounts, [null, "$0.000001"]);
  assert.equal(rows.find((row) => row.key === "web_search")?.unit, "每千次");
});

test("基础价和多段长上下文列标明互不重叠的完整输入范围", () => {
  const bands = cardDetails({ default: {
    base: { input: "1.000001" },
    long_context: [
      { above_input_tokens: 128000, rates: { input: "2.000002" } },
      { above_input_tokens: 200000, rates: { input: "3.000003" } },
    ],
  } }).tiers[0]?.bands;
  assert.deepEqual(bands?.map((band) => band.label), [
    "基础价 · 输入 ≤ 128000 tokens",
    "长上下文 · 输入 > 128000 且 ≤ 200000 tokens",
    "长上下文 · 输入 > 200000 tokens（无上界）",
  ]);
  assert.deepEqual(bands?.map((band) => band.rates[0]?.amount), ["$1.000001", "$2.000002", "$3.000003"]);
  assert.equal(cardDetails({ default: { base: { input: "0" } } }).tiers[0]?.bands[0]?.label, "基础价 · 全部输入长度");
});

test("区间上界和下界逐字保留超安全整数字符串与未来 BigInt 门槛", () => {
  const future = 184467440737095516160000000001n;
  const source = { custom_tier: { base: {}, long_context: [
    { above_input_tokens: "9007199254740993", rates: { input: "0" } },
    { above_input_tokens: future, rates: { input: "1" } },
  ] } };
  assert.deepEqual(cardDetails(source).tiers[0]?.bands.map((band) => band.label), [
    "基础价 · 输入 ≤ 9007199254740993 tokens",
    "长上下文 · 输入 > 9007199254740993 且 ≤ 184467440737095516160000000001 tokens",
    "长上下文 · 输入 > 184467440737095516160000000001 tokens（无上界）",
  ]);
  assert.equal(source.custom_tier.long_context[0]?.above_input_tokens, "9007199254740993");
  assert.equal(source.custom_tier.long_context[1]?.above_input_tokens, future);
});
