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
