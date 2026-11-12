import assert from "node:assert/strict";
import { test } from "node:test";
import { modelPriceCatalog } from "./priceCatalog.ts";
import type { QuotedGroup, QuotedModel } from "./priceQuote.ts";

function model(id: string, name: string, fx = "7.2", multiplier = "6.21"): QuotedModel {
  return {
    model_id: id, name, input_per_million: "24.84", output_per_million: "124.2",
    cny_per_usd: fx, effective_multiplier: multiplier,
    prices_by_tier: { default: { base: [{ dimension: "input", unit: "token", price_cny_per_million: "24.84" }], long_context: [] } },
  };
}

function group(id: string, models: QuotedModel[]): QuotedGroup {
  return { id, display_name: `分组 ${id}`, description: `${id} 条件`, models };
}

test("同一模型只占一个父记录，保留每组原始精确报价与完整矩阵", () => {
  const a = model("m", "同一模型");
  const b = model("m", "同一模型", "6", "5.175");
  const catalog = modelPriceCatalog([group("a", [a]), group("b", [b])]);
  assert.equal(catalog.length, 1);
  assert.equal(catalog[0]?.model_id, "m");
  assert.deepEqual(catalog[0]?.groups.map((row) => [row.id, row.quote.cny_per_usd, row.quote.effective_multiplier]), [
    ["a", "7.2", "6.21"], ["b", "6", "5.175"],
  ]);
  assert.strictEqual(catalog[0]?.groups[0]?.quote, a);
  assert.strictEqual(catalog[0]?.groups[1]?.quote.prices_by_tier, b.prices_by_tier);
});

test("同名不同ID不合并，按模型名与ID稳定排序，筛选只保留所选分组", () => {
  const groups = [group("a", [model("b", "Zulu"), model("c", "Alpha"), model("a", "Zulu")]), group("b", [model("a", "Zulu")])];
  assert.deepEqual(modelPriceCatalog(groups).map((row) => row.model_id), ["c", "a", "b"]);
  const filtered = modelPriceCatalog(groups, "b");
  assert.deepEqual(filtered.map((row) => [row.model_id, row.groups.map((row) => row.id)]), [["a", ["b"]]]);
  assert.deepEqual(modelPriceCatalog(groups, "not-granted"), []);
  assert.deepEqual(modelPriceCatalog([]), []);
});
