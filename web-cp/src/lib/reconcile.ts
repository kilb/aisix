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
 * What the operator reads when the check fails: plain words for an unknown
 * tenant (404) and a check that outran its time bound (503). Any other
 * failure is worded by `fallback`.
 */
export function failureText(e: unknown, fallback: (e: unknown) => string): string {
  const status = (e as { status?: unknown } | null)?.status;
  if (status === 404) return "找不到所选租户，可能已被删除。请重新选择租户。";
  if (status === 503) return "对账超时，控制面暂时无法完成这次检查。请稍后点「刷新」重试。";
  return fallback(e);
}
