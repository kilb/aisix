// 只索引表格已公开的业务字段，避免把密钥、原始资源文档或审计详情加入搜索。
export function keySearchText(row: { id: string; label: string; prefix: string; group?: { name: string; display_name: string } | null }): string[] {
  return [row.id, row.label, row.prefix, row.group?.name ?? "", row.group?.display_name ?? ""];
}

export function resourceSearchText(row: { id: string; doc: { name?: unknown; display_name?: unknown } }): string[] {
  return [row.id, typeof row.doc.name === "string" ? row.doc.name : "", typeof row.doc.display_name === "string" ? row.doc.display_name : ""];
}

export function auditSearchText(row: { id: number; actor: { kind: string; id: string | null; email: string | null }; action: string; target_kind: string | null; target_id: string | null; tenant_id: string | null }): string[] {
  return [String(row.id), row.actor.kind, row.actor.id ?? "", row.actor.email ?? "", row.action, row.target_kind ?? "", row.target_id ?? "", row.tenant_id ?? ""];
}

// 当前页不足一页时，新增匹配项先补当前页，避免搜索后继续加载时跳过结果。
export function cursorPageAfterLoad(page: number, pageSize: number, filteredCount: number, requestedPage: number): number {
  return filteredCount < page * pageSize ? page : requestedPage;
}
