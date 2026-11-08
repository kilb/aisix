// Percent inputs of the pricing pages, as integer basis points. Integer arithmetic only.

/** `加价（%）` typed with at most 2 decimals, as basis points; null when it is not such a number. */
export function markupBps(text: string): number | null {
  const m = /^(\d{1,5})(?:\.(\d{1,2}))?$/.exec(text.trim());
  if (!m) return null;
  return Number(m[1]) * 100 + Number((m[2] ?? "").padEnd(2, "0"));
}

/** `折扣（%）` typed with at most 2 decimals, 0 to 100, as basis points; null when it is not such a number. */
export function discountBps(text: string): number | null {
  const m = /^(\d{1,3})(?:\.(\d{1,2}))?$/.exec(text.trim());
  if (!m) return null;
  const bps = Number(m[1]) * 100 + Number((m[2] ?? "").padEnd(2, "0"));
  return bps <= 10_000 ? bps : null;
}

/** Basis points as a percentage in integer arithmetic: 2000 is `20%`, 1250 is `12.5%`. */
export function fmtPercent(bps: number): string {
  const frac = String(bps % 100).padStart(2, "0").replace(/0+$/, "");
  return `${Math.floor(bps / 100)}${frac ? `.${frac}` : ""}%`;
}

