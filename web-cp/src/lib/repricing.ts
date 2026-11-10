import type { RepricingFilterInput, RepricingRun } from "./api";

/** The value a `datetime-local` input takes for `at`: its UTC wall clock to the second. */
export function utcInputValue(at: Date): string {
  return at.toISOString().slice(0, 19);
}

/**
 * The instant a `datetime-local` value names when it is read as UTC, as an
 * ISO string; null when it is not a date. The browser's time zone plays no
 * part: the form says UTC and the control plane stores UTC.
 */
export function utcInputToIso(value: string): string | null {
  const m = /^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2})(?::(\d{2}))?$/.exec(value);
  if (!m) return null;
  const iso = `${m[1]}T${m[2]}:${m[3] ?? "00"}Z`;
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) || at.toISOString().slice(0, 16) !== iso.slice(0, 16) ? null : at.toISOString();
}

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
