export function metricViewOf(value: string | null): "requests" | "latency" | "tokens" | "cache" | "cost" {
  return value === "latency" || value === "tokens" || value === "cache" || value === "cost" ? value : "requests";
}

/** 记录接口不支持时间筛选，不将分析时间写成已应用的过滤条件。 */
export function chargesQuery(query: URLSearchParams): Record<string, string> {
  const filter: Record<string, string> = {};
  for (const key of ["tenant_id", "api_key_id", "model_id", "error_code", "event_id"]) {
    const value = query.get(key);
    if (value) filter[key] = value;
  }
  for (const key of ["status", "exclude_status"]) {
    const value = query.get(key);
    if (value && ["charged", "exempt", "nothing_to_bill", "unpriced"].includes(value)) filter[key] = value;
  }
  return filter;
}
