import assert from "node:assert/strict";
import { test } from "node:test";
import type { BaselineRevision, Group, ModelGroupPriceRow, ServedBy } from "./api.ts";
import { pricedState, servingGroups, unpricedReason } from "./publicPricing.ts";

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
