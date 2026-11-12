import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeApiMetrics, maximumMetricIndex, metricsQuery, metricsRangeError, rankMetricModels, requestRateSummary } from "./apiMetrics.ts";

const enhancements = await import("./apiMetrics.ts");

// 去掉 P99 解码或把缺字段补 0，都会丢失尾延迟/未知语义。
test("P99 preserves measured zero and remains unknown when absent or invalid", () => {
  const data = decodeApiMetrics(payload({ first_token_latency_ms: { sample_count: 1, missing_count: 0, avg: 0, p50: 0, p95: 0, p99: 0 }, response_latency_ms: { p99: 199 } }));
  assert.equal(data.totals.first_token_latency_ms.p99, 0);
  assert.equal(data.totals.response_latency_ms.p99, 199);
  assert.equal(data.totals.attempt_duration_ms.p99, null);
  assert.equal(decodeApiMetrics(payload({ response_latency_ms: { p99: -1 } })).totals.response_latency_ms.p99, null);
});

// 改用请求数作分母、把无样本补 0%，或吞掉未知计数都会变红。
test("sample coverage uses known plus missing attempts and keeps zero denominators unknown", () => {
  assert.equal(typeof enhancements.sampleCoverage, "function");
  assert.equal(enhancements.sampleCoverage(3, 1), 0.75);
  assert.equal(enhancements.sampleCoverage(0, 1), 0);
  assert.equal(enhancements.sampleCoverage(0, 0), null);
  assert.equal(enhancements.sampleCoverage(3, null), null);
  assert.equal(enhancements.sampleCoverage(undefined, 0), null);
  assert.equal(enhancements.sampleCoverage(9007199254740992, 0), null);
});

// 无可用用量的非空桶不能画成已测得 0；真正空桶仍保留 0。
test("token trends distinguish unavailable usage, partial records and measured zero", () => {
  assert.equal(typeof enhancements.tokenTrendValue, "function");
  const sample = (tokens: Record<string, unknown>) => decodeApiMetrics(payload({ tokens })).totals.tokens;
  assert.equal(enhancements.tokenTrendValue(sample({ input_tokens: 0, known_usage_attempts: 0, unknown_usage_attempts: 1 }), "input_tokens"), null);
  assert.equal(enhancements.tokenTrendValue(sample({ input_tokens: 0, known_usage_attempts: 0, unknown_usage_attempts: 0 }), "input_tokens"), 0);
  assert.equal(enhancements.tokenTrendValue(sample({ input_tokens: 200, completion_tokens: 30, cached_prompt_tokens: 80, known_usage_attempts: 2, unknown_usage_attempts: 1 }), "input_tokens"), 200);
  assert.equal(enhancements.tokenTrendValue(sample({ input_tokens: 200, completion_tokens: 30, known_usage_attempts: 2, unknown_usage_attempts: 0 }), "completion_tokens"), 30);
  assert.equal(enhancements.tokenTrendValue(sample({ input_tokens: 0 }), "input_tokens"), null);
});

// 构成使用全部最终请求，不能拿成功率的已知结果分母，也不能由差值虚构未知。
test("outcome composition includes unknown requests and rejects incomplete counts", () => {
  assert.equal(typeof enhancements.requestOutcomeShares, "function");
  const sample = (totals: Record<string, unknown>) => decodeApiMetrics(payload(totals)).totals;
  assert.deepEqual(enhancements.requestOutcomeShares(sample({ requests: 4, succeeded_requests: 2, failed_requests: 1, unknown_outcome_requests: 1 })), [0.5, 0.25, 0.25]);
  assert.deepEqual(enhancements.requestOutcomeShares(sample({ requests: 0, succeeded_requests: 0, failed_requests: 0, unknown_outcome_requests: 0 })), [0, 0, 0]);
  assert.equal(enhancements.requestOutcomeShares(sample({ requests: 4, succeeded_requests: 2, failed_requests: 1 })), null);
  assert.equal(enhancements.requestOutcomeShares(sample({ requests: 4, succeeded_requests: 2, failed_requests: 1, unknown_outcome_requests: 0 })), null);
});

