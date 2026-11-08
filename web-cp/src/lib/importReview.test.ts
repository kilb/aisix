import assert from "node:assert/strict";
import test from "node:test";
import { datasetNotes, flagText, statusText } from "./importReview.ts";

test("每项标记都有中文说法，未知的标记原样显示", () => {
  assert.equal(flagText("bad_number"), "价格数字无法读取");
  assert.equal(flagText("unsupported_field"), "含无法计价的收费项");
  assert.equal(flagText("unsupported_variant"), "含无法表达的计价条件");
  assert.equal(flagText("time_of_day"), "分时段价格");
  assert.equal(flagText("invalid_card"), "价格不足以构成价目卡");
  // A flag a newer control plane added reaches the operator verbatim, never blank.
  assert.equal(flagText("split_by_region"), "split_by_region");
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
