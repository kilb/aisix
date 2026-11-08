import assert from "node:assert/strict";
import { test } from "node:test";
import { catalogueUpdateWords } from "./baselineUpdate.ts";

const update = {
  catalogue_version: "2026-11-06",
  entry_key: "openai/gpt-5.4",
  entries: [{ effective_from: null, card: { default: { base: { input: "2.5", output: "15" } } } }],
};

test("the row names the catalogue version and the panel names its entry", () => {
  const w = catalogueUpdateWords(update);
  assert.equal(w.row, "官方目录已更新（目录 2026-11-06）");
  assert.match(w.panel, /2026-11-06/);
  assert.match(w.panel, /openai\/gpt-5\.4/);
  assert.match(w.panel, /保存一份新的官方目录基准价即可确认/);
});

test("another catalogue version reads as that version, not a fixed string", () => {
  assert.match(catalogueUpdateWords({ ...update, catalogue_version: "2027-01-01" }).row, /2027-01-01/);
});
