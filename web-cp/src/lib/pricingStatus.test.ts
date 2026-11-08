import assert from "node:assert/strict";
import { test } from "node:test";
import type { PricingStatus } from "./api.ts";
import { reasonLabel, statusView } from "./pricingStatus.ts";

const status: PricingStatus = {
  pricing_version: 7,
  invalid_revisions: [
    { table: "sell", revision_id: 11, reason: "bad rate" },
    { table: "cost", revision_id: "abc", reason: "overlap" },
  ],
  unpriced_last_24h: { total: 5, by_code: { missing_price: 3, unknown: 2 } },
};

test("two invalid revisions give two rows and a card count of 2", () => {
  const v = statusView(status);
  assert.equal(v.invalid.length, 2);
  assert.deepEqual(
    v.invalid.map((r) => [r.table, r.revision, r.reason]),
    [
      ["售价", "11", "bad rate"],
      ["成本", "abc", "overlap"],
    ],
  );
  assert.equal(new Set(v.invalid.map((r) => r.key)).size, 2);
});

test("unpriced counts keep their code's label and total", () => {
  const v = statusView(status);
  assert.equal(v.unpricedTotal, 5);
  assert.deepEqual(
    v.unpriced.map((u) => [u.label, u.count]),
    [
      ["未定价", 3],
      ["未记录原因", 2],
    ],
  );
});

test("every code the backend stores has a label; a stranger shows raw", () => {
  const labels: [string, string][] = [
    ["missing_event_id", "缺少事件编号"],
    ["unknown_tenant", "未知租户"],
    ["unknown_model", "未知模型"],
    ["unknown", "未记录原因"],
  ];
  for (const [code, label] of labels) assert.equal(reasonLabel(code), label);
  assert.equal(reasonLabel("brand_new"), "brand_new");
});
