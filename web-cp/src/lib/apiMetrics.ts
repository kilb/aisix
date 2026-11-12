import { exactInteger, exactRatio } from "./chartFormat.ts";

export type MetricsScope = "tenant" | "admin";
export type MetricsBucket = "minute" | "hour" | "day";
export interface ApiMetricsQuery {
  from: string;
  to: string;
  bucket?: MetricsBucket;
  model_id?: string;
  member_id?: string;
  api_key_id?: string;
  tenant_id?: string;
}

const COUNT_FIELDS = ["requests", "succeeded_requests", "failed_requests", "unknown_outcome_requests", "unknown_final_attempts", "billing_attempts", "known_outcome_requests", "unpriced_attempts"] as const;
const TOKEN_FIELDS = ["prompt_tokens", "completion_tokens", "reasoning_tokens", "cached_prompt_tokens", "cache_read_tokens", "cache_creation_tokens", "input_tokens", "cache_hit_tokens", "known_usage_attempts", "unknown_usage_attempts"] as const;
const CACHE_FIELDS = ["hit_requests", "miss_requests", "bypass_requests", "disabled_requests", "unknown_requests"] as const;

type MetricCounts = Record<typeof COUNT_FIELDS[number], number | null>;
export interface MetricsLatency {
  sample_count: number | null;
  missing_count: number | null;
  avg: number | null;
  p50: number | null;
  p95: number | null;
  p99: number | null;
}
export interface ApiMetricValues extends MetricCounts {
  success_rate: number | null;
  charged_micro_cny: bigint | null;
  adjusted_micro_cny: bigint | null;
  first_token_latency_ms: MetricsLatency;
  response_latency_ms: MetricsLatency;
  attempt_duration_ms: MetricsLatency;
  tokens: Record<typeof TOKEN_FIELDS[number], number | null> & { cache_hit_rate: number | null };
  response_cache: Record<typeof CACHE_FIELDS[number], number | null> & { hit_rate: number | null };
}
export interface ApiMetricPeriod extends ApiMetricValues { period_start: string }
export interface ApiMetricModel extends ApiMetricValues { model: { id: string | null; name: string } }
export interface ApiMetricsResponse {
  from: string;
  to: string;
  bucket: MetricsBucket;
  totals: ApiMetricValues;
  series: ApiMetricPeriod[];
  models: ApiMetricModel[];
}

export class ApiMetricsDecodeError extends Error {}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function count(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}
function measurement(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : null;
}
function rate(value: unknown): number | null {
  const result = measurement(value);
  return result !== null && result <= 1 ? result : null;
}
function latency(value: unknown): MetricsLatency {
  const row = record(value) ?? {};
  return { sample_count: count(row.sample_count), missing_count: count(row.missing_count), avg: measurement(row.avg), p50: measurement(row.p50), p95: measurement(row.p95), p99: measurement(row.p99) };
}
function values(input: Record<string, unknown>): ApiMetricValues {
  const tokens = record(input.tokens) ?? {};
  const cache = record(input.response_cache) ?? {};
  return {
    ...Object.fromEntries(COUNT_FIELDS.map(key => [key, count(input[key])])) as MetricCounts,
    success_rate: rate(input.success_rate),
    charged_micro_cny: typeof input.charged_micro_cny === "string" ? exactInteger(input.charged_micro_cny) : null,
    adjusted_micro_cny: typeof input.adjusted_micro_cny === "string" ? exactInteger(input.adjusted_micro_cny) : null,
    first_token_latency_ms: latency(input.first_token_latency_ms),
    response_latency_ms: latency(input.response_latency_ms),
    attempt_duration_ms: latency(input.attempt_duration_ms),
    tokens: { ...Object.fromEntries(TOKEN_FIELDS.map(key => [key, count(tokens[key])])) as ApiMetricValues["tokens"], cache_hit_rate: rate(tokens.cache_hit_rate) },
    response_cache: { ...Object.fromEntries(CACHE_FIELDS.map(key => [key, count(cache[key])])) as ApiMetricValues["response_cache"], hit_rate: rate(cache.hit_rate) },
  };
}
function timestamp(value: unknown): value is string {
  return typeof value === "string" && Number.isFinite(Date.parse(value));
}

/** 只验证结构边界，缺失/不安全计数不补零；金额字符串永不经 number。 */
export function decodeApiMetrics(input: unknown): ApiMetricsResponse {
  const row = record(input);
  if (!row || !timestamp(row.from) || !timestamp(row.to) || !["minute", "hour", "day"].includes(String(row.bucket)) || !record(row.totals) || !Array.isArray(row.series) || !Array.isArray(row.models)) {
    throw new ApiMetricsDecodeError("统计响应格式不完整，请重试或升级控制面。");
  }
  const series = row.series.map(input => {
    const period = record(input);
    if (!period || !timestamp(period.period_start)) throw new ApiMetricsDecodeError("统计响应的期间数据无效，请重试。");
    return { ...values(period), period_start: period.period_start };
  });
  const models = row.models.map(input => {
    const item = record(input);
    const model = record(item?.model);
    if (!item || !model || !(model.id === null || typeof model.id === "string") || typeof model.name !== "string") throw new ApiMetricsDecodeError("统计响应的模型数据无效，请重试。");
    return { ...values(item), model: { id: model.id, name: model.name } };
  });
  return { from: row.from, to: row.to, bucket: row.bucket as MetricsBucket, totals: values(record(row.totals) ?? {}), series, models };
}

