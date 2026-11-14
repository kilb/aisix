/** 能力对照 v1.1 的线协议；统计结论只由服务端提供。 */
export type AssessmentMode = "quick" | "full";
export type AssessmentState = "planned" | "queued" | "running" | "completed" | "cancelled" | "interrupted";
export type AssessmentVerdict = "weaker_on_suite" | "no_directional_evidence" | "insufficient";
export type AssessmentDimension = "reasoning" | "code" | "context" | "tool" | "protocol";
export interface AssessmentTargetInput {
  model_id: string;
  model_version: string;
  provider_key_version: string;
}
export interface AssessmentTarget extends AssessmentTargetInput {
  model_name: string;
  account_id: string;
  account_name: string;
}
export interface AssessmentTargetSnapshot extends AssessmentTarget {
  provider: string;
  adapter: string;
  configured_model: string;
  endpoint_display: string;
}
export interface AssessmentSource {
  kind: "direct_provider" | "verified_channel";
  note: string;
  verified_at: string;
  independence_confirmed: true;
}
export interface AssessmentPlanRequest {
  reference: AssessmentTargetInput;
  candidate: AssessmentTargetInput;
  reference_source: AssessmentSource;
  mode: AssessmentMode;
  suite_id: "capability-v1";
  suite_version: "1";
}
export interface AssessmentDimensionReport {
  dimension: AssessmentDimension;
  planned_clusters: number;
  valid_clusters: number;
  valid_pairs: number;
  paired_coverage: number;
  reference_score: number | null;
  candidate_score: number | null;
  delta: number | null;
  reference_wins: number;
  candidate_wins: number;
  ties: number;
  p_one_sided: number | null;
  p_threshold: number;
  verdict: AssessmentVerdict;
  reason_codes: string[];
}
export interface AssessmentRunSummary {
  id: string;
  state: AssessmentState;
  mode: AssessmentMode;
  reference: AssessmentTargetSnapshot;
  candidate: AssessmentTargetSnapshot;
  created_at: string;
  started_at: string | null;
  finished_at: string | null;
  reserved_requests: number;
  sent_requests: number;
  unknown_requests: number;
  finished_requests: number;
  planned_requests: number;
  verdict: AssessmentVerdict;
}
export interface AssessmentRun extends AssessmentRunSummary {
  schema_version: 1;
  actor_id: string;
  expires_at: string;
  suite: { id: string; version: string; manifest_hash: string; generator_version: string; grader_version: string; seed: string };
  reference_source: AssessmentSource;
  limits: {
    max_requests: number;
    max_output_tokens_per_request: number;
    max_input_bytes_per_request: number;
    max_response_bytes_per_request: number;
    request_timeout_seconds: number;
    run_timeout_seconds: number;
    concurrency: 2;
    per_account_concurrency: 1;
  };
  estimated_cost: { currency: string; amount_decimal: string; price_revision: string } | null;
  cost_status: "estimated" | "unknown";
  cancel_requested_at: string | null;
  completion_reason: "all_scheduled" | "cancelled" | "deadline" | "budget" | "configuration_changed" | "owner_lost" | null;
  reason_codes: string[];
  dimensions: AssessmentDimensionReport[];
  warnings: string[];
}
export interface AssessmentSample {
  id: string;
  ordinal: number;
  pair_id: string;
  cluster_id: string;
  dimension: AssessmentDimension;
  role: "reference" | "candidate";
  request_semantic_hash: string;
  task_hash: string;
  input_bytes: number;
  dispatch_state: "not_sent" | "reserved" | "sent" | "finished" | "cancelled_before_send" | "aborted_after_send" | "interrupted_unknown";
  result: "pass" | "fail" | "error" | "unsupported" | "not_run";
  error_kind: "auth" | "rate_limit" | "upstream_http" | "timeout" | "transport" | "decode" | "truncated" | "configuration_changed" | "cancelled" | null;
  owner_generation: string | null;
  dispatch_deadline_at: string | null;
  sent_at: string | null;
  finished_at: string | null;
  http_status: number | null;
  finish_reason: string | null;
  score: 0 | 1 | null;
  usage: { input_tokens: number | null; output_tokens: number | null; upstream_total_tokens: number | null; cached_prompt_tokens: number | null; cache_read_tokens: number | null; cache_creation_tokens: number | null; reasoning_tokens: number | null };
  duration_ms: number | null;
  output_bytes: number | null;
  evidence: { checks: { name: string; passed: boolean }[]; expected_hash: string; answer_preview: string | null; answer_scrubbed: boolean };
  effective_request: { native_protocol: string; adapter_build: string; stream: boolean; output_cap: number; sampling_policy: "upstream_defaults"; semantic_configuration_hash: string };
  error: string | null;
}
export interface AssessmentExport {
  schema_version: 1;
  interface_version: "1.1";
  snapshot_at: string;
  partial: boolean;
  run: AssessmentRun;
  tasks: { pair_id: string; cluster_id: string; task_hash: string; normalized_task: unknown; expected: unknown; grader_config: { id: string; version: string; build: string; config: unknown } }[];
  samples: (AssessmentSample & { normalized_answer: { text: string; tool_calls: { name: string; arguments: unknown }[] } | null })[];
}

