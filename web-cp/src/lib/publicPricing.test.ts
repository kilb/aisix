import assert from "node:assert/strict";
import { test } from "node:test";
import type { BaselineRevision, Group, ModelGroupPriceRow, ServedBy } from "./api.ts";
import { pricedState, pricingStates, servingGroups, unpricedReason } from "./publicPricing.ts";

// `npm run test:unit`.

const group = (id: string, name: string, display: string, retired = false) =>
  ({ id, name, display_name: display, retired_at: retired ? "2026-11-01T00:00:00Z" : null }) as Group;

const CLI = group("g-cli", "cli", "号池反代");
const DEFAULT = group("g-default", "default", "默认");
const SHARED = group("g-shared", "shared", "通用分组");
const GROUPS = [CLI, DEFAULT, SHARED];

const account = (groups: string[], enabled = true) => ({ groups, enabled }) as ServedBy;

const baseline = { revision_id: 1 } as BaselineRevision;

/** A price row with a baseline (or none) and `cny_per_usd` per group id (null: no rate). */
function row(rates: Record<string, string | null>, withBaseline = true): ModelGroupPriceRow {
  return {
    model_id: "m",
    name: "m",
    baseline: { current: withBaseline ? baseline : null, upcoming: [] },
    groups: GROUPS.map((g) => ({
      group_id: g.id,
      display_name: g.display_name,
      cny_per_usd: g.id in rates ? rates[g.id]! : null,
      source: g.id in rates && rates[g.id] !== null ? "default" : null,
    })),
  };
}

test("only the groups that serve the model are judged: the production shape is priced", () => {
  // Every account serving it is in `cli`; `default` and `shared` serve nothing for it.
  const model = { served_by: [account(["cli"]), account(["cli"])] };
  assert.deepEqual(pricedState(model, row({ "g-cli": "7.2" }), GROUPS), { kind: "priced" });
});

test("a serving group without a rate is named", () => {
  const model = { served_by: [account(["cli", "shared"])] };
  const state = pricedState(model, row({ "g-cli": "7.2" }), GROUPS);
  assert.deepEqual(state, { kind: "missing_rates", groups: ["通用分组"] });
  assert.equal(unpricedReason(state as Exclude<typeof state, { kind: "priced" }>), "通用分组 未设换算率");
});

test("no baseline is said as such, whatever the rates", () => {
  const model = { served_by: [account(["cli"])] };
  const state = pricedState(model, row({ "g-cli": "7.2" }, false), GROUPS);
  assert.deepEqual(state, { kind: "no_baseline" });
  assert.equal(unpricedReason(state as Exclude<typeof state, { kind: "priced" }>), "无基准价");
  assert.deepEqual(pricedState(model, undefined, GROUPS), { kind: "no_baseline" });
});

test("a disabled account's groups do not serve the model", () => {
  const model = { served_by: [account(["cli"]), account(["shared"], false)] };
  assert.deepEqual(pricedState(model, row({ "g-cli": "7.2" }), GROUPS), { kind: "priced" });
});

test("a model no live group serves is judged on its baseline alone", () => {
  assert.deepEqual(pricedState({ served_by: [] }, row({}), GROUPS), { kind: "priced" });
  assert.deepEqual(pricedState({ served_by: [account([])] }, row({}), GROUPS), { kind: "priced" });
  assert.deepEqual(pricedState({ served_by: [] }, row({}, false), GROUPS), { kind: "no_baseline" });
});

test("a retired or unknown routing name serves nothing", () => {
  const retired = group("g-old", "old", "旧分组", true);
  const model = { served_by: [account(["old", "nowhere", "cli"])] };
  assert.deepEqual(
    servingGroups(model, [...GROUPS, retired]).map((g) => g.id),
    ["g-cli"],
  );
  assert.deepEqual(pricedState(model, row({ "g-cli": "1" }), [...GROUPS, retired]), { kind: "priced" });
});