/** 任何未知值均阻止全量排名；不能把未知当零再称其余模型为最高。 */
export function rankMetricModels(models: readonly ApiMetricModel[], field: "requests" | "charged_micro_cny"): ApiMetricModel[] | null {
  if (models.some(model => model[field] === null)) return null;
  return [...models].sort((left, right) => {
    const a = exactInteger(left[field]);
    const b = exactInteger(right[field]);
    return a === null || b === null || a === b ? left.model.name.localeCompare(right.model.name) : a > b ? -1 : 1;
  });
}

/** 先验证范围再发请求，长历史页仍保留其余明细，不悄悄截取最近窗口。 */
export function metricsRangeError(from: string, to: string, bucket?: MetricsBucket): string | null {
  const start = Date.parse(from);
  const end = Date.parse(to);
  if (!Number.isFinite(start) || !Number.isFinite(end) || start >= end) return "请选择有效的起止时间，结束时间须晚于开始时间。";
  if (end - start > 92 * 86_400_000) return "完整 API 指标最多查询 92 天。请缩小时间范围；下方历史明细仍按原范围显示。";
  const duration = bucket === "minute" ? 60_000 : bucket === "hour" ? 3_600_000 : bucket === "day" ? 86_400_000 : null;
  if (duration !== null && Math.ceil(end / duration) - Math.floor(start / duration) > 1500) return "当前粒度超过 1,500 个期间，请缩小范围或改用更粗粒度。";
  return null;
}

export function metricsQuery(query: ApiMetricsQuery): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) if (value !== undefined && value !== "") params.set(key, value);
  return params.size ? `?${params}` : "";
}

const APPLICATION_SCOPE: MetricsScope = "admin";

/** 动态载入既有封装，保留同源会话、错误呈现和会话失效处理。 */
export async function fetchApiMetrics(scope: MetricsScope, query: ApiMetricsQuery): Promise<ApiMetricsResponse> {
  if (scope !== APPLICATION_SCOPE) throw new ApiMetricsDecodeError("当前入口不支持此指标范围。");
  const rangeError = metricsRangeError(query.from, query.to, query.bucket);
  if (rangeError) throw new ApiMetricsDecodeError(rangeError);
  const { call } = await import("./api");
  const path = scope === "tenant" ? "/usage/metrics" : "/admin/usage/metrics";
  return decodeApiMetrics((await call<unknown>("GET", `${path}${metricsQuery(query)}`)).data);
}

/** 结论必须比较原值，几何截断后的相等位置不能决定最高期间。 */
export function maximumMetricIndex(values: readonly (number | bigint | null)[]): number {
  let best: number | bigint | null = null;
  let bestIndex = -1;
  values.forEach((value, index) => {
    if (value === null || typeof value === "number" && !Number.isFinite(value)) return;
    if (best === null || value > best) {
      best = value;
      bestIndex = index;
    }
  });
  return bestIndex;
}

/** 最终请求的统计周期平均速率，不当作瞬时吞吐；失败率沿用已知结果分母。 */
export function requestRateSummary(requests: number | null, successRate: number | null, from: string, to: string): { failureRate: number | null; averageRps: number | null } {
  const durationSeconds = (Date.parse(to) - Date.parse(from)) / 1000;
  return {
    failureRate: successRate !== null && Number.isFinite(successRate) && successRate >= 0 && successRate <= 1 ? 1 - successRate : null,
    averageRps: requests !== null && Number.isSafeInteger(requests) && requests >= 0 && Number.isFinite(durationSeconds) && durationSeconds > 0 ? requests / durationSeconds : null,
  };
}

/** 分母仅为同口径的实测与缺失样本；无样本、旧字段与非法计数保持未知。 */
export function sampleCoverage(samples: number | null | undefined, missing: number | null | undefined): number | null {
  const known = count(samples);
  const unknown = count(missing);
  return known === null || unknown === null ? null : exactRatio(BigInt(known), BigInt(known) + BigInt(unknown));
}

/** 已记录 token 可为部分用量；只有未知记录的非空桶不能冒充实测零。 */
export function tokenTrendValue(tokens: ApiMetricValues["tokens"] | undefined, field: "input_tokens" | "completion_tokens"): number | null {
  const known = count(tokens?.known_usage_attempts);
  const unknown = count(tokens?.unknown_usage_attempts);
  return known === null || unknown === null || known === 0 && unknown > 0 ? null : count(tokens?.[field]);
}

/** 请求构成按全部最终请求；计数不完整或不一致时不推算缺失部分。 */
export function requestOutcomeShares(totals?: Pick<ApiMetricValues, "requests" | "succeeded_requests" | "failed_requests" | "unknown_outcome_requests">): [number, number, number] | null {
  const requests = count(totals?.requests);
  const succeeded = count(totals?.succeeded_requests);
  const failed = count(totals?.failed_requests);
  const unknown = count(totals?.unknown_outcome_requests);
  if (requests === null || succeeded === null || failed === null || unknown === null || BigInt(succeeded) + BigInt(failed) + BigInt(unknown) !== BigInt(requests)) return null;
  return requests === 0 ? [0, 0, 0] : [succeeded / requests, failed / requests, unknown / requests];
}