const empty = { sample_count: 0, missing_count: 3, avg: null, p50: null, p95: null };
function payload(totals: Record<string, unknown> = {}): unknown {
  return { from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", bucket: "hour", totals, series: [], models: [] };
}

// 删掉安全整数门限会把已经舍入的线上计数当成真值。
test("metrics decoder preserves precise fees and distinguishes unsafe, zero and missing counts", () => {
  const data = decodeApiMetrics(payload({ requests: 0, billing_attempts: 9007199254740992, charged_micro_cny: "900719925474099312345", success_rate: null }));
  assert.equal(data.totals.requests, 0);
  assert.equal(data.totals.succeeded_requests, null);
  assert.equal(data.totals.billing_attempts, null);
  assert.equal(data.totals.charged_micro_cny, 900719925474099312345n);
  assert.equal(data.totals.success_rate, null);
  assert.equal(decodeApiMetrics(payload({ charged_micro_cny: 12 })).totals.charged_micro_cny, null);
});

test("a measured zero TTFT remains a sample while absent and nonstream values remain unknown", () => {
  const data = decodeApiMetrics(payload({ first_token_latency_ms: { sample_count: 1, missing_count: 2, avg: 0, p50: 0, p95: 0 }, response_latency_ms: empty }));
  assert.equal(data.totals.first_token_latency_ms.avg, 0);
  assert.equal(data.totals.first_token_latency_ms.sample_count, 1);
  assert.equal(data.totals.first_token_latency_ms.missing_count, 2);
  assert.equal(data.totals.response_latency_ms.avg, null);
  assert.equal(data.totals.attempt_duration_ms.avg, null);
  assert.equal(data.totals.attempt_duration_ms.sample_count, null);
});

test("provider token cache and response cache retain the backend's separate denominators", () => {
  const data = decodeApiMetrics(payload({ tokens: { prompt_tokens: 100, cached_prompt_tokens: 20, cache_read_tokens: 40, cache_creation_tokens: 60, input_tokens: 200, cache_hit_tokens: 60, cache_hit_rate: 0.3 }, response_cache: { hit_requests: 1, miss_requests: 3, bypass_requests: 100, disabled_requests: 200, unknown_requests: 0, hit_rate: 0.25 } }));
  assert.equal(data.totals.tokens.cache_hit_rate, 0.3);
  assert.equal(data.totals.tokens.input_tokens, 200);
  assert.equal(data.totals.tokens.cache_hit_tokens, 60);
  assert.equal(data.totals.response_cache.hit_rate, 0.25);
  assert.equal(data.totals.response_cache.unknown_requests, 0);
  assert.equal(decodeApiMetrics(payload({ tokens: { cache_hit_rate: 2 } })).totals.tokens.cache_hit_rate, null);
});

test("real series and full model breakdown survive decoding without invented missing buckets", () => {
  const base = payload() as Record<string, unknown>;
  const data = decodeApiMetrics({ ...base, series: [{ period_start: "2026-11-01T01:00:00Z", requests: 3 }, { period_start: "2026-11-01T03:00:00Z", requests: 1 }], models: [{ model: { id: "a", name: "A" }, charged_micro_cny: "9007199254740993", requests: 3 }, { model: { id: "b", name: "B" }, charged_micro_cny: "9007199254740994", requests: 1 }, { model: { id: null, name: "" }, charged_micro_cny: "0", requests: 0 }] });
  assert.deepEqual(data.series.map(row => row.period_start), ["2026-11-01T01:00:00Z", "2026-11-01T03:00:00Z"]);
  assert.deepEqual(rankMetricModels(data.models, "charged_micro_cny")?.map(row => row.model.id), ["b", "a", null]);
  assert.equal(data.models.length, 3);
  const unknown = decodeApiMetrics({ ...base, models: [{ model: { id: "a", name: "A" }, requests: 3 }, { model: { id: "b", name: "B" } }] });
  assert.equal(rankMetricModels(unknown.models, "requests"), null);
});

test("invalid envelopes fail instead of appearing as a healthy empty dashboard", () => {
  assert.throws(() => decodeApiMetrics(null));
  assert.throws(() => decodeApiMetrics({ from: "2026-11-01", to: "2026-11-02", bucket: "hour", totals: {}, series: null, models: [] }));
  assert.throws(() => decodeApiMetrics({ ...(payload() as object), series: [{ period_start: "not a date" }] }));
});

test("metrics range validation protects real 92-day and bucket limits without rejecting their boundary", () => {
  assert.equal(metricsRangeError("2026-01-01T00:00:00Z", "2026-04-03T00:00:00Z"), null);
  assert.match(metricsRangeError("2026-01-01T00:00:00Z", "2026-04-03T00:00:01Z") ?? "", /92/);
  assert.equal(metricsRangeError("2026-01-01T00:00:00Z", "2026-01-02T01:00:00Z", "minute"), null);
  assert.match(metricsRangeError("2026-01-01T00:00:00Z", "2026-01-02T01:01:00Z", "minute") ?? "", /1,500/);
  assert.notEqual(metricsRangeError("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"), null);
  assert.notEqual(metricsRangeError("invalid", "2026-01-01T00:00:00Z"), null);
});

test("query carries real filters with URL encoding and never adds a tenant scope implicitly", () => {
  assert.equal(metricsQuery({ from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", bucket: "hour", model_id: "a&b", api_key_id: "key", member_id: undefined }), "?from=2026-11-01T00%3A00%3A00Z&to=2026-11-02T00%3A00%3A00Z&bucket=hour&model_id=a%26b&api_key_id=key");
});

// 已知结果分母、待计价与调整必须独立，不能把未知结果压成失败或把未计价当免费。
test("known outcomes, unpriced attempts and precise adjustments remain independent", () => {
  const data = decodeApiMetrics(payload({ requests: 4, known_outcome_requests: 3, succeeded_requests: 2, failed_requests: 1, unknown_outcome_requests: 1, success_rate: 2 / 3, unpriced_attempts: 0, adjusted_micro_cny: "-900719925474099312345" }));
  assert.equal(data.totals.known_outcome_requests, 3);
  assert.equal(data.totals.success_rate, 2 / 3);
  assert.equal(data.totals.unknown_outcome_requests, 1);
  assert.equal(data.totals.unpriced_attempts, 0);
  assert.equal(data.totals.adjusted_micro_cny, -900719925474099312345n);
  assert.equal(decodeApiMetrics(payload()).totals.unpriced_attempts, null);
});

// 这两个整数转换成 number 后相等，按浮点排序会让较低费用排在前面。
test("model fee ranking distinguishes adjacent integers that collide as numbers", () => {
  const data = decodeApiMetrics({ ...(payload() as object), models: [
    { model: { id: "lower", name: "A" }, charged_micro_cny: "9007199254740992" },
    { model: { id: "higher", name: "B" }, charged_micro_cny: "9007199254740993" },
  ] });
  assert.deepEqual(rankMetricModels(data.models, "charged_micro_cny")?.map(row => row.model.id), ["higher", "lower"]);
});

// 极大负调整把两个小正值的几何同时截到0，真实最高期间仍须按原值比较。
test("peak period compares original exact values before geometry truncation", () => {
  assert.equal(maximumMetricIndex([-1000000000000n, 1n, 2n]), 2);
  assert.equal(maximumMetricIndex([null, 0, null]), 1);
  assert.equal(maximumMetricIndex([null, Number.NaN]), -1);
});

// 比率为空不能用0补齐，周期平均也不能取当前分页行数或把毫秒当秒。
test("failure and average request rates retain unknown and use the complete UTC interval", () => {
  assert.deepEqual(requestRateSummary(60, 0.75, "2026-11-01T00:00:00Z", "2026-11-01T00:02:00Z"), { failureRate: 0.25, averageRps: 0.5 });
  assert.deepEqual(requestRateSummary(0, null, "2026-11-01T00:00:00Z", "2026-11-01T00:01:00Z"), { failureRate: null, averageRps: 0 });
  assert.deepEqual(requestRateSummary(null, 1, "2026-11-01T00:00:00Z", "2026-11-01T00:01:00Z"), { failureRate: 0, averageRps: null });
  assert.equal(requestRateSummary(9007199254740992, null, "2026-11-01T00:00:00Z", "2026-11-01T00:01:00Z").averageRps, null);
  assert.equal(requestRateSummary(1, 0, "invalid", "2026-11-01T00:01:00Z").averageRps, null);
});
