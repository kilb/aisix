import { formatDuration } from "../lib/chartFormat";

export interface UsagePerformanceData {
  first_token_latency_ms?: number | null;
  attempt_duration_ms?: number | null;
  throughput_tps?: number | null;
  usage_estimated?: boolean | null;
}

const rate = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 });

export const OUTPUT_THROUGHPUT_NOTE = "输出 Token（含推理，不含输入）÷ 完整上游耗时";
export const ESTIMATED_THROUGHPUT_NOTE = "用量含网关估算，无法区分估算发生在输入还是输出，输出 TPS 仅供参考。";

/** 时间仍为实测；已有来源标记仅让不能确认为实测的吞吐量显示为参考值。 */
export function formatThroughput(data: Pick<UsagePerformanceData, "throughput_tps" | "usage_estimated"> | null | undefined): string {
  const tps = data?.throughput_tps;
  return typeof tps === "number" && Number.isFinite(tps) && tps > 0 ? `${data?.usage_estimated === true ? "≈ " : ""}${rate.format(tps)}` : "—";
}

/** 同一调用的三项性能指标并列，未知或不可比较的吞吐量不伪装为零。 */
export function UsagePerformance({ data }: { data: UsagePerformanceData }) {
  return <dl className="usage-performance" aria-label="响应性能">
    <div title="请求开始至首个生成内容交付的实测延迟"><dt>首字</dt><dd>{formatDuration(data.first_token_latency_ms)}</dd></div>
    <div title="本次上游请求从派发至结束的总用时"><dt>总用时</dt><dd>{formatDuration(data.attempt_duration_ms)}</dd></div>
    <div title={`${OUTPUT_THROUGHPUT_NOTE}${data.usage_estimated === true ? `；${ESTIMATED_THROUGHPUT_NOTE}` : ""}`}><dt>输出 TPS</dt><dd>{formatThroughput(data)}</dd></div>
  </dl>;
}
