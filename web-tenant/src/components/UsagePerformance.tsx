import { formatDuration } from "../lib/chartFormat";

export interface UsagePerformanceData {
  first_token_latency_ms?: number | null;
  attempt_duration_ms?: number | null;
  throughput_tps?: number | null;
}

const rate = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 });

/** 同一调用的三项实测指标并列，未知或不可比较的吞吐量不伪装为零。 */
export function UsagePerformance({ data }: { data: UsagePerformanceData }) {
  const tps = data.throughput_tps;
  return <dl className="usage-performance" aria-label="响应性能">
    <div title="请求开始至首个生成内容交付的实测延迟"><dt>首字</dt><dd>{formatDuration(data.first_token_latency_ms)}</dd></div>
    <div title="本次上游请求从派发至结束的总用时"><dt>总用时</dt><dd>{formatDuration(data.attempt_duration_ms)}</dd></div>
    <div title="基于上游实测每 Token 生成间隔的吞吐量；未知时不以总用时估算"><dt>TPS</dt><dd>{typeof tps === "number" && Number.isFinite(tps) && tps > 0 ? rate.format(tps) : "—"}</dd></div>
  </dl>;
}
