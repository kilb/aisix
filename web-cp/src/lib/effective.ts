// Start-time handling shared by the price forms.

/** What a form asks `EffectiveFrom` for when it submits. */
export interface EffectiveResolver {
  /** True while 立即生效 is on: the instant is stamped at submit. */
  immediate: boolean;
  /** The ISO instant to send, stamped now when immediate; null when the typed time is not a UTC time. */
  stamp(): string | null;
}

const TYPED = /^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2})(:\d{2})?Z?$/;

/** A time typed as `YYYY-MM-DD HH:mm[:ss]` in UTC, as an ISO instant; null when it is not one. */
export function parseUtc(text: string): string | null {
  const m = TYPED.exec(text.trim());
  if (!m) return null;
  const at = new Date(`${m[1]}T${m[2]}${m[3] ?? ":00"}Z`);
  return Number.isNaN(at.getTime()) ? null : at.toISOString();
}

/** A 422 refusal at `/effective_from` (duck-typed, so this file needs nothing from the API client). */
function isPastStart(e: unknown): boolean {
  const err = e as { status?: unknown; details?: { path?: unknown }[] } | null;
  return !!err && err.status === 422 && Array.isArray(err.details) && err.details.some((d) => d.path === "/effective_from");
}

/**
 * Sends a write with the resolver's start. A start stamped at submit that the
 * server finds in the past (`422 /effective_from`: the clock moved between
 * the stamp and the check) is stamped again and sent once more; a typed time
 * is never retried. `null` from the resolver (a time that is not valid) is
 * `invalid`.
 */
export async function sendAt<T>(
  resolver: EffectiveResolver,
  send: (effectiveFrom: string) => Promise<T>,
): Promise<T | "invalid"> {
  const first = resolver.stamp();
  if (first === null) return "invalid";
  try {
    return await send(first);
  } catch (e) {
    const past = isPastStart(e);
    if (!resolver.immediate || !past) throw e;
    const again = resolver.stamp();
    if (again === null) throw e;
    return await send(again);
  }
}
