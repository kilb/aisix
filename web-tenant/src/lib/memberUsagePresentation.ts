import { exactInteger } from "./chartFormat.ts";
import { fmtMicroCny } from "./money.ts";

interface MemberCharge {
  charged_micro_cny: string;
  pending_requests: number;
  unpriced_requests: number;
}

/** 列表只展示费用；待计价和缺失金额不能读成免费。 */
export function memberChargeLabel(usage: MemberCharge | undefined): string {
  if (!usage) return "暂无费用数据";
  const amount = exactInteger(usage.charged_micro_cny);
  if (amount === null) return "费用未知";
  const pending = usage.pending_requests > 0 || usage.unpriced_requests > 0;
  if (pending && amount === 0n) return "待计价";
  return `${fmtMicroCny(amount)}${pending ? "（部分待计价）" : ""}`;
}
