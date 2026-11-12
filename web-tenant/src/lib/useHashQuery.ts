import { useCallback, useEffect, useState } from "react";

/** query 变化不卸载页面；浏览器返回和可分享链接直接恢复已显示的筛选。 */
export function useHashQuery() {
  const [search, setSearch] = useState(() => typeof window === "undefined" ? "" : window.location.hash.split("?", 2)[1] ?? "");
  useEffect(() => {
    const read = () => setSearch(window.location.hash.split("?", 2)[1] ?? "");
    window.addEventListener("hashchange", read);
    return () => window.removeEventListener("hashchange", read);
  }, []);
  const update = useCallback((patch: Record<string, string | undefined>) => {
    const [page, current = ""] = window.location.hash.split("?", 2);
    const query = new URLSearchParams(current);
    for (const [key, value] of Object.entries(patch)) {
      if (value) query.set(key, value);
      else query.delete(key);
    }
    const next = query.toString();
    window.location.hash = `${page}${next ? `?${next}` : ""}`;
  }, []);
  return [new URLSearchParams(search), update] as const;
}
