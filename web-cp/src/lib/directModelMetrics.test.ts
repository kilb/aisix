import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeDirectModelMetrics, directModelMetricsQuery } from "./directModelMetrics.ts";

function payload(totals: Record<string, unknown> = {}, models: unknown[] = [], series: unknown[] = []): unknown {
  return { from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", bucket: "hour", totals, models, series };
}

// 总输入包括未上报缓存的尝试；缓存率必须展示它自身的已采样分母。
test("direct cache coverage preserves measured denominator separately from all input", () => {
  const data = decodeDirectModelMetrics(payload({ tokens: { input_tokens: 300, cache_hit_tokens: 50, cache_hit_rate: 0.25, cache_measured_attempts: 2, cache_missing_attempts: 1, cache_measured_input_tokens: 200, cache_measured_hit_tokens: 50 } }));
  assert.equal(data.totals.tokens.input_tokens, 300);
  assert.equal(data.totals.tokens.cache_hit_rate, 0.25);
  const tokens = data.totals.tokens;
  assert.equal(tokens.cache_measured_attempts, 2);
  assert.equal(tokens.cache_missing_attempts, 1);
  assert.equal(tokens.cache_measured_input_tokens, 200);
  assert.equal(tokens.cache_measured_hit_tokens, 50);
  const zero = decodeDirectModelMetrics(payload({ tokens: { cache_measured_attempts: 1, cache_missing_attempts: 0, cache_measured_input_tokens: 100, cache_measured_hit_tokens: 0, cache_hit_rate: 0 } }));
  assert.equal(zero.totals.tokens.cache_measured_hit_tokens, 0);
  for (const invalid of [undefined, null, -1, "2", 9007199254740992]) {
    const value = decodeDirectModelMetrics(payload({ tokens: { cache_measured_attempts: invalid, cache_missing_attempts: invalid, cache_measured_input_tokens: invalid, cache_measured_hit_tokens: invalid } })).totals.tokens;
    for (const field of ["cache_measured_attempts", "cache_missing_attempts", "cache_measured_input_tokens", "cache_measured_hit_tokens"] as const) assert.equal(value[field], null);
  }
});

// 缺失值不能冒充健康的零失败；删除安全整数门槛会让超限计数变红。
test("direct monitoring keeps unknown, unsafe and measured zero values distinct", () => {
  const data = decodeDirectModelMetrics(payload({ attempts: 0, failure_rate: 0, succeeded_attempts: 9007199254740992, failed_attempts: -1, unknown_outcome_attempts: "2", tokens: { input_tokens: 200, cache_hit_tokens: 105, cache_hit_rate: 0.525 } }));
  assert.equal(data.totals.attempts, 0);
  assert.equal(data.totals.failure_rate, 0);
  assert.equal(data.totals.succeeded_attempts, null);
  assert.equal(data.totals.failed_attempts, null);
  assert.equal(data.totals.unknown_outcome_attempts, null);
  assert.equal(data.totals.success_rate, null);
  assert.equal(data.totals.tokens.input_tokens, 200);
  assert.equal(data.totals.tokens.cache_hit_rate, 0.525);
  assert.equal(data.totals.tokens.known_usage_attempts, null);
  assert.equal(decodeDirectModelMetrics(payload({ failure_rate: 1.01 })).totals.failure_rate, null);
});

// 0ms 是实测；无样本或缺失不能画成有值的延迟与吞吐。
test("direct distributions preserve zero measurements and reject values without samples", () => {
  const data = decodeDirectModelMetrics(payload({ upstream_ttft_ms: { sample_count: 1, missing_count: 2, avg: 0, p50: 0, p95: 0, p99: 0 }, throughput_tps: { sample_count: 0, missing_count: 4, avg: 40, p95: 40 } }));
  assert.deepEqual(data.totals.upstream_ttft_ms, { sample_count: 1, missing_count: 2, avg: 0, p50: 0, p95: 0, p99: 0 });
  assert.equal(data.totals.throughput_tps.sample_count, 0);
  assert.equal(data.totals.throughput_tps.missing_count, 4);
  assert.equal(data.totals.throughput_tps.avg, null);
  assert.equal(data.totals.throughput_tps.p95, null);
  assert.equal(data.totals.first_token_latency_ms.p50, null);
  for (const sample_count of [undefined, null, -1, 9007199254740992, "1"]) {
    const invalid = decodeDirectModelMetrics(payload({ throughput_tps: { sample_count, avg: 50, p50: 50, p95: 60, p99: 70 } }));
    assert.equal(invalid.totals.throughput_tps.sample_count, null);
    assert.equal(invalid.totals.throughput_tps.avg, null);
    assert.equal(invalid.totals.throughput_tps.p50, null);
    assert.equal(invalid.totals.throughput_tps.p95, null);
    assert.equal(invalid.totals.throughput_tps.p99, null);
  }
});

// 实际模型与账号归属及真实时间桶要原样保留，不能以入口模型或合成桶覆盖。
test("direct monitoring preserves every attributed model and real series period", () => {
  const data = decodeDirectModelMetrics(payload({}, [
    { model: { id: "actual-a", name: "A" }, provider_key: { id: "account-a", name: "账号甲" }, attempts: 4, failed_attempts: 1, unmeasured_attempts: 2 },
    { model: { id: "idle", name: "Idle" }, provider_key: { id: null, name: "" }, attempts: 0, failure_rate: null },
  ], [{ period_start: "2026-11-01T01:00:00Z", attempts: 2 }, { period_start: "2026-11-01T03:00:00Z", attempts: 1 }]));
  assert.equal(data.models.length, 2);
  assert.equal(data.models[0]?.model.id, "actual-a");
  assert.equal(data.models[0]?.provider_key.id, "account-a");
  assert.equal(data.models[0]?.unmeasured_attempts, 2);
  assert.equal(data.models[1]?.failure_rate, null);
  assert.deepEqual(data.series.map(period => period.period_start), ["2026-11-01T01:00:00Z", "2026-11-01T03:00:00Z"]);
});

// 错误 envelope 与归属信息必须报错，不能回退到看似正常的空面板。
test("invalid direct monitoring envelopes and attributions fail explicitly", () => {
  assert.throws(() => decodeDirectModelMetrics(null));
  assert.throws(() => decodeDirectModelMetrics({ from: "bad", to: "bad", bucket: "hour", totals: {}, models: [], series: [] }));
  assert.throws(() => decodeDirectModelMetrics(payload({}, [{ model: { id: "a", name: "A" } }])));
  assert.throws(() => decodeDirectModelMetrics(payload({}, [], [{ period_start: "bad" }])));
});

// 删除白名单会带出意外字段；不编码则模型名中的 & 会改变请求范围。
test("direct monitoring query preserves its exact filter scope and URL encodes values", () => {
  const query = directModelMetricsQuery({ from: "2026-11-01T00:00:00Z", to: "2026-11-02T00:00:00Z", bucket: "hour", model_id: "m&a", provider_key_id: "account", tenant_id: "tenant", api_key_id: "key", service_tier: "fast", outcome: "failed" });
  const params = new URLSearchParams(query);
  assert.equal(params.get("model_id"), "m&a");
  assert.equal(params.get("provider_key_id"), "account");
  assert.equal(params.get("tenant_id"), "tenant");
  assert.equal(params.get("api_key_id"), "key");
  assert.equal(params.get("service_tier"), "fast");
  assert.equal(params.get("outcome"), "failed");
  assert.equal(params.size, 9);
  assert.equal(new URLSearchParams(directModelMetricsQuery({ from: "a", to: "b", bucket: "day", model_id: "" })).has("model_id"), false);
});
