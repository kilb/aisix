import { PORTAL_TABS } from "./navigation.ts";

export type PortalEntry = "home" | "login" | "signup" | "console" | "invite";

export function portalPagePath(base: string, page: PortalEntry, hash = ""): string {
  const root = base.endsWith("/") ? base : base + "/";
  return root + (page === "home" ? "" : page) + hash;
}

export function portalEntryOf(pathname: string, hash: string, base: string): PortalEntry {
  const root = portalPagePath(base, "home");
  const page = pathname.startsWith(root) ? pathname.slice(root.length).replace(/\/$/, "") : "";

  switch (page) {
    case "login":
    case "signup":
    case "console":
    case "invite":
      return page;
    default:
      // 老控制台书签直接进入原分区，公开首页的价格锚点留在首页。
      const section = hash.slice(1).split("?", 1)[0];
      return !page && PORTAL_TABS.some((tab) => tab.id === section) ? "console" : "home";
  }
}
