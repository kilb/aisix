/**
 * A direct credit has no idempotency key: the request may have been stored while the
 * answer was lost (dropped connection, a proxy's 5xx). The outcome of such a failure is
 * unknown, so the caller must not hand the form back to the operator to retry.
 * Only the shape of `status` is read, without importing `ApiError` itself, so this file
 * loads directly under node:test.
 */
export function isUnknownOutcome(e: unknown): boolean {
  const answer = typeof e === "object" && e !== null ? (e as { status?: unknown; code?: unknown }) : {};
  // Only the control plane's own 4xx answer proves nothing was stored. Anything else (a network failure,
  // a 5xx, a 2xx whose body was not JSON, a proxy's 4xx page) leaves it unknown whether the request landed.
  const status = typeof answer.status === "number" ? answer.status : 0;
  return !(status >= 400 && status < 500 && typeof answer.code === "string" && answer.code !== "bad_response");
}

export type CreditOutcome<T> = { kind: "done"; value: T } | { kind: "refused"; error: unknown } | { kind: "unknown"; error: unknown };

/** Sends the credit once and classifies how it ended: done, a definite refusal, or an unknown result. */
export async function sendCredit<T>(post: () => Promise<T>): Promise<CreditOutcome<T>> {
  try {
    return { kind: "done", value: await post() };
  } catch (error) {
    return isUnknownOutcome(error) ? { kind: "unknown", error } : { kind: "refused", error };
  }
}
