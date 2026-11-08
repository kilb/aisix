// One copy per app (ruling P2c-R4): web-cp/src/lib/money.ts is the same file.
// Change both in the same commit.

const MICRO_PER_USD = 1_000_000n;

/**
 * An amount of micro-dollars as `$1,234.5678`: exact (integer arithmetic on
 * a `BigInt`, never a float), six decimals trimmed of trailing zeros down to
 * two, thousands separators, and the sign before the `$`. `fmtMicroUsd(11_000)`
 * is `$0.011`, not `$0.01`. `micro` must be an integer; anything else throws.
 */
export function fmtMicroUsd(micro: number): string {
  const value = BigInt(micro);
  const abs = value < 0n ? -value : value;
  const whole = (abs / MICRO_PER_USD).toString().replace(/\B(?=(\d{3})+$)/g, ",");
  let fraction = (abs % MICRO_PER_USD).toString().padStart(6, "0");
  while (fraction.length > 2 && fraction.endsWith("0")) fraction = fraction.slice(0, -1);
  return `${value < 0n ? "-" : ""}$${whole}.${fraction}`;
}

/** Dollars typed by a person: up to seven digits and at most two decimals, and above zero unless `allowZero`. */
export function isUsdAmount(s: string, opts: { allowZero: boolean }): boolean {
  if (!/^\d{1,7}(\.\d{1,2})?$/.test(s)) return false;
  return opts.allowZero || /[1-9]/.test(s);
}

/** The day after a `YYYY-MM-DD` date, as `YYYY-MM-DD`. */
function nextDay(date: string): string {
  const [y = 0, m = 1, d = 1] = date.split("-").map(Number);
  return new Date(Date.UTC(y, m - 1, d + 1)).toISOString().slice(0, 10);
}

/**
 * The half-open UTC interval covering whole days `fromDate` to `toDate`,
 * both inclusive: from the first day's midnight to the midnight after the
 * last.
 */
export function utcRange(fromDate: string, toDate: string): { from: string; to: string } {
  return { from: `${fromDate}T00:00:00Z`, to: `${nextDay(toDate)}T00:00:00Z` };
}

/** The UTC date of `now`, as `YYYY-MM-DD`, whatever the viewer's time zone. */
export function todayUtc(now: Date = new Date()): string {
  return now.toISOString().slice(0, 10);
}

/** An ISO instant as `2026-11-07 13:05:09 UTC`. A value that is not a date is shown as it came. */
export function fmtUtc(iso: string): string {
  const at = new Date(iso);
  if (Number.isNaN(at.getTime())) return iso;
  return `${at.toISOString().slice(0, 19).replace("T", " ")} UTC`;
}

/** The label of a usage period: its UTC day (`2026-11-07`) or its UTC month (`2026-11`). */
export function periodLabel(periodStart: string, group: "day" | "month"): string {
  const at = new Date(periodStart);
  const day = Number.isNaN(at.getTime()) ? periodStart.slice(0, 10) : at.toISOString().slice(0, 10);
  return group === "day" ? day : day.slice(0, 7);
}
