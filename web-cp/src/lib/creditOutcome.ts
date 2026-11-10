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

/**
 * Where a tenant's direct credit stands outside any one form: `pending` while
 * a request is on its way, `unknown` once it ended without a definite answer.
 */
export type CreditLock = "pending" | "unknown";

/**
 * The per-tenant credit locks. They live outside the form, so closing the
 * panel (or opening another tenant) never hands a fresh, unlocked form back,
 * and they are observable, so a form opened while a credit is still on its
 * way follows that credit to its end: locked while it is pending, and kept
 * locked if it turns out unknown. A credit refused after its form closed
 * leaves the refusal here, for the tenant's next form to say until another
 * credit is sent: nothing was credited, and a form that just unlocks says
 * nothing of it. They last for this page load only; a reload starts clean,
 * as the operator then reads the ledger anyway.
 */
export function createCreditLocks() {
  const locks = new Map<string, CreditLock>();
  const refusals = new Map<string, string>();
  const listeners = new Set<() => void>();
  const changed = () => {
    for (const listener of listeners) listener();
  };
  return {
    get(tenant: string): CreditLock | null {
      return locks.get(tenant) ?? null;
    },
    /**
     * Takes the lock for a new credit, synchronously, so a second confirm
     * cannot get in: false while another credit of the tenant is on its way
     * or unresolved.
     */
    acquire(tenant: string): boolean {
      if (locks.has(tenant)) return false;
      locks.set(tenant, "pending");
      refusals.delete(tenant);
      changed();
      return true;
    },
    /**
     * The credit ended: an unknown outcome keeps the lock until the operator
     * has checked; any other frees it. `refused` is what a refusal says when
     * no form is left to show it.
     */
    settle(tenant: string, outcome: CreditOutcome<unknown>["kind"], refused?: string): void {
      if (outcome === "unknown") locks.set(tenant, "unknown");
      else locks.delete(tenant);
      if (outcome === "refused" && refused !== undefined) refusals.set(tenant, refused);
      changed();
    },
    /** What the tenant's last credit, refused after its form closed, was refused for; `null` for none. */
    refusal(tenant: string): string | null {
      return refusals.get(tenant) ?? null;
    },
    /** The operator has checked an unknown credit and starts over; a credit still on its way keeps its lock. */
    release(tenant: string): void {
      if (locks.get(tenant) !== "unknown") return;
      locks.delete(tenant);
      changed();
    },
    subscribe(listener: () => void): () => void {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

/** The page's own locks, shared by every credit form it mounts. */
export const creditLocks = createCreditLocks();
