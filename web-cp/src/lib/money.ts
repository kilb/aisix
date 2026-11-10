// One copy per app (ruling P2c-R4): web-cp/src/lib/money.ts and web-tenant/src/lib/money.ts are byte-equal (pinned by tokenParity.test.ts).
// Change both in the same commit.

const MICRO_PER_UNIT = 1_000_000n;

/** `abs` with thousands separators: `123456` is `123,456`. */
function grouped(abs: bigint): string {
  return abs.toString().replace(/\B(?=(\d{3})+$)/g, ",");
}

/**
 * An amount of micro-CNY as `¥1,234.56`: exact (integer arithmetic on a
 * `BigInt`, never a float), six decimals trimmed of trailing zeros down to
 * two, thousands separators, and the sign before the `¥`. `fmtMicroCny(11_000)`
 * is `¥0.011`, not `¥0.01`. `micro` must be an integer; anything else throws.
 *
 * Every amount a tenant holds, is charged or is shown is micro-CNY, and every
 * one of them goes through this function; no page rounds a figure itself.
 */
export function fmtMicroCny(micro: number): string {
  const value = BigInt(micro);
  const abs = value < 0n ? -value : value;
  let fraction = (abs % MICRO_PER_UNIT).toString().padStart(6, "0");
  while (fraction.length > 2 && fraction.endsWith("0")) fraction = fraction.slice(0, -1);
  return `${value < 0n ? "-" : ""}¥${grouped(abs / MICRO_PER_UNIT)}.${fraction}`;
}

/**
 * An amount of micro-CNY as the yuan a person types into an edit field:
 * `1_500_000` is `1.50`, `-500_000` is `-0.50`. Exact, never a float; no `¥`
 * and no separators, so it reads back through `isCnyAmount`. Anything below
 * the fen is dropped: every amount typed in is whole fen.
 */
export function microCnyToInput(micro: number): string {
  const value = BigInt(micro);
  const abs = value < 0n ? -value : value;
  const fen = ((abs % MICRO_PER_UNIT) / 10_000n).toString().padStart(2, "0");
  return `${value < 0n ? "-" : ""}${abs / MICRO_PER_UNIT}.${fen}`;
}

/** Yuan typed by a person: up to eight digits and at most two decimals, and above zero unless `allowZero`. */
export function isCnyAmount(s: string, opts: { allowZero: boolean }): boolean {
  if (!/^\d{1,8}(\.\d{1,2})?$/.test(s)) return false;
  return opts.allowZero || /[1-9]/.test(s);
}

/**
 * A per-million rate as the API gave it: `¥28.80`. The value is an exact
 * decimal string, so nothing here is a float and nothing is rounded — a rate
 * with fewer than two decimals is padded to two so a column of them lines up,
 * and one with more keeps every decimal it has (`¥0.00000000000001`).
 */
export function fmtCnyPerMillion(s: string): string {
  const [whole = "0", fraction = ""] = s.split(".");
  const decimals = fraction.length < 2 ? fraction.padEnd(2, "0") : fraction;
  return `¥${grouped(BigInt(whole))}.${decimals}`;
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

/**
 * The decimal `s` with its point moved `places` to the right (left when
 * negative): exact, since only the point moves, and in its shortest form.
 * `null` for anything but digits with an optional `.` and more digits.
 */
function shiftPoint(s: string, places: number): string | null {
  const m = /^(\d+)(?:\.(\d+))?$/.exec(s);
  if (!m) return null;
  const whole = m[1] ?? "";
  let digits = whole + (m[2] ?? "");
  let point = whole.length + places;
  if (point < 0) {
    digits = "0".repeat(-point) + digits;
    point = 0;
  }
  if (point > digits.length) digits += "0".repeat(point - digits.length);
  const int = digits.slice(0, point).replace(/^0+(?=\d)/, "") || "0";
  const fraction = digits.slice(point).replace(/0+$/, "");
  return fraction ? `${int}.${fraction}` : int;
}

/**
 * A rate per million units as the same rate per thousand: `"10000"` is
 * `"10"`, `"3.5"` is `"0.0035"`. Exact string arithmetic, never a float.
 * What is not a decimal comes back as it was, so nothing is hidden.
 */
export function perThousandOfPerMillion(perMillion: string): string {
  return shiftPoint(perMillion, -3) ?? perMillion;
}

/**
 * A rate typed per thousand units as the rate per million a card stores:
 * `"10"` is `"10000"`, `"0.0035"` is `"3.5"`. Exact; `null` for anything but
 * a plain decimal of at most 9 decimals, which is the most a card's 6
 * decimals per million hold.
 */
export function perMillionOfPerThousand(perThousand: string): string | null {
  if (!/^\d+(\.\d{1,9})?$/.test(perThousand)) return null;
  return shiftPoint(perThousand, 3);
}
