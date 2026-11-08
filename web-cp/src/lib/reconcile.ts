import type { Count } from "./api";

/** What each check of the reconciliation is called on the page. */
const CHECK_LABEL: Record<string, string> = {
  balance: "余额与流水",
  usage: "扣费与计费",
  adjustments: "调整与重新计价",
  topups: "充值与审批",
  running_balance: "逐笔余额",
};

/** A check's label; a check this page does not know shows as the control plane named it. */
export function checkLabel(check: string): string {
  return CHECK_LABEL[check] ?? check;
}

/** What each reason a gateway drops usage for is called on the page. */
const DROP_LABEL: Record<string, string> = {
  sink_full: "队列已满",
  sink_closed: "队列已关闭",
  sink_disabled: "未接入",
  send_failed: "发送失败",
  retry_budget_exhausted: "重试超时",
  send_unconfirmed: "未确认送达（可能已入账）",
  oversized: "单条过大",
};

/** A drop reason's label; an unknown reason shows raw. */
export function dropLabel(reason: string): string {
  return DROP_LABEL[reason] ?? reason;
}

/**
 * A count with thousands separators, exact at any size: counts the control
 * plane saturates or the gateways report can pass 2^53, so this is `BigInt`
 * arithmetic on the digits, never a `Number` sum.
 */
export function fmtCount(n: Count): string {
  let digits: bigint;
  try {
    digits = BigInt(n);
  } catch {
    return String(n);
  }
  const text = (digits < 0n ? -digits : digits).toString().replace(/\B(?=(\d{3})+$)/g, ",");
  return digits < 0n ? `-${text}` : text;
}

/** Whether a count is above zero, exact at any size. */
export function isPositive(n: Count): boolean {
  try {
    return BigInt(n) > 0n;
  } catch {
    return false;
  }
}
