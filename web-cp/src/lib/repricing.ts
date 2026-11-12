import type { RepricingFilterInput, RepricingRun } from "./api";

/** What a total delta means to the tenant: positive is charged more, negative is refunded. */
export function deltaWord(micro: number): string {
  if (micro > 0) return "多收";
  if (micro < 0) return "退还";
  return "无差额";
}

/**
 * Whether a failed apply may still have been executed. Only an answer the
 * control plane itself gave as a 4xx proves nothing was done; a network
 * failure, a 5xx, an answer that is not the control plane's JSON, or any
 * other error leaves the outcome unknown.
 */
export function outcomeUnknown(e: unknown): boolean {
  // Read structurally (an `ApiError` has these two fields), so this file stays free of the client's runtime.
  const answer = e as { status?: unknown; code?: unknown } | null;
  const status = typeof answer?.status === "number" ? answer.status : 0;
  return !(status >= 400 && status < 500 && typeof answer?.code === "string" && answer.code !== "bad_response");
}

/**
 * What previewing `run` again sends: its window, every filter it was stored
 * with and its reason, so the new run selects exactly the calls the stale one
 * did, at today's prices. A filter the form cannot set (a group, from a run
 * created through the API) is carried like the others, never dropped.
 */
export function previewAgainBody(run: Pick<RepricingRun, "filter" | "reason">): RepricingFilterInput {
  const f = run.filter;
  return {
    from: f.from,
    to: f.to,
    model_id: f.model_id ?? undefined,
    tenant_id: f.tenant_id ?? undefined,
    group_id: f.group_id ?? undefined,
    status: f.status ?? undefined,
    reason: run.reason,
  };
}