interface ModelResource {
  id: string;
  version: number;
  doc: { display_name?: string; provider_key_id?: string; routing?: unknown; ensemble?: unknown; semantic?: unknown };
}
interface AccountResource { id: string; version: number; doc: { display_name?: string } }

/** 仅提取名称和精确 revision，不复制账号秘密或配置 URL。 */
export function assessmentTargets(models: readonly ModelResource[], accounts: readonly AccountResource[]): AssessmentTarget[] {
  const accountMap = new Map(accounts.map((account) => [account.id, account]));
  return models.flatMap(({ id, version, doc }) => {
    const account = accountMap.get(doc.provider_key_id ?? "");
    if (doc.routing != null || doc.ensemble != null || doc.semantic != null || !account) return [];
    return [{ model_id: id, model_version: String(version), provider_key_version: String(account.version), model_name: doc.display_name || id, account_id: account.id, account_name: account.doc.display_name || account.id }];
  });
}

export function assessmentSelectionError(reference: AssessmentTarget | undefined, candidate: AssessmentTarget | undefined, independent: boolean): string | null {
  if (!reference || !candidate) return "请选择已保存的参考与被测模型。";
  if (reference.model_id === candidate.model_id) return "参考与被测必须选择不同模型。";
  if (reference.account_id === candidate.account_id) return "参考与被测必须使用不同账号。";
  if (!independent) return "请审定参考来源并确认供应渠道独立性。";
  return null;
}

export function assessmentCost(run: Pick<AssessmentRun, "estimated_cost" | "cost_status">): string {
  return run.cost_status === "unknown" || run.estimated_cost === null ? "费用未知" : `${run.estimated_cost.currency} ${run.estimated_cost.amount_decimal}（估算）`;
}
export const ASSESSMENT_DIMENSIONS: Record<AssessmentDimension, string> = { reasoning: "推理", code: "代码阅读", context: "上下文", tool: "工具选择与参数", protocol: "协议约束" };
export const ASSESSMENT_STATES: Record<AssessmentState, string> = { planned: "待确认开始", queued: "已排队", running: "运行中", completed: "已完成", cancelled: "已取消", interrupted: "已中断" };
export function assessmentVerdict(verdict: AssessmentVerdict): string {
  return { weaker_on_suite: "本题集中更多独立题组较弱", no_directional_evidence: "未获得充分方向性证据", insufficient: "证据不足" }[verdict];
}
export function assessmentPercent(value: number | null): string {
  return value === null ? "—" : `${Number((value * 100).toFixed(1))}%`;
}
export function assessmentDelta(value: number | null): string {
  return value === null ? "—" : `${value < 0 ? "−" : value > 0 ? "+" : ""}${Number((Math.abs(value) * 100).toFixed(1))} 个百分点`;
}
export function isAssessmentActive(state: AssessmentState): boolean { return state === "queued" || state === "running"; }
export function canCancelAssessment(state: AssessmentState): boolean { return state === "planned" || isAssessmentActive(state); }

/** getRandomValues 也适用于 HTTP 内网控制台，不依赖仅安全上下文提供的 randomUUID。 */
export function assessmentRequestId(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6]! & 15) | 64;
  bytes[8] = (bytes[8]! & 63) | 128;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
