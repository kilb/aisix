import assert from "node:assert/strict";
import test from "node:test";
import { datasetNotes, droppedFieldText, flagText, flagsBlock, statusText, unshownParts } from "./importReview.ts";

test("每项标记都有中文说法，未知的标记原样显示", () => {
  assert.equal(flagText("bad_number"), "价格数字无法读取");
  assert.equal(flagText("unsupported_field"), "含无法计价的收费项");
  assert.equal(flagText("truncated_field"), "含分段过多无法完整换算的收费项");
  assert.equal(flagText("unsupported_variant"), "含无法表达的计价条件");
  assert.equal(flagText("time_of_day"), "分时段价格");
  assert.equal(flagText("invalid_card"), "价格不足以构成价目卡");
  // A flag a newer control plane added reaches the operator verbatim, never blank.
  assert.equal(flagText("split_by_region"), "split_by_region");
});

test("只有含无法计价收费项这一种标记不阻止确认，其余都阻止", () => {
  // An unmeterable price is dropped from a complete card: nothing billable is
  // lost, so the card can be confirmed.
  assert.equal(flagsBlock([]), false);
  assert.equal(flagsBlock(["unsupported_field"]), false);
  assert.equal(flagsBlock(["unsupported_field", "unsupported_field"]), false);
  // A truncated price would have been billed, so it blocks — beside the
  // dropped one or on its own.
  assert.equal(flagsBlock(["truncated_field"]), true);
  assert.equal(flagsBlock(["unsupported_field", "truncated_field"]), true);
  for (const flag of ["bad_number", "unsupported_variant", "time_of_day", "invalid_card"]) {
    assert.equal(flagsBlock([flag]), true, flag);
  }
  // A flag this build does not know blocks: it cannot say the flag is harmless.
  assert.equal(flagsBlock(["split_by_region"]), true);
});

test("未计入的收费项说出它是什么、数据集里叫什么，以及网关不计量", () => {
  assert.equal(
    droppedFieldText("storage_searches_kcount"),
    "文件搜索存储（数据集字段 storage_searches_kcount，网关不计量）",
  );
  // A field this build does not know is still named by the dataset's own name.
  assert.equal(
    droppedFieldText("new_metric_kcount"),
    "new_metric_kcount（数据集字段 new_metric_kcount，网关不计量）",
  );
});

test("每种状态都有中文说法，未知的状态原样显示", () => {
  assert.equal(statusText("pending"), "待处理");
  assert.equal(statusText("confirmed"), "已确认");
  assert.equal(statusText("rejected"), "已驳回");
  assert.equal(statusText("superseded"), "已取代");
  assert.equal(statusText("expired"), "expired");
});

test("数据集的备注按条目原名索引，短少之处不编造", () => {
  const notes = datasetNotes(
    JSON.stringify([
      {
        id: "cloudflare",
        price_comments: "the provider's own note, which is not an entry's",
        models: [
          { id: "@cf/cloudflare/clef", price_comments: "Input only, $0.24 per million tokens." },
          { id: "plain" },
          { id: "blank", price_comments: "" },
        ],
      },
      { id: "openai", models: [{ id: "gpt-5.4", price_comments: "Flat." }] },
      { models: [{ id: "orphan", price_comments: "no provider id" }] },
    ]),
  );
  assert.deepEqual(notes, {
    "cloudflare/@cf/cloudflare/clef": "Input only, $0.24 per million tokens.",
    "openai/gpt-5.4": "Flat.",
  });
});

test("不是数据集的内容读不出备注，也不抛出", () => {
  for (const body of ["", "not json", "{}", "[]", "[1]", '[{"id":1,"models":[]}]', '[{"id":"a","models":{}}]']) {
    assert.deepEqual(datasetNotes(body), {}, body);
  }
});

test("价格对照能展示的卡没有隐藏部分", () => {
  // Every compared dimension, in the default tier, with no band.
  assert.deepEqual(
    unshownParts({
      default: {
        base: { input: "1", output: "2", cache_read: "0.1", cache_write: "0.25", cache_write_1h: "0.5", web_search: "10" },
      },
    }),
    [],
  );
  // No card at all is nothing to hide: the flags say why there is none.
  assert.deepEqual(unshownParts(null), []);
});

test("价格对照展示不了的档位、分段与价格项都说出来", () => {
  assert.deepEqual(unshownParts({ default: { base: { input: "1", output: "2" }, long_context: [{ above_input_tokens: 200000 }] } }), [
    "长上下文分段价",
  ]);
  assert.deepEqual(unshownParts({ default: { base: { input: "1", output: "2" } }, batch: { base: { input: "0.5", output: "1" } } }), [
    "档位「批量」的价格",
  ]);
  // A rate the comparison never renders — audio here — is named, so the operator
  // knows which part of the card is off screen.
  assert.deepEqual(unshownParts({ default: { base: { input: "0.06", output: "0.24", audio_input: "3.4", audio_output: "13.6" } } }), [
    "价格项「音频输入」",
    "价格项「音频输出」",
  ]);
  // A key this build cannot name is still reported, never skipped.
  assert.deepEqual(unshownParts({ default: { base: { input: "1", output: "2", video_input: "9" } } }), ["价格项「video_input」"]);
});
