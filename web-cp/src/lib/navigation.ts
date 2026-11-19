export const ADMIN_TAB_IDS = ["overview", "analytics", "totals", "calls", "upstream-monitoring", "providers", "models", "public-routes", "model-tests", "cache", "tenants", "pricing", "billing", "keys", "gateways", "resources", "authentication", "operators", "audit", "account"] as const;
export type AdminTabId = (typeof ADMIN_TAB_IDS)[number];

/** 旧书签仅迁移已拆分的子页，其余账务入口保持含义。 */
export function adminRouteOf(hash: string): { tab: AdminTabId; query: URLSearchParams } {
  const [name, search = ""] = hash.replace(/^#/, "").split("?", 2);
  const query = new URLSearchParams(search);
  let tab: AdminTabId = ADMIN_TAB_IDS.find(id => id === name) ?? "overview";
  if (tab === "pricing" && query.get("section") === "public-models") {
    tab = "public-routes";
    query.delete("section");
  } else if (tab === "billing" && query.get("section") === "charges") {
    tab = "calls";
    query.delete("section");
  } else if (query.get("tenant") && ((tab === "billing" && (!query.get("section") || query.get("section") === "accounts")) || (tab === "pricing" && query.get("section") === "plans"))) {
    const source = tab;
    tab = "tenants";
    query.delete("section");
    query.set("view", source === "billing" ? "billing" : "discounts");
    query.set("from", source);
  }
  return { tab, query };
}

export const TENANT_VIEWS = [
  { id: "basic", label: "基本与限流" },
  { id: "access", label: "模型与分组" },
  { id: "members", label: "成员" },
  { id: "keys", label: "密钥" },
  { id: "billing", label: "余额与计费" },
  { id: "quotas", label: "额度" },
  { id: "discounts", label: "折扣记录" },
  { id: "activity", label: "账务记录" },
] as const;
export type TenantView = (typeof TENANT_VIEWS)[number]["id"];
export type TenantSource = "tenants" | "billing" | "pricing";

export function tenantViewOf(value: string | null | undefined): TenantView {
  return TENANT_VIEWS.find((view) => view.id === value)?.id ?? "basic";
}

export function tenantHash(id: string, view: TenantView = "basic", from: TenantSource = "tenants"): string {
  const query = new URLSearchParams({ tenant: id, view, from });
  return `#tenants?${query}`;
}

export function canSeeAdminTab(tab: AdminTabId, role: string): boolean {
  return role === "admin" || (tab !== "operators" && tab !== "authentication");
}

export function adminHash(tab: AdminTabId, section?: string): string {
  const route = adminRouteOf(`#${tab}${section ? `?section=${encodeURIComponent(section)}` : ""}`);
  const query = route.query.toString();
  return `#${route.tab}${query ? `?${query}` : ""}`;
}
