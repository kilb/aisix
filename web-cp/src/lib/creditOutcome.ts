/**
 * A direct credit has no idempotency key: the request may have been stored while the
 * answer was lost (dropped connection, a proxy's 5xx). The outcome of such a failure is
 * unknown, so the caller must not hand the form back to the operator to retry.
 * Only the shape of `status` is read, without importing `ApiError` itself, so this file
 * loads directly under node:test.
 */
export function isUnknownOutcome(e: unknown): boolean {
  const status = typeof e === "object" && e !== null ? (e as { status?: unknown }).status : undefined;
  // Not an ApiError (a network-layer exception, a parse failure): unknown whether the request reached the backend.
  if (typeof status !== "number") return true;
  return status === 0 || status >= 500;
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
