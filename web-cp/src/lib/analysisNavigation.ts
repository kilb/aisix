export function metricViewOf(value: string | null): "requests" | "latency" | "tokens" | "cache" | "cost" {
  return value === "latency" || value === "tokens" || value === "cache" || value === "cost" ? value : "requests";
}

/** 分析深链接与调用记录使用相同的服务端筛选；其他 URL 字段不扩大范围。 */
export function chargesQuery(query: URLSearchParams): Record<string, string> {
  const filter: Record<string, string> = {};
  for (const key of ["tenant_id", "api_key_id", "error_code", "event_id", "from", "to", "member_id", "provider_key_id", "request_id", "outcome", "http_status", "error_class", "cache_hit", "service_tier", "operation", "final_attempt", "priced_since", "priced_before", "exclude_corrected"]) {
    const value = query.get(key);
    if (value) filter[key] = value;
  }
  // 这些字段的空串是明确的未记录值，不能在导航时扩大成全部。
  for (const key of ["model_id", "group_id", "pricing_tier"]) {
    const value = query.get(key);
    if (value !== null) filter[key] = value;
  }
  for (const key of ["status", "exclude_status"]) {
    const value = query.get(key);
    if (value && ["charged", "exempt", "nothing_to_bill", "unpriced"].includes(value)) filter[key] = value;
  }
  return filter;
}
