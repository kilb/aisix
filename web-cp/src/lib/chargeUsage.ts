/**
 * What 计费明细 shows to check a charge: the quantities and rates it billed
 * (its `lines`), the counters the upstream reported (its `usage`), and the
 * charge recomputed from the lines by the engine's own formula, in integers:
 *
 *   charge µ¥ = ⌈ Σ quantity × rate_µ$ × cny_per_usd_e4 × (10000 − discount_bps) / 10¹⁴ ⌉
 *
 * where rate_µ$ is the line's USD rate per million units in micro-dollars
 * (crates/aisix-pricing, "No floating point on the charge path"). No float
 * touches a figure shown here.
 */

/** One billed line as a charge stores it: the rate is USD per million units, as a decimal string. */
export interface ChargeLine {
  dimension: string;
  quantity: number;
  rate: string;
}

/** The counters a usage row can carry, as the charges listing returns them. */
export type ReportedUsage = Partial<Record<string, number>>;

const DIMENSION_LABEL: Record<string, string> = {
  input: "输入",
  output: "输出",
  reasoning: "推理",
  cache_read: "缓存读取",
  cache_write: "缓存写入（5 分钟）",
  cache_write_1h: "缓存写入（1 小时）",
  audio_input: "音频输入",
  audio_output: "音频输出",
  image_input: "图片输入",
  image_output: "图片输出",
  web_search: "联网搜索（次）",
};

/** A billed dimension's name on the page; an unknown one shows as stored. */
export function dimensionLabel(dimension: string): string {
  return DIMENSION_LABEL[dimension] ?? dimension;
}

/** The counters the upstream reports, in the order the page lists them, with what each counts. */
const REPORTED: { key: string; label: string }[] = [
  { key: "prompt_tokens", label: "输入" },
  { key: "cached_prompt_tokens", label: "其中缓存命中" },
  { key: "cache_write_tokens", label: "其中缓存写入" },
  { key: "cache_read_tokens", label: "缓存读取（另计）" },
  { key: "cache_creation_tokens", label: "缓存写入（另计）" },
  { key: "cache_creation_1h_tokens", label: "其中 1 小时缓存写入" },
  { key: "completion_tokens", label: "输出" },
  { key: "reasoning_tokens", label: "其中推理" },
  { key: "web_search_requests", label: "联网搜索次数" },
  { key: "total_tokens", label: "上游报告的总数" },
];

/** The counters `usage` carries, labelled, in page order. A counter the row does not carry is left out. */
export function reportedCounters(usage: ReportedUsage | null | undefined): { key: string; label: string; value: number }[] {
  if (!usage) return [];
  return REPORTED.filter((r) => typeof usage[r.key] === "number").map((r) => ({ ...r, value: usage[r.key]! }));
}

const int = new Intl.NumberFormat("en-US");

/**
 * The row's usage in one short line: input and output, then cache reads and
 * writes when there were any. `null` when the row reported nothing.
 */
export function usageSummary(usage: ReportedUsage | null | undefined): string | null {
  if (!usage) return null;
  const n = (k: string) => (typeof usage[k] === "number" ? usage[k]! : 0);
  if (!["prompt_tokens", "completion_tokens", "cache_read_tokens", "cache_creation_tokens", "web_search_requests"].some((k) => k in usage))
    return null;
  const parts = [`输入 ${int.format(n("prompt_tokens"))}`, `输出 ${int.format(n("completion_tokens"))}`];
  const hit = n("cached_prompt_tokens") + n("cache_read_tokens");
  const written = n("cache_creation_tokens") + n("cache_write_tokens");
  if (hit > 0) parts.push(`缓存读 ${int.format(hit)}`);
  if (written > 0) parts.push(`缓存写 ${int.format(written)}`);
  if (n("web_search_requests") > 0) parts.push(`联网 ${int.format(n("web_search_requests"))} 次`);
  return parts.join(" · ");
}

/** A decimal string as an integer count of millionths; `null` when it is not a plain non-negative decimal. */
function micros(decimal: string): bigint | null {
  const m = /^(\d+)(?:\.(\d{1,6}))?$/.exec(decimal.trim());
  if (!m) return null;
  return BigInt(m[1]!) * 1_000_000n + BigInt((m[2] ?? "").padEnd(6, "0"));
}

/** `value / 10^scale` as a decimal string, trailing zeros dropped. */
function decimalText(value: bigint, scale: number): string {
  const unit = 10n ** BigInt(scale);
  const whole = value / unit;
  const frac = (value % unit).toString().padStart(scale, "0").replace(/0+$/, "");
  return frac ? `${int.format(whole)}.${frac}` : int.format(whole);
}

/** What one line comes to in USD, exactly: quantity × rate / 10⁶. `null` for an unreadable rate. */
export function lineUsd(line: ChargeLine): string | null {
  const rate = micros(line.rate);
  if (rate === null || !Number.isSafeInteger(line.quantity) || line.quantity < 0) return null;
  // rate is µ$ per million units, so quantity × rate counts 10⁻¹² USD.
  return decimalText(BigInt(line.quantity) * rate, 12);
}

/**
 * The charge in micro-CNY that `lines` come to at the group rate and the
 * discount, rounded up once, as the engine computes it. `null` when a rate
 * is unreadable or the rate in force is unknown.
 */
export function chargeFromLines(lines: ChargeLine[], cnyPerUsdE4: number | null, discountBps: number): number | null {
  if (cnyPerUsdE4 === null || !Number.isSafeInteger(cnyPerUsdE4) || !Number.isSafeInteger(discountBps)) return null;
  let sum = 0n;
  for (const line of lines) {
    const rate = micros(line.rate);
    if (rate === null || !Number.isSafeInteger(line.quantity) || line.quantity < 0) return null;
    sum += BigInt(line.quantity) * rate;
  }
  const numerator = sum * BigInt(cnyPerUsdE4) * BigInt(10_000 - discountBps);
  const denominator = 10n ** 14n;
  const charge = (numerator + denominator - 1n) / denominator;
  return Number(charge);
}

/** The group rate as typed: CNY per USD, from ten-thousandths. */
export function rateText(cnyPerUsdE4: number): string {
  return decimalText(BigInt(cnyPerUsdE4), 4);
}
