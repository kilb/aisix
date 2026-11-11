import type { PublicModelRouting } from "./api.ts";

/**
 * The routing settings of a public model, as 模型与路由 edits them.
 *
 * The stored set (`PublicModel.routing`) is what the operator changed, so an
 * absent key means the gateway's own default. The form therefore shows an
 * empty field for "not set" and sends back only what the operator wrote: a
 * field cleared here is a key left out, which is how a setting goes back to
 * the default rather than being pinned to a copy of it.
 */
export interface RoutingDraft {
  strategy: string;
  retries: string;
  maxFallbacks: string;
  retryOn429: boolean;
  /** The extra statuses, comma-, space- or `、`-separated. */
  statuses: string;
  whenAllUnavailable: string;
}

/** The form's fields for a stored set. */
export function draftOf(routing: PublicModelRouting, defaultStrategy: string): RoutingDraft {
  return {
    strategy: typeof routing.strategy === "string" && routing.strategy !== "" ? routing.strategy : defaultStrategy,
    retries: routing.retries === undefined || routing.retries === null ? "" : String(routing.retries),
    maxFallbacks: routing.max_fallbacks === undefined || routing.max_fallbacks === null ? "" : String(routing.max_fallbacks),
    retryOn429: routing.retry_on_429 === true,
    statuses: Array.isArray(routing.fallback_on_statuses) ? routing.fallback_on_statuses.join(", ") : "",
    whenAllUnavailable:
      typeof routing.when_all_unavailable === "string" ? routing.when_all_unavailable : "",
  };
}

/** The statuses in `text`, or null when one of them is not a whole number. */
export function statusesOf(text: string): number[] | null {
  const parts = text.split(/[,，、\s]+/).filter((p) => p !== "");
  const out: number[] = [];
  for (const part of parts) {
    if (!/^\d+$/.test(part)) return null;
    out.push(Number(part));
  }
  return out;
}

/** The settings this form has a control for; every other key is carried over. */
const EDITED = ["strategy", "retries", "max_fallbacks", "retry_on_429", "fallback_on_statuses", "when_all_unavailable"];

/**
 * The body to send for `draft`.
 *
 * A key the form does not show (`hash_on`, and any setting a later gateway
 * adds) is carried over from `stored` exactly as read, so saving a strategy
 * never drops a field this form has no control for. Every key it does show is
 * decided here alone, so clearing a field clears what was stored rather than
 * silently keeping it.
 *
 * An empty text field is a key left out, which the control plane stores as
 * "the gateway's default". A strategy equal to `defaultStrategy` is left out
 * too — the control plane drops it as well, and sending it would only say the
 * same thing twice.
 */
export function routingBody(
  draft: RoutingDraft,
  stored: PublicModelRouting,
  defaultStrategy: string,
  statuses: number[] | null,
): PublicModelRouting {
  const body: PublicModelRouting = {};
  for (const [key, value] of Object.entries(stored)) {
    if (!EDITED.includes(key)) body[key] = value;
  }
  if (draft.strategy !== "" && draft.strategy !== defaultStrategy) body.strategy = draft.strategy;
  if (draft.retries.trim() !== "") body.retries = Number(draft.retries);
  if (draft.maxFallbacks.trim() !== "") body.max_fallbacks = Number(draft.maxFallbacks);
  if (draft.retryOn429) body.retry_on_429 = true;
  if (statuses !== null && statuses.length > 0) body.fallback_on_statuses = statuses;
  if (draft.whenAllUnavailable !== "") body.when_all_unavailable = draft.whenAllUnavailable;
  return body;
}

/** `value` as JSON with every object's keys sorted, so two equal sets compare equal. */
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

/** Whether two settings sets say the same thing, whatever order their keys are in. */
export function sameRouting(a: PublicModelRouting, b: PublicModelRouting): boolean {
  return canonical(a) === canonical(b);
}
