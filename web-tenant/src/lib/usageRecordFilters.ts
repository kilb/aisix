export interface UsageRecordFilterValues {
  outcome: string;
  http_status: string;
  error_class: string;
  cache_hit: string;
  service_tier: string;
  operation: string;
  final_attempt: string;
}

export type UsageRecordFilterKey = keyof UsageRecordFilterValues;
export type UsageRecordApiFilters = Partial<UsageRecordFilterValues>;

export const USAGE_RECORD_FILTER_FIELDS: readonly UsageRecordFilterKey[] = [
  "outcome", "http_status", "error_class", "cache_hit", "service_tier", "operation", "final_attempt",
];
export const EMPTY_USAGE_RECORD_FILTERS: Readonly<UsageRecordFilterValues> = {
  outcome: "", http_status: "", error_class: "", cache_hit: "", service_tier: "", operation: "", final_attempt: "",
};

/** URL 是可分享的筛选状态；未知值交由服务端校验，不悄悄放宽为全部。 */
export function usageRecordFilterValues(query: URLSearchParams): UsageRecordFilterValues {
  return Object.fromEntries(USAGE_RECORD_FILTER_FIELDS.map(field => [field, query.get(field) ?? ""])) as unknown as UsageRecordFilterValues;
}

/** 仅传递已声明的筛选字段，false 字符串仍是明确条件；租户不提供原始状态筛选。 */
export function usageRecordFilterParams(value: UsageRecordFilterValues, scope: "admin" | "tenant"): UsageRecordApiFilters {
  return Object.fromEntries(USAGE_RECORD_FILTER_FIELDS.filter(field => value[field] !== "" && (scope === "admin" || field !== "http_status")).map(field => [field, value[field]]));
}
