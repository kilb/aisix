/** 详情标记与列表条件共存；返回时不改动筛选或游标。 */
export function callDetailHash(query: URLSearchParams, eventId: string): string {
  const next = new URLSearchParams(query);
  next.set("detail", eventId);
  return `#calls?${next}`;
}

export function callListHash(query: URLSearchParams): string {
  const next = new URLSearchParams(query);
  next.delete("detail");
  const search = next.toString();
  return `#calls${search ? `?${search}` : ""}`;
}

/** 缺失不补零；记录中的零与 false 都有独立含义。 */
export function logValue(value: unknown): string {
  if (value === null || value === undefined || value === "") return "未记录";
  if (typeof value === "boolean") return value ? "是" : "否";
  if (typeof value === "number" || typeof value === "string") return String(value);
  return JSON.stringify(value);
}

export function attemptKindLabel(kind: string | null | undefined): string {
  return ({ initial: "首次尝试", retry: "同目标重试", fallback: "故障转移" } as Record<string, string>)[kind ?? ""] ?? "类型未记录";
}

/** 已记录错误优先于2xx；缺少最终标记或有效状态时保持未知。 */
export function callOutcomeLabel(call: { status_code: number | null; error_class: string | null; is_final_attempt: boolean | null; request_group?: { complete: boolean } }): string {
  if (call.request_group?.complete === false) return "结果未知";
  if (call.is_final_attempt === false) return "已重试";
  if (call.is_final_attempt !== true) return "结果未知";
  if (call.error_class) return "失败";
  const code = call.status_code;
  if (code == null || code < 100 || code >= 600) return "结果未知";
  return code >= 200 && code < 300 ? "成功" : "失败";
}
