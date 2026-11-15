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
