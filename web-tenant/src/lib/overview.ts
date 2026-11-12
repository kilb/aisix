/** 首页聚合复用各页面的权限边界，未知角色和非启用组织不请求统计。 */
export function overviewAccess(status: string, role: string): { usage: boolean; budgets: boolean; billing: boolean } {
  const active = status === "active" && ["owner", "admin", "member", "billing"].includes(role);
  return { usage: active, budgets: active, billing: active && ["owner", "admin", "billing"].includes(role) };
}

/** 有精确计算时不退回旧数字；未知金额不能冒充零。 */
export function exactOverviewAmount(
  balance: {
    calculation?: Partial<Record<"balance_micro_cny" | "available_micro_cny" | "credit_limit_micro_cny", unknown>> | null;
    balance_micro_cny?: number;
    available_micro_cny?: number;
    credit_limit_micro_cny?: number;
  },
  field: "balance_micro_cny" | "available_micro_cny" | "credit_limit_micro_cny",
): string | null {
  if (balance.calculation !== undefined) {
    const value = balance.calculation?.[field];
    return typeof value === "string" && /^-?\d+$/.test(value) ? value : null;
  }
  const value = balance[field];
  return typeof value === "number" && Number.isSafeInteger(value) ? String(value) : null;
}

/** 每个预算各自约束同一笔消费，金额保持独立而不求和。 */
export function budgetAmounts(budget: { limit_micro_cny: number; spent_micro_cny: number; remaining_micro_cny: number }): {
  limit: bigint | null;
  spent: bigint | null;
  remaining: bigint | null;
} {
  const exact = (value: number) => Number.isSafeInteger(value) ? BigInt(value) : null;
  return { limit: exact(budget.limit_micro_cny), spent: exact(budget.spent_micro_cny), remaining: exact(budget.remaining_micro_cny) };
}
