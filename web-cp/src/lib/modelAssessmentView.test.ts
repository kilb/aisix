import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { AssessmentRun } from "./modelAssessment.ts";

const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { ModelCapabilityAssessment, AssessmentRunPanel } from "./components/ModelCapabilityAssessment.tsx"; export { ModelTests } from "./tabs/ModelTests.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "assessment-view.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true })); } }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-assessment-view-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { ModelCapabilityAssessment, AssessmentRunPanel, ModelTests } = require(entry) as Record<string, ComponentType<any>>;
const target = { model_id: "model", model_name: "参考模型", model_version: "3", provider_key_version: "5", account_id: "account", account_name: "参考账号", provider: "openai", adapter: "openai", configured_model: "model", endpoint_display: "https://example.test/" };
const run: AssessmentRun = {
  schema_version: 1, id: "run", actor_id: "actor", state: "planned", mode: "quick", reference: target, candidate: { ...target, model_id: "candidate", model_name: "被测模型", account_id: "other" },
  created_at: "2026-11-14T00:00:00Z", expires_at: "2099-11-14T00:10:00Z", started_at: null, finished_at: null,
  planned_requests: 20, reserved_requests: 0, sent_requests: 0, unknown_requests: 0, finished_requests: 0, verdict: "insufficient",
  suite: { id: "capability-v1", version: "1", manifest_hash: "hash", seed: "seed", generator_version: "1", grader_version: "1" },
  reference_source: { kind: "direct_provider", note: "经独立核实", verified_at: "2026-11-13T00:00:00Z", independence_confirmed: true },
  limits: { max_requests: 20, max_output_tokens_per_request: 1024, max_input_bytes_per_request: 65536, max_response_bytes_per_request: 65536, request_timeout_seconds: 30, run_timeout_seconds: 180, concurrency: 2, per_account_concurrency: 1 },
  estimated_cost: null, cost_status: "unknown", cancel_requested_at: null, completion_reason: null, reason_codes: [], dimensions: [], warnings: [],
};
const actions = { busy: false, acceptUnknown: false, acceptLocked: false, onAcceptUnknown: () => undefined, onStart: () => undefined, onCancel: () => undefined, onRefresh: () => undefined, onExport: () => undefined };
function renderRun(value: AssessmentRun, extra = {}): string {
  assert.ok(AssessmentRunPanel);
  return renderToStaticMarkup(createElement(AssessmentRunPanel, { run: value, ...actions, ...extra }));
}
function button(markup: string, name: string): string {
  const found = (markup.match(/<button\b[^>]*>[\s\S]*?<\/button>/g) ?? []).find((part) => part.replace(/<[^>]+>/g, "").trim() === name);
  assert.ok(found, `应有操作：${name}`);
  return found;
}

// 删除显式开始、费用接受、到期或权限 gate 时，相应按钮断言必须失败。
test("初次页面只提供计划，说明20/160调用且不出现开始按钮", () => {
  assert.ok(ModelCapabilityAssessment);
  const markup = renderToStaticMarkup(createElement(ModelCapabilityAssessment, { models: [], accounts: [], loadError: false }));
  assert.match(markup, /20 次生成调用/);
  assert.match(markup, /160 次生成调用/);
  assert.match(markup, /生成计划（不调用模型）/);
  assert.doesNotMatch(markup, /开始对照/);
});

test("计划显示冻结预算，费用未知必须单独接受才能显式开始", () => {
  const markup = renderRun(run);
  assert.match(markup, /1024/);
  assert.match(markup, /65536/);
  assert.match(markup, /费用未知/);
  assert.match(button(markup, "开始对照（费用未知）"), /disabled=""/);
  assert.doesNotMatch(button(renderRun(run, { acceptUnknown: true }), "开始对照（费用未知）"), /disabled=""/);
  assert.match(button(renderRun({ ...run, expires_at: "2020-01-01T00:00:00Z" }, { acceptUnknown: true }), "开始对照（费用未知）"), /disabled=""/);
  assert.doesNotMatch(button(renderRun({ ...run, cost_status: "estimated", estimated_cost: { currency: "USD", amount_decimal: "0.000001", price_revision: "7" } }), "开始对照（按估算费用）"), /disabled=""/);
});

test("未知发送不被已知发送零掩盖，终态只保留只读操作", () => {
  const markup = renderRun({ ...run, state: "interrupted", reserved_requests: 3, sent_requests: 0, unknown_requests: 3 });
  assert.match(markup, /未知发送/);
  assert.match(markup, /潜在收费/);
  assert.doesNotMatch(markup, /开始对照|取消对照/);
  assert.match(markup, /下载可重评 JSON/);
  assert.match(markup, /不代表模型身份真伪/);
  assert.match(markup, /无方向性证据不代表等价/);
});

test("各维展示覆盖与原始得分，保留服务端证据不足与差值", () => {
  const markup = renderRun({ ...run, state: "completed", dimensions: [{ dimension: "reasoning", planned_clusters: 16, valid_clusters: 12, valid_pairs: 12, paired_coverage: 0.75, reference_score: 0.5, candidate_score: 0.25, delta: -0.25, reference_wins: 4, candidate_wins: 1, ties: 7, p_one_sided: null, p_threshold: 0.002, verdict: "insufficient", reason_codes: ["paired_coverage_below_threshold"] }] });
  assert.match(markup, /12\s*\/\s*16/);
  assert.match(markup, /75%/);
  assert.match(markup, /50%/);
  assert.match(markup, /25%/);
  assert.match(markup, /−25 个百分点/);
  assert.match(markup, /证据不足/);
  assert.match(markup, /p：—/);
});

test("viewer与operator的能力对照入口受限，旧历史导航保持可用", () => {
  assert.ok(ModelTests);
  for (const role of ["viewer", "operator"]) {
    const markup = renderToStaticMarkup(createElement(ModelTests, { operator: { role }, initialSection: "history", onGoto: () => undefined }));
    assert.match(markup, /id="model-tests-history-tab" aria-selected="true"/);
    const entry = (markup.match(/<button\b[^>]*>[\s\S]*?<\/button>/g) ?? []).find((part) => part.includes('id="model-tests-assessment-tab"'));
    assert.ok(entry);
    assert.match(entry, /disabled=""/);
    assert.doesNotMatch(markup, /生成计划（不调用模型）/);
    const deepLink = renderToStaticMarkup(createElement(ModelTests, { operator: { role }, initialSection: "assessment", onGoto: () => undefined }));
    assert.doesNotMatch(deepLink, /生成计划（不调用模型）/);
  }
});
