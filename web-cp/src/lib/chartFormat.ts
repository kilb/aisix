// 两个应用保留一致实现；金额显示继续使用 money.ts，本文件只投影图形比例。
export type ExactValue = number | string | bigint | null | undefined;

/** 不安全 number 已失去精度，不能用 BigInt 将其伪装成精确值。 */
export function exactInteger(value: ExactValue): bigint | null {
  if (typeof value === "bigint") return value;
  if (typeof value === "number") return Number.isSafeInteger(value) ? BigInt(value) : null;
  return typeof value === "string" && /^-?\d+$/.test(value) ? BigInt(value) : null;
}

const GEOMETRY_SCALE = 1_000_000n;

/** 比例只用于布局；超额封顶，零限额与未知均无可用比例。 */
export function exactRatio(value: ExactValue, total: ExactValue): number | null {
  const numerator = exactInteger(value);
  const denominator = exactInteger(total);
  if (numerator === null || denominator === null || denominator <= 0n) return null;
  if (numerator <= 0n) return 0;
  if (numerator >= denominator) return 1;
  return Number(numerator * GEOMETRY_SCALE / denominator) / Number(GEOMETRY_SCALE);
}

/** 先用整数缩至六位比例再转 number；负值保留，原金额不经浮点。 */
export function normalizeExactValues(values: readonly ExactValue[]): (number | null)[] {
  const integers = values.map(exactInteger);
  let maximum = 0n;
  for (const value of integers) {
    if (value === null) continue;
    const absolute = value < 0n ? -value : value;
    if (absolute > maximum) maximum = absolute;
  }
  return integers.map(value => value === null ? null : maximum === 0n ? 0 : Number(value * GEOMETRY_SCALE / maximum) / Number(GEOMETRY_SCALE));
}

/** 普通计数也保留大整数；缺失与非法输入不显示成零。 */
export function formatCount(value: ExactValue): string {
  const integer = exactInteger(value);
  return integer === null ? "—" : integer.toString().replace(/\B(?=(\d{3})+$)/g, ",");
}

export function formatRatio(value: number | null | undefined, digits = 1): string {
  return value === null || value === undefined || !Number.isFinite(value)
    ? "—"
    : new Intl.NumberFormat("en-US", { style: "percent", maximumFractionDigits: digits }).format(value);
}

/** 参数沿用调用日志的毫秒；未知首 token 延迟不回退到其他耗时。 */
export function formatDuration(milliseconds: number | null | undefined): string {
  if (milliseconds === null || milliseconds === undefined || !Number.isFinite(milliseconds) || milliseconds < 0) return "—";
  const unit = milliseconds < 1000 ? "ms" : "s";
  const value = milliseconds < 1000 ? milliseconds : milliseconds / 1000;
  return `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(value)} ${unit}`;
}

export interface UsageChartRow {
  period_start: string;
  model: { id: string | null; name: string };
  requests: ExactValue;
  charged_micro_cny: ExactValue;
  estimated_requests: ExactValue;
}

export interface UsageChartSummary {
  requests: bigint | null;
  chargedMicroCny: bigint | null;
  estimatedRequests: bigint | null;
  hasMergedPeriods: boolean;
  periods: { label: string; requests: bigint | null; chargedMicroCny: bigint | null }[];
  models: { id: string | null; label: string; requests: bigint | null; chargedMicroCny: bigint | null }[];
}

function addKnown(left: bigint | null, right: bigint | null): bigint | null {
  return left === null || right === null ? null : left + right;
}

function countValue(value: ExactValue): bigint | null {
  const integer = exactInteger(value);
  return integer !== null && integer >= 0n ? integer : null;
}

/** 仅消费全窗口 summary，不接受分页调用日志；other 的日期不能代表其跨期流量。 */
export function summarizeUsage(rows: readonly UsageChartRow[]): UsageChartSummary {
  const summary: UsageChartSummary = { requests: 0n, chargedMicroCny: 0n, estimatedRequests: 0n, hasMergedPeriods: false, periods: [], models: [] };
  const periods = new Map<string, UsageChartSummary["periods"][number]>();
  const models = new Map<string, UsageChartSummary["models"][number]>();
  for (const row of rows) {
    const requests = countValue(row.requests);
    const charged = exactInteger(row.charged_micro_cny);
    summary.requests = addKnown(summary.requests, requests);
    summary.chargedMicroCny = addKnown(summary.chargedMicroCny, charged);
    summary.estimatedRequests = addKnown(summary.estimatedRequests, countValue(row.estimated_requests));
    const other = row.model.id === null && row.model.name === "other";
    if (other) summary.hasMergedPeriods = true;
    const modelKey = row.model.id === null ? `name:${row.model.name}` : `id:${row.model.id}`;
    const model = models.get(modelKey) ?? { id: row.model.id, label: other ? "其他" : row.model.name, requests: 0n, chargedMicroCny: 0n };
    model.requests = addKnown(model.requests, requests);
    model.chargedMicroCny = addKnown(model.chargedMicroCny, charged);
    models.set(modelKey, model);
    const period = periods.get(row.period_start) ?? { label: row.period_start, requests: 0n, chargedMicroCny: 0n };
    period.requests = addKnown(period.requests, requests);
    period.chargedMicroCny = addKnown(period.chargedMicroCny, charged);
    periods.set(row.period_start, period);
  }
  summary.models = [...models.values()];
  summary.periods = summary.hasMergedPeriods ? [] : [...periods.values()].sort((left, right) => left.label.localeCompare(right.label));
  return summary;
}
