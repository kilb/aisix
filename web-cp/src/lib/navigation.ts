export const ADMIN_TAB_IDS = ["overview", "analytics", "calls", "providers", "models", "public-routes", "model-tests", "cache", "tenants", "pricing", "billing", "keys", "gateways", "resources", "operators", "audit", "account"] as const;
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
  }
  return { tab, query };
}

export function canSeeAdminTab(tab: AdminTabId, role: string): boolean {
  return tab !== "operators" || role === "admin";
}

export function adminHash(tab: AdminTabId, section?: string): string {
  const route = adminRouteOf(`#${tab}${section ? `?section=${encodeURIComponent(section)}` : ""}`);
  const query = route.query.toString();
  return `#${route.tab}${query ? `?${query}` : ""}`;
}