test("every serving group without a rate is listed once", () => {
  const model = { served_by: [account(["shared", "default"]), account(["shared"])] };
  assert.deepEqual(pricedState(model, row({}), GROUPS), { kind: "missing_rates", groups: ["通用分组", "默认"] });
});

// 恢复逐个分组 find 的实现会超过线性读取上限，不依赖机器耗时。
test("定价检查对每个分组的费率只建立一次索引", () => {
  const size = 256;
  const groups = Array.from({ length: size }, (_, i) => group(`g-${i}`, `route-${i}`, `分组 ${i}`));
  let reads = 0;
  const price: ModelGroupPriceRow = {
    ...row({}),
    groups: groups.map((g) => ({
      get group_id() { reads += 1; return g.id; },
      display_name: g.display_name,
      cny_per_usd: Number(g.id.slice(2)) % 2 === 0 ? "7.2" : null,
      source: null,
    })),
  };
  const model = { served_by: [account(groups.map((g) => g.name).reverse()), account([groups[0]!.name])] };
  const result = pricedState(model, price, groups);
  assert.deepEqual(result, { kind: "missing_rates", groups: groups.filter((_, i) => i % 2 !== 0).reverse().map((g) => g.display_name) });
  assert.ok(reads <= size * 2, `费率标识读取 ${reads} 次，${size} 个分组应在线性范围内`);
});

test("重复费率沿用首条，缺失费率沿用当前分组名称，顺序来自可用账号", () => {
  const price = row({ "g-cli": "7.2" });
  price.groups = [
    { group_id: "g-shared", display_name: "价格行名称", cny_per_usd: null, source: null },
    { group_id: "g-shared", display_name: "后续名称", cny_per_usd: "9", source: "default" },
    ...price.groups.filter((g) => g.group_id === "g-cli"),
  ];
  const model = { served_by: [account(["cli", "shared", "default", "shared"]), account(["default", "shared"]), account(["ignored"], false)] };
  assert.deepEqual(pricedState(model, price, GROUPS), { kind: "missing_rates", groups: ["价格行名称", "默认"] });
  assert.deepEqual(pricedState({ served_by: [account(["shared"], false)] }, price, GROUPS), { kind: "priced" });
  assert.deepEqual(pricedState({ served_by: [account(["shared"])] }, row({}, false), GROUPS), { kind: "no_baseline" });
});

// 退回逐模型重建全量分组索引时，此检查会按读取次数失败。
test("公共模型清单共用一次生效分组索引，状态与逐个检查一致", () => {
  let reads = 0;
  const groups = GROUPS.map((g) => ({ ...g, get retired_at() { reads += 1; return null; } }));
  const retired = group("g-old", "old", "旧分组", true);
  const models = [
    { id: "missing", served_by: [account(["cli"])] },
    { id: "unpriced", served_by: [account(["shared"])] },
    { id: "ready", served_by: [account(["cli"]), account(["shared"], false)] },
    { id: "gap", served_by: [account(["shared", "default", "shared"]), account(["cli"])] },
    { id: "empty", served_by: [] },
    { id: "retired-group", served_by: [account(["old", "unknown"])] },
  ];
  const prices = new Map([
    ["unpriced", row({}, false)], ["ready", row({ "g-cli": "7.2" })],
    ["gap", row({ "g-cli": "7.2" })], ["empty", row({})], ["retired-group", row({})],
  ]);
  const states = pricingStates(models, prices, [...groups, retired]);
  assert.deepEqual([...states], [
    ["missing", { kind: "no_baseline" }], ["unpriced", { kind: "no_baseline" }],
    ["ready", { kind: "priced" }], ["gap", { kind: "missing_rates", groups: ["通用分组", "默认"] }],
    ["empty", { kind: "priced" }], ["retired-group", { kind: "priced" }],
  ]);
  assert.equal(reads, groups.length, "每个分组的生效状态在整个清单只读取一次");
  assert.deepEqual([...pricingStates([], prices, groups)], []);
});
