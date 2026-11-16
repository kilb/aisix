import { exactInteger } from "./chartFormat.ts";
import { metricsRangeError } from "./apiMetrics.ts";

export interface CostTotals {
  knownCostMicroCny: bigint | null;
  rowsWithCost: bigint | null;
  rowsWithoutCost: bigint | null;
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}

function count(value: unknown): bigint | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? BigInt(value) : null;
}

function add(left: bigint | null, right: bigint | null): bigint | null {
  return left === null || right === null ? null : left + right;
}

/** 只汇总正式接口的全部分组；不安全金额和缺失覆盖信息绝不补零。 */
export function decodeCostTotals(input: unknown): CostTotals {
  const items = record(input)?.items;
  if (!Array.isArray(items)) throw new Error("成本统计响应格式不完整，请重试或升级控制面。");
  let knownCostMicroCny: bigint | null = 0n;
  let rowsWithCost: bigint | null = 0n;
  let rowsWithoutCost: bigint | null = 0n;
  for (const item of items) {
    const row = record(item);
    if (!row) throw new Error("成本统计响应的分组无效，请重试。");
    knownCostMicroCny = add(knownCostMicroCny, typeof row.cost_micro_cny === "number" ? exactInteger(row.cost_micro_cny) : null);
    rowsWithCost = add(rowsWithCost, count(row.rows));
    rowsWithoutCost = add(rowsWithoutCost, count(row.rows_without_cost));
  }
  if (rowsWithCost === null || rowsWithoutCost === null || rowsWithCost === 0n && rowsWithoutCost > 0n) knownCostMicroCny = null;
  return { knownCostMicroCny, rowsWithCost, rowsWithoutCost };
}

/** 与输入聚合使用同一显式窗口；接口自身按分组返回全量，无分页。 */
export async function fetchCostTotals(from: string, to: string): Promise<CostTotals> {
  const error = metricsRangeError(from, to);
  if (error) throw new Error(error);
  const { call } = await import("./api");
  const query = new URLSearchParams({ from, to });
  return decodeCostTotals((await call<unknown>("GET", `/admin/pricing/margin?${query}`)).data);
}

export interface ProfitSummary {
  revenueMicroCny: bigint;
  adjustedMicroCny: bigint;
  knownCostMicroCny: bigint;
  revenueWithCostMicroCny: bigint;
  revenueWithoutCostMicroCny: bigint;
  rowsWithCost: bigint;
  rowsWithoutCost: bigint;
  unpricedRows: bigint;
  rows: bigint;
  profitMicroCny: bigint | null;
  marginPct: string | null;
  knownProfitMicroCny: bigint | null;
  knownMarginPct: string | null;
}

/** 利润除以同一总体的收入，以整数计算百分数并按半远离零舍入。 */
function percent(profit: bigint, revenue: bigint): string | null {
  if (revenue <= 0n) return null;
  const numerator = profit * 10_000n;
  const magnitude = ((numerator < 0n ? -numerator : numerator) * 2n + revenue) / (revenue * 2n);
  return `${numerator < 0n && magnitude > 0n ? "-" : ""}${magnitude / 100n}.${(magnitude % 100n).toString().padStart(2, "0")}`;
}

/** 金额和计数在传输时也是字符串；缺字段绝不当成零。 */
export function decodeProfitSummary(input: unknown): ProfitSummary {
  const row = record(record(input)?.totals);
  function integer(name: string, nonnegative = false): bigint {
    const value = row?.[name];
    if (typeof value !== "string" || !/^-?\d+$/.test(value)) throw new Error("本利汇算统计响应不完整，请重试或升级控制面。");
    const parsed = BigInt(value);
    if (nonnegative && parsed < 0n) throw new Error("本利汇算统计响应的覆盖计数无效，请重试。");
    return parsed;
  }
  const revenueMicroCny = integer("charged_micro_cny");
  const adjustedMicroCny = integer("adjusted_micro_cny");
  const knownCostMicroCny = integer("known_cost_micro_cny", true);
  const revenueWithCostMicroCny = integer("charged_with_cost_micro_cny");
  const rowsWithCost = integer("rows_with_cost", true);
  const rowsWithoutCost = integer("rows_without_cost", true);
  const unpricedRows = integer("unpriced_rows", true);
  const rows = integer("rows", true);
  const complete = rowsWithoutCost === 0n && unpricedRows === 0n;
  const knownProfitMicroCny = rowsWithCost > 0n ? revenueWithCostMicroCny - knownCostMicroCny : null;
  const profitMicroCny = complete ? revenueMicroCny - knownCostMicroCny : null;
  return {
    revenueMicroCny, adjustedMicroCny, knownCostMicroCny, revenueWithCostMicroCny,
    revenueWithoutCostMicroCny: revenueMicroCny - revenueWithCostMicroCny,
    rowsWithCost, rowsWithoutCost, unpricedRows, rows,
    profitMicroCny, marginPct: profitMicroCny === null ? null : percent(profitMicroCny, revenueMicroCny),
    knownProfitMicroCny, knownMarginPct: knownProfitMicroCny === null ? null : percent(knownProfitMicroCny, revenueWithCostMicroCny),
  };
}

/** 只查总额无需限制时间桶数；没有边界表示全部时间。 */
export function profitRangeError(from?: string, to?: string): string | null {
  if (from === undefined && to === undefined) return null;
  if (from === undefined || to === undefined) return "请同时给出开始和结束时间。";
  const start = Date.parse(from), end = Date.parse(to);
  if (!Number.isFinite(start) || !Number.isFinite(end)) return "请输入有效的开始和结束时间。";
  return end > start ? null : "结束时间须晚于开始时间。";
}

export async function fetchProfitSummary(from?: string, to?: string): Promise<ProfitSummary> {
  const error = profitRangeError(from, to);
  if (error) throw new Error(error);
  const { call } = await import("./api");
  const query = from === undefined || to === undefined ? "" : `?${new URLSearchParams({ from, to })}`;
  return decodeProfitSummary((await call<unknown>("GET", `/admin/billing/profit-summary${query}`)).data);
}
