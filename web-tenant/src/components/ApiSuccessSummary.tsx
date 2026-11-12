import type { ApiMetricValues } from "../lib/apiMetrics";
import { formatCount, formatRatio } from "../lib/chartFormat";
import { Metric } from "./page";

export type SuccessSummaryTotals = Pick<ApiMetricValues, "requests" | "succeeded_requests" | "failed_requests" | "unknown_outcome_requests" | "known_outcome_requests" | "unknown_final_attempts" | "success_rate">;

/** 成功率直接使用同次全范围响应；未知结果始终另列，不补零或混入分母。 */
export function ApiSuccessSummary({ totals }: { totals?: SuccessSummaryTotals }) {
  const items = [
    { label: "成功率", value: formatRatio(totals?.success_rate) },
    { label: "成功", value: formatCount(totals?.succeeded_requests) },
    { label: "失败", value: formatCount(totals?.failed_requests) },
    { label: "未知", value: formatCount(totals?.unknown_outcome_requests) },
  ];
  return <div role="group" aria-label="请求成功率摘要" className="min-w-0">
    <dl className="m-0 grid grid-cols-2 gap-x-6 gap-y-4 desktop:grid-cols-4">
      {items.map(item => <div key={item.label} className="min-w-0"><dt className="font-mono text-micro text-muted">{item.label}</dt><dd className="m-0 mt-2"><Metric size="value">{item.value}</Metric></dd></div>)}
    </dl>
    <dl className="m-0 mt-5 flex flex-wrap gap-x-6 gap-y-2 border-t border-ink/7 pt-4 text-caption">
      <div><dt className="text-muted">已知结果样本（分母）</dt><dd className="m-0 mt-1 font-mono tabular-nums">{formatCount(totals?.known_outcome_requests)}</dd></div>
      <div><dt className="text-muted">最终请求</dt><dd className="m-0 mt-1 font-mono tabular-nums">{formatCount(totals?.requests)}</dd></div>
      <div><dt className="text-muted">最终状态未知的尝试</dt><dd className="m-0 mt-1 font-mono tabular-nums">{formatCount(totals?.unknown_final_attempts)}</dd></div>
    </dl>
    <p className="mt-3 text-caption text-muted">成功 ÷（成功 + 失败）；未知结果单列，不进入成功率分母。</p>
  </div>;
}
