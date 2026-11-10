import assert from "node:assert/strict";
import { test } from "node:test";
import type { PricingStatus } from "./api.ts";
import { chargesFilterFor, handChargesFilter, modelText, reasonLabel, remedy, statusView, takeChargesFilter } from "./pricingStatus.ts";

const status: PricingStatus = {
  pricing_version: 7,
  invalid_revisions: [
    { table: "baseline", revision_id: 11, reason: "bad rate" },
    { table: "cost", revision_id: "abc", reason: "overlap" },
  ],
  unpriced_last_24h: { total: 5, by_code: { missing_price: 3, unknown: 2 } },
  unpriced_details: {
    items: [
      {
        code: "missing_tier_price",
        count: 1,
        latest_at: "2026-11-10T09:32:00Z",
        model: { id: "m-sol", name: "gpt-6.1-sol", deleted: false },
        service_tier: "auto",
        group: null,
      },
      {
        code: "missing_rate",
        count: 2,
        latest_at: "2026-11-10T09:00:00Z",
        model: { id: "m-old", name: "gpt-5.6", deleted: true },
        service_tier: null,
        group: { id: "g1", name: "vip", display_name: "VIP 分组" },
      },
      {
        code: "unknown_model",
        count: 2,
        latest_at: "2026-11-10T08:00:00Z",
        model: { id: null, name: null, deleted: false },
        service_tier: null,
        group: null,
      },
    ],
    truncated: false,
  },
  corrected_last_24h: { total: 1, by_code: { missing_tier_price: 1 } },
};

test("two invalid revisions give two rows and a card count of 2", () => {
  const v = statusView(status);
  assert.equal(v.invalid.length, 2);
  assert.deepEqual(
    v.invalid.map((r) => [r.table, r.revision, r.reason]),
    [
      ["基准价", "11", "bad rate"],
      ["成本换算率", "abc", "overlap"],
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
    ["missing_rate", "分组未设换算率"],
    ["no_group", "密钥未选分组"],
  ];
  for (const [code, label] of labels) assert.equal(reasonLabel(code), label);
  assert.equal(reasonLabel("brand_new"), "brand_new");
});

test("each unpriced kind names its model, the tier or group it is about, and what to do", () => {
  const v = statusView(status);
  assert.deepEqual(
    v.details.map((d) => [d.label, d.count, d.model, d.scope]),
    [
      ["缺少档位价", 1, "gpt-6.1-sol", "档位 auto"],
      ["分组未设换算率", 2, "gpt-5.6（已删除）", "分组 VIP 分组（vip）"],
      ["未知模型", 2, "—", "—"],
    ],
  );
  assert.equal(v.details[0]!.remedy, "该模型的基准价没有 auto 档；补上该档或确认后用重计价更正");
  assert.match(v.details[1]!.remedy, /^分组 VIP 分组（vip） 没有该模型的换算率/);
  assert.equal(new Set(v.details.map((d) => d.key)).size, 3);
  // Calls a run corrected are counted apart, never in the unpriced total.
  assert.equal(v.correctedTotal, 1);
  assert.equal(v.unpricedTotal, 5);
});

test("a kind's link opens 计费明细 on its unpriced calls of that reason and model", () => {
  const v = statusView(status);
  assert.deepEqual(v.details[0]!.filter, { status: "unpriced", error_code: "missing_tier_price", model_id: "m-sol" });
  // A call that named no model is listed by its reason alone.
  assert.deepEqual(v.details[2]!.filter, { status: "unpriced", error_code: "unknown_model" });
  assert.deepEqual(chargesFilterFor({ code: "missing_price", model: { id: "x", name: null, deleted: false } }), {
    status: "unpriced",
    error_code: "missing_price",
    model_id: "x",
  });
  // The hand-off is read once.
  handChargesFilter(v.details[0]!.filter);
  assert.deepEqual(takeChargesFilter(), v.details[0]!.filter);
  assert.equal(takeChargesFilter(), null);
});

test("every reason says what to do; a tier the upstream did not report is said so", () => {
  for (const code of Object.keys({ missing_price: 1, missing_rate: 1, no_group: 1, invalid_price: 1, missing_dimension_price: 1, unknown_api_key: 1 })) {
    assert.notEqual(remedy({ code, service_tier: null, group: null }), "未记录原因", code);
  }
  assert.equal(remedy({ code: "missing_tier_price", service_tier: "", group: null }), "该模型的基准价没有 （未报告） 档；补上该档或确认后用重计价更正");
  assert.equal(remedy({ code: "brand_new", service_tier: null, group: null }), "未记录原因");
  assert.equal(modelText(null, false), "—");
  assert.equal(modelText("m", true), "m（已删除）");
});
