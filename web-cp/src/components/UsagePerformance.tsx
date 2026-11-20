import type { UpstreamTiming } from "../lib/api";
import { formatDuration } from "../lib/chartFormat";

export interface UsagePerformanceData {
  first_token_latency_ms?: number | null;
  attempt_duration_ms?: number | null;
  error_class?: string | null;
  upstream_timing?: UpstreamTiming | null;
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

/** 仅记录发起 HTTP 的断开可解释为未等到响应；已收到响应或旧行保留通用口径。 */
export function attemptDurationLabel(errorClass: unknown, timing?: UpstreamTiming | null): string {
  const send = timing?.http_send_started_ms;
  if (errorClass !== "client_disconnected" || !timing || typeof send !== "number"
    || !Number.isInteger(send) || send < 0 || send > 4294967295) {
    return "本次上游耗时";
  }
  const onlySendObserved = Object.entries(timing).every(([key, value]) => key === "http_send_started_ms" || value == null);
  return onlySendObserved ? "尝试开始至断开" : "本次上游耗时";
}

/** 完整尝试耗时的旧零值表示未测；首内容及各阶段的实测零另行保留。 */
export function formatAttemptDuration(ms: number | null | undefined): string {
  return formatDuration(ms != null && ms > 0 ? ms : null);
}

/** 同一调用的三项性能指标并列，未知或不可比较的吞吐量不伪装为零。 */
export function UsagePerformance({ data }: { data: UsagePerformanceData }) {
  return <dl className="usage-performance" aria-label="响应性能">
    <div title="从请求到达到网关首次交付生成内容，包含读取请求体和先前尝试的时间"><dt>请求首内容</dt><dd>{formatDuration(data.first_token_latency_ms)}</dd></div>
    <div title="从本次上游尝试开始计时。等候响应时断开，则计至网关观察到断开。"><dt>{attemptDurationLabel(data.error_class, data.upstream_timing)}</dt><dd>{formatAttemptDuration(data.attempt_duration_ms)}</dd></div>
    <div title={`${OUTPUT_THROUGHPUT_NOTE}${data.usage_estimated === true ? `；${ESTIMATED_THROUGHPUT_NOTE}` : ""}`}><dt>输出 TPS</dt><dd>{formatThroughput(data)}</dd></div>
  </dl>;
}
