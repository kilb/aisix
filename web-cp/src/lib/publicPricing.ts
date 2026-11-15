import type { Group, ModelGroupPriceRow, PublicModel } from "./api.ts";

/**
 * Whether a public model can be charged for, as 价格 · 公共模型 says it.
 *
 * A key in group X is served only by the upstream accounts in group X, so a
 * call for the model is only ever priced in a group that serves it: the
 * groups of its enabled serving accounts. A group that serves nothing for the
 * model never prices a call of it, and a missing rate there is no gap. So the
 * model is priced when it has a baseline in force and every group that serves
 * it has a rate in force; a model no live group serves is judged on its
 * baseline alone, since no call can reach a group yet.
 *
 * Imports types only, so this file loads directly under `node:test`.
 */
export type PricedState =
  /** A baseline in force, and a rate in force in every group that serves it. */
  | { kind: "priced" }
  /** No baseline in force: no group can price it. */
  | { kind: "no_baseline" }
  /** A baseline, and these serving groups (display names) without a rate in force. */
  | { kind: "missing_rates"; groups: string[] };

function liveGroups(groups: readonly Group[]): Map<string, Group> {
  return new Map(groups.filter((g) => g.retired_at === null).map((g) => [g.name, g] as const));
}

function servedGroups(model: Pick<PublicModel, "served_by">, live: ReadonlyMap<string, Group>): Group[] {
  const out = new Map<string, Group>();
  for (const account of model.served_by) {
    if (!account.enabled) continue;
    for (const name of account.groups) {
      const group = live.get(name);
      if (group !== undefined) out.set(group.id, group);
    }
  }
  return [...out.values()];
}

/** The live groups that serve `model`: those its enabled accounts list, by id. */
export function servingGroups(model: Pick<PublicModel, "served_by">, groups: readonly Group[]): Group[] {
  return servedGroups(model, liveGroups(groups));
}

function stateForGroups(
  model: Pick<PublicModel, "served_by">,
  row: ModelGroupPriceRow | undefined,
  live: ReadonlyMap<string, Group>,
): PricedState {
  if (row === undefined || row.baseline.current === null) return { kind: "no_baseline" };
  const served = servedGroups(model, live);
  if (served.length === 0) return { kind: "priced" };
  const prices = new Map<string, ModelGroupPriceRow["groups"][number]>();
  for (const price of row.groups) {
    const id = price.group_id;
    if (!prices.has(id)) prices.set(id, price);
  }
  const missing: string[] = [];
  for (const group of served) {
    // The API sends `null` for a group with no rate in force; a group the
    // row does not list has none either.
    const price = prices.get(group.id);
    if (price === undefined || price.cny_per_usd === null) missing.push(price?.display_name ?? group.display_name);
  }
  return missing.length === 0 ? { kind: "priced" } : { kind: "missing_rates", groups: missing };
}

/** `model`'s state, from its row of `GET /admin/pricing/models` (absent: not priceable yet) and the live groups. */
export function pricedState(model: Pick<PublicModel, "served_by">, row: ModelGroupPriceRow | undefined, groups: readonly Group[]): PricedState {
  if (row === undefined || row.baseline.current === null) return { kind: "no_baseline" };
  return stateForGroups(model, row, liveGroups(groups));
}

/** 清单统计与可见行共用定价结果，生效分组只索引一次。 */
export function pricingStates(models: readonly Pick<PublicModel, "id" | "served_by">[], prices: ReadonlyMap<string, ModelGroupPriceRow>, groups: readonly Group[]): Map<string, PricedState> {
  const live = liveGroups(groups);
  return new Map(models.map((model) => [model.id, stateForGroups(model, prices.get(model.id), live)]));
}

/** The second line under 未定价: what is missing. */
export function unpricedReason(state: Exclude<PricedState, { kind: "priced" }>): string {
  return state.kind === "no_baseline" ? "无基准价" : `${state.groups.join("、")} 未设换算率`;
}
