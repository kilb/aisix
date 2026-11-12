export const PORTAL_TABS = [
  { id: "org", label: "组织", group: "工作空间" },
  { id: "keys", label: "密钥", group: "工作空间" },
  { id: "prices", label: "价格", group: "工作空间" },
  { id: "usage", label: "用量", group: "使用与账务" },
  { id: "budgets", label: "预算", group: "使用与账务" },
  { id: "billing", label: "账务账户", group: "使用与账务" },
  { id: "members", label: "成员", group: "组织管理" },
  { id: "account", label: "账号设置", group: "个人" },
] as const;

export type PortalTabId = (typeof PORTAL_TABS)[number]["id"];

/** 深链仍使用既有 hash；账号不复用组织的 billing 页面。 */
export function portalRouteOf(hash: string): { tab: PortalTabId; group: string | null } {
  const [name, query = ""] = hash.replace(/^#/, "").split("?", 2);
  const tab = PORTAL_TABS.find((item) => item.id === name)?.id ?? "org";
  const group = new URLSearchParams(query).get("group");
  return { tab, group: group === "" ? null : group };
}

/** 个人安全独立于组织状态；组织业务权限保持原规则。 */
export function canSeePortalTab(id: PortalTabId, role: string, status: string): boolean {
  if (id === "account" || id === "org") return true;
  if (status !== "active" && status !== "suspended") return false;
  if ((id === "usage" || id === "budgets" || id === "billing" || id === "prices") && status !== "active") return false;
  switch (id) {
    case "keys":
      return role === "owner" || role === "admin" || role === "member";
    case "members":
      return role === "owner" || role === "admin";
    case "usage":
    case "budgets":
    case "prices":
      return role === "owner" || role === "admin" || role === "member" || role === "billing";
    case "billing":
      return role === "owner" || role === "admin" || role === "billing";
  }
}
