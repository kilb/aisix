import { call } from "./api";
import type { AssessmentExport, AssessmentPlanRequest, AssessmentRun, AssessmentRunSummary, AssessmentSample } from "./modelAssessment";

export interface AssessmentPage<T> { items: T[]; next_cursor: string | null }
const base = "/admin/model-assessments";
const runPath = (id: string): string => `${base}/${encodeURIComponent(id)}`;

/** 计划不调用模型；所有写操作只由显式点击发起，不自动重试。 */
export async function planAssessment(body: AssessmentPlanRequest, signal?: AbortSignal): Promise<AssessmentRun> {
  return (await call<AssessmentRun>("POST", `${base}/plan`, body, undefined, signal)).data;
}
export async function startAssessment(id: string, clientRequestId: string, acceptUnknownPrice: boolean, signal?: AbortSignal): Promise<AssessmentRun> {
  return (await call<AssessmentRun>("POST", `${runPath(id)}/start`, { client_request_id: clientRequestId, accept_unknown_price: acceptUnknownPrice }, undefined, signal)).data;
}
export async function cancelAssessment(id: string, signal?: AbortSignal): Promise<AssessmentRun> {
  return (await call<AssessmentRun>("POST", `${runPath(id)}/cancel`, {}, undefined, signal)).data;
}
export async function getAssessment(id: string, signal?: AbortSignal): Promise<AssessmentRun> {
  return (await call<AssessmentRun>("GET", runPath(id), undefined, undefined, signal)).data;
}
export async function listAssessments(limit: number, before?: string, modelId?: string, signal?: AbortSignal): Promise<AssessmentPage<AssessmentRunSummary>> {
  const query = new URLSearchParams({ limit: String(limit) });
  if (before) query.set("before", before);
  if (modelId) query.set("model_id", modelId);
  return (await call<AssessmentPage<AssessmentRunSummary>>("GET", `${base}?${query}`, undefined, undefined, signal)).data;
}
export async function listAssessmentSamples(id: string, limit: number, after?: string, signal?: AbortSignal): Promise<AssessmentPage<AssessmentSample>> {
  const query = new URLSearchParams({ limit: String(limit) });
  if (after) query.set("after", after);
  return (await call<AssessmentPage<AssessmentSample>>("GET", `${runPath(id)}/samples?${query}`, undefined, undefined, signal)).data;
}
export async function exportAssessment(id: string, signal?: AbortSignal): Promise<AssessmentExport> {
  return (await call<AssessmentExport>("GET", `${runPath(id)}/export`, undefined, undefined, signal)).data;
}
