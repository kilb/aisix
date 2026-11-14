import type { MetricsBucket, MetricsLatency } from "./apiMetrics.ts";
import { ApiMetricsDecodeError, metricsRangeError } from "./apiMetrics.ts";

export interface DirectModelMetricsQuery {
  from: string;
  to: string;
  bucket: MetricsBucket;
  model_id?: string;
  provider_key_id?: string;
  tenant_id?: string;
  api_key_id?: string;
  service_tier?: string;
  outcome?: "succeeded" | "failed" | "unknown";
}

export interface DirectModelMetricValues {
  attributed_attempts: number | null;
  attempts: number | null;
  excluded_attempts: number | null;
  unmeasured_attempts: number | null;
  succeeded_attempts: number | null;
  failed_attempts: number | null;
  unknown_outcome_attempts: number | null;
  failure_rate: number | null;
  success_rate: number | null;
  upstream_ttft_ms: MetricsLatency;
  first_token_latency_ms: MetricsLatency;
  attempt_duration_ms: MetricsLatency;
  throughput_tps: MetricsLatency;
  tokens: {
    input_tokens: number | null;
    cache_hit_tokens: number | null;
    cache_hit_rate: number | null;
    known_usage_attempts: number | null;
    unknown_usage_attempts: number | null;
    cache_measured_attempts: number | null;
    cache_missing_attempts: number | null;
    cache_measured_input_tokens: number | null;
    cache_measured_hit_tokens: number | null;
  };
}

export interface DirectModelMetricRow extends DirectModelMetricValues {
  model: { id: string; name: string };
  provider_key: { id: string | null; name: string };
}

export interface DirectModelMetricPeriod extends DirectModelMetricValues { period_start: string }
export interface DirectModelMetricsResponse {
  from: string;
  to: string;
  bucket: MetricsBucket;
  totals: DirectModelMetricValues;
  models: DirectModelMetricRow[];
  series: DirectModelMetricPeriod[];
}

function object(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function count(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}
function measured(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
}
function rate(value: unknown): number | null {
  const result = measured(value);
  return result !== null && result <= 1 ? result : null;
}
function distribution(value: unknown): MetricsLatency {
  const row = object(value) ?? {};
  const samples = count(row.sample_count);
  const measure = (value: unknown) => samples === null || samples === 0 ? null : measured(value);
  return { sample_count: samples, missing_count: count(row.missing_count), avg: measure(row.avg), p50: measure(row.p50), p95: measure(row.p95), p99: measure(row.p99) };
}
function values(row: Record<string, unknown>): DirectModelMetricValues {
  const tokens = object(row.tokens) ?? {};
  return {
    attributed_attempts: count(row.attributed_attempts), attempts: count(row.attempts), excluded_attempts: count(row.excluded_attempts), unmeasured_attempts: count(row.unmeasured_attempts),
    succeeded_attempts: count(row.succeeded_attempts), failed_attempts: count(row.failed_attempts), unknown_outcome_attempts: count(row.unknown_outcome_attempts),
    failure_rate: rate(row.failure_rate), success_rate: rate(row.success_rate),
    upstream_ttft_ms: distribution(row.upstream_ttft_ms), first_token_latency_ms: distribution(row.first_token_latency_ms), attempt_duration_ms: distribution(row.attempt_duration_ms), throughput_tps: distribution(row.throughput_tps),
    tokens: { input_tokens: count(tokens.input_tokens), cache_hit_tokens: count(tokens.cache_hit_tokens), cache_hit_rate: rate(tokens.cache_hit_rate), known_usage_attempts: count(tokens.known_usage_attempts), unknown_usage_attempts: count(tokens.unknown_usage_attempts), cache_measured_attempts: count(tokens.cache_measured_attempts), cache_missing_attempts: count(tokens.cache_missing_attempts), cache_measured_input_tokens: count(tokens.cache_measured_input_tokens), cache_measured_hit_tokens: count(tokens.cache_measured_hit_tokens) },
  };
}
function timestamp(value: unknown): value is string {
  return typeof value === "string" && Number.isFinite(Date.parse(value));
}

/** 缺失遥测保持未知；不将不安全计数、无样本分位数或非法比率补成零。 */
export function decodeDirectModelMetrics(value: unknown): DirectModelMetricsResponse {
  const row = object(value);
  if (!row || !timestamp(row.from) || !timestamp(row.to) || !["minute", "hour", "day"].includes(String(row.bucket)) || !object(row.totals) || !Array.isArray(row.models) || !Array.isArray(row.series)) {
    throw new ApiMetricsDecodeError("上游监测响应不完整，请重试或升级控制面。");
  }
  const models = row.models.map(value => {
    const row = object(value);
    const model = object(row?.model);
    const account = object(row?.provider_key);
    if (!row || !model || typeof model.id !== "string" || typeof model.name !== "string" || !account || !(account.id === null || typeof account.id === "string") || typeof account.name !== "string") {
      throw new ApiMetricsDecodeError("上游监测的模型归属信息无效，请刷新。");
    }
    return { ...values(row), model: { id: model.id, name: model.name }, provider_key: { id: account.id, name: account.name } };
  });
  const series = row.series.map(value => {
    const row = object(value);
    if (!row || !timestamp(row.period_start)) throw new ApiMetricsDecodeError("上游监测的时间数据无效，请刷新。");
    return { ...values(row), period_start: row.period_start };
  });
  return { from: row.from, to: row.to, bucket: row.bucket as MetricsBucket, totals: values(object(row.totals) ?? {}), models, series };
}

export function directModelMetricsQuery(query: DirectModelMetricsQuery): string {
  const params = new URLSearchParams();
  for (const field of ["from", "to", "bucket", "model_id", "provider_key_id", "tenant_id", "api_key_id", "service_tier", "outcome"] as const) {
    const value = query[field];
    if (value !== undefined && value !== "") params.set(field, value);
  }
  return `?${params}`;
}

/** 只调用管理员监测接口；租户数据面不暴露上游账号。 */
export async function fetchDirectModelMetrics(query: DirectModelMetricsQuery): Promise<DirectModelMetricsResponse> {
  const invalid = metricsRangeError(query.from, query.to, query.bucket);
  if (invalid) throw new ApiMetricsDecodeError(invalid);
  const { call } = await import("./api");
  return decodeDirectModelMetrics((await call<unknown>("GET", `/admin/usage/direct-models${directModelMetricsQuery(query)}`)).data);
}
