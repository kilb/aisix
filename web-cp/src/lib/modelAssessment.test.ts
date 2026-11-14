import assert from "node:assert/strict";
import { test } from "node:test";
import { assessmentTargets, assessmentSelectionError, assessmentCost, assessmentVerdict, assessmentPercent, assessmentDelta, assessmentRequestId, canCancelAssessment, isAssessmentActive } from "./modelAssessment.ts";

const models = [
  { id: "reference", version: 7, doc: { display_name: "参考", provider_key_id: "a", model_name: "model" } },
  { id: "candidate", version: 8, doc: { display_name: "被测", provider_key_id: "b" } },
  { id: "same-account", version: 9, doc: { provider_key_id: "a" } },
  { id: "router", version: 1, doc: { routing: {}, provider_key_id: "a" } },
  { id: "ensemble", version: 1, doc: { ensemble: {}, provider_key_id: "a" } },
  { id: "semantic", version: 1, doc: { semantic: {}, provider_key_id: "a" } },
  { id: "missing-account", version: 1, doc: { provider_key_id: "missing" } },
];
const accounts = [{ id: "a", version: 12, doc: { display_name: "账号A" } }, { id: "b", version: 13, doc: { display_name: "账号B" } }];

// 混入动态路由、缺账号或丢 revision 都必须使测试变红。
test("能力对照只选择已保存的具体模型和精确账号 revision", () => {
  const targets = assessmentTargets(models, accounts);
  assert.deepEqual(targets.map((target) => target.model_id), ["reference", "candidate", "same-account"]);
  assert.equal(targets[0]?.model_version, "7");
  assert.equal(targets[0]?.provider_key_version, "12");
  assert.equal(targets[0]?.account_id, "a");
});

test("相同模型、相同账号及缺独立性声明不能生成计划", () => {
  const [reference, candidate, sameAccount] = assessmentTargets(models, accounts);
  assert.ok(reference && candidate && sameAccount);
  assert.match(assessmentSelectionError(undefined, candidate, true) ?? "", /选择/);
  assert.match(assessmentSelectionError(reference, reference, true) ?? "", /不同模型/);
  assert.match(assessmentSelectionError(reference, sameAccount, true) ?? "", /不同账号/);
  assert.match(assessmentSelectionError(reference, candidate, false) ?? "", /独立性/);
  assert.equal(assessmentSelectionError(reference, candidate, true), null);
});

// 未知误补零、或把精确小数转换为浮点，都会破坏这些断言。
test("费用缺失与真实零不同，已知估算保留所有小数", () => {
  assert.equal(assessmentCost({ estimated_cost: null, cost_status: "unknown" }), "费用未知");
  assert.equal(assessmentCost({ estimated_cost: { currency: "USD", amount_decimal: "0.000000000000001", price_revision: "12" }, cost_status: "estimated" }), "USD 0.000000000000001（估算）");
  assert.equal(assessmentCost({ estimated_cost: { currency: "USD", amount_decimal: "0", price_revision: "12" }, cost_status: "estimated" }), "USD 0（估算）");
});

test("UI只翻译服务端结论，不将无方向证据当等价或身份结论", () => {
  assert.equal(assessmentVerdict("insufficient"), "证据不足");
  assert.equal(assessmentVerdict("no_directional_evidence"), "未获得充分方向性证据");
  assert.equal(assessmentVerdict("weaker_on_suite"), "本题集中更多独立题组较弱");
  assert.equal(assessmentPercent(null), "—");
  assert.equal(assessmentPercent(0), "0%");
  assert.equal(assessmentDelta(null), "—");
  assert.equal(assessmentDelta(-0.1), "−10 个百分点");
  assert.equal(assessmentDelta(0.125), "+12.5 个百分点");
});

test("仅活跃运行轮询，计划也可取消，终态不能重新开始或继续", () => {
  for (const state of ["queued", "running"] as const) assert.equal(isAssessmentActive(state), true);
  for (const state of ["planned", "completed", "cancelled", "interrupted"] as const) assert.equal(isAssessmentActive(state), false);
  for (const state of ["planned", "queued", "running"] as const) assert.equal(canCancelAssessment(state), true);
  for (const state of ["completed", "cancelled", "interrupted"] as const) assert.equal(canCancelAssessment(state), false);
});

test("开始请求标识是UUID v4且每个新请求独立", () => {
  const first = assessmentRequestId();
  assert.match(first, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.notEqual(assessmentRequestId(), first);
});
