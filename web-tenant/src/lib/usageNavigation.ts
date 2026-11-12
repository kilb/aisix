import { beijingRange, parseBeijingInput, todayBeijing } from "./time.ts";

export function metricViewOf(value: string | null): "requests" | "latency" | "tokens" | "cache" | "cost" {
 return value === "latency" || value === "tokens" || value === "cache" || value === "cost" ? value : "requests";
}

/** API 时间窗保持精确，日期输入仅显示对应的北京日期。 */
export function usageDateValue(value: string | null, end = false): string {
 if (!value) return "";
 if (/^\d{4}-\d{2}-\d{2}$/.test(value)) return value;
 const time = Date.parse(value);
 return Number.isFinite(time) ? todayBeijing(new Date(time - (end ? 1 : 0))) : "";
}

export function usageRange(query: URLSearchParams): { from?: string; to?: string } | null {
 const result: { from?: string; to?: string } = {};
 for (const key of ["from", "to"] as const) {
  const raw = query.get(key);
  if (!raw) continue;
  if (/^\d{4}-\d{2}-\d{2}$/.test(raw)) {
   if (parseBeijingInput(`${raw}T00:00:00`) === null) return null;
   result[key] = beijingRange(raw, raw)[key];
  } else {
   if (!Number.isFinite(Date.parse(raw))) return null;
   result[key] = raw;
  }
 }
 if (result.from && result.to && Date.parse(result.to) <= Date.parse(result.from)) return null;
 return result;
}

/** 原始日期也是身份的一部分：空范围与非法范围不能共享依赖。 */
export function usageRequestKey(query: URLSearchParams, request: object): string {
 return JSON.stringify([query.get("from"), query.get("to"), usageRange(query) !== null, request]);
}
