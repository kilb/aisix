import assert from "node:assert/strict";
import { test } from "node:test";
import type { Charge } from "./api.ts";
import { chargeReason, nothingToBillReason } from "./chargeReason.ts";

const base: Charge = {
  seq: 1,
  event_id: "e1",
  tenant_id: "t",
  api_key_id: "k",
  user_id: "u",
  requested_model_id: "m",
  occurred_at: "2026-11-10T00:00:00Z",
  status: "nothing_to_bill",
  charge_micro_cny: 0,
  cost_micro_cny: null,
  group_id: null,
  baseline_revision_id: null,
  rate_revision_id: null,
  cny_per_usd_e4: null,
  service_tier: null,
  requested_service_tier: null,
  long_context_above: null,
  estimated: false,
  exemption: null,
  error_code: null,
  adjusted_micro_cny: null,
  model_name: "gpt-5.6",
  model_deleted: true,
  status_code: 200,
  error_class: null,
  attempt_kind: "initial",
  attempt_index: 0,
  is_final_attempt: true,
};

test("a failed upstream attempt says so, with its status and which retry it was", () => {
  assert.equal(
    nothingToBillReason({ status_code: 502, error_class: "upstream_status", attempt_kind: "retry", attempt_index: 2 }),
    "调用失败 · 上游 502 · 第 2 次重试",
  );
  assert.equal(
    nothingToBillReason({ status_code: 503, error_class: "upstream_status", attempt_kind: "fallback", attempt_index: 1 }),
    "调用失败 · 上游 503 · 第 2 次尝试（换用备选）",
  );
  assert.equal(nothingToBillReason({ status_code: null, error_class: "timeout", attempt_kind: "initial", attempt_index: 0 }), "调用失败 · 上游超时");
});

test("a request the gateway refused says so, with its status", () => {
  assert.equal(nothingToBillReason({ status_code: 404, error_class: "model_not_found", attempt_kind: "initial", attempt_index: 0 }), "请求被拒绝 · 404");
  assert.equal(nothingToBillReason({ status_code: 403, error_class: null, attempt_kind: "initial", attempt_index: 0 }), "请求被拒绝 · 403");
});

test("a caller that hung up is not a request the gateway refused", () => {
  assert.equal(nothingToBillReason({ status_code: 499, error_class: "client_disconnected", attempt_kind: "initial", attempt_index: 0 }), "调用方中断");
  assert.equal(
    nothingToBillReason({ status_code: 499, error_class: "client_disconnected", attempt_kind: "retry", attempt_index: 1 }),
    "调用方中断 · 第 1 次重试",
  );
});

test("no upstream to take the call is said so, not as an upstream's 503", () => {
  for (const cls of ["all_candidates_unavailable", "provider_unavailable"]) {
    assert.equal(nothingToBillReason({ status_code: 503, error_class: cls, attempt_kind: "initial", attempt_index: 0 }), "调用失败 · 没有可用的上游", cls);
    assert.equal(nothingToBillReason({ status_code: null, error_class: cls, attempt_kind: "initial", attempt_index: 0 }), "调用失败 · 没有可用的上游", cls);
  }
  // An upstream's own 503 still reads as the upstream's.
  assert.equal(nothingToBillReason({ status_code: 503, error_class: "upstream_status", attempt_kind: "initial", attempt_index: 0 }), "调用失败 · 上游 503");
});

test("a fault of the gateway's own is not a request it refused", () => {
  assert.equal(nothingToBillReason({ status_code: 500, error_class: "api_error", attempt_kind: "initial", attempt_index: 0 }), "网关出错 · 500");
  assert.equal(nothingToBillReason({ status_code: 500, error_class: null, attempt_kind: "initial", attempt_index: 0 }), "网关出错 · 500");
  assert.equal(nothingToBillReason({ status_code: 503, error_class: "api_error", attempt_kind: "initial", attempt_index: 0 }), "网关出错 · 503");
  assert.equal(nothingToBillReason({ status_code: null, error_class: "semantic_decision_failed", attempt_kind: "initial", attempt_index: 0 }), "网关出错");
  // A refusal stays a refusal.
  assert.equal(nothingToBillReason({ status_code: 429, error_class: "rate_limit_exceeded", attempt_kind: "initial", attempt_index: 0 }), "请求被拒绝 · 429");
});

test("a call that ended well and reported nothing has no usage", () => {
  assert.equal(nothingToBillReason({ status_code: 200, error_class: null, attempt_kind: "initial", attempt_index: 0 }), "无用量");
});

test("the 原因 column follows the status", () => {
  assert.equal(chargeReason({ ...base, status: "nothing_to_bill", status_code: 502, error_class: "upstream_status", attempt_kind: "retry", attempt_index: 1 }), "调用失败 · 上游 502 · 第 1 次重试");
  assert.equal(chargeReason({ ...base, status: "unpriced", error_code: "missing_tier_price", charge_micro_cny: null }), "缺少档位价");
  assert.equal(
    chargeReason({ ...base, status: "unpriced", error_code: "missing_tier_price", charge_micro_cny: null, adjusted_micro_cny: 680 }),
    "缺少档位价 · 已通过重计价更正",
  );
  assert.equal(chargeReason({ ...base, status: "exempt", exemption: "response_cache_hit" }), "缓存命中");
  assert.equal(chargeReason({ ...base, status: "charged", charge_micro_cny: 680 }), "—");
});
