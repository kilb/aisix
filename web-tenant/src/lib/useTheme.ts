// Modelled on the Dashboard's useTheme (web-cp/src/lib/useTheme.ts), with the portal's
// storage key.
import { useCallback, useLayoutEffect, useState } from "react";

export type Theme = "dark" | "light";
const KEY = "aisix-portal-theme";

function stored(): Theme {
  try {
    return localStorage.getItem(KEY) === "dark" ? "dark" : "light";
  } catch {
    // Storage blocked (a private window, a sandboxed frame): the default.
    return "light";
  }
}

/**
 * The viewer's theme: light unless they chose dark with the toggle, which
 * is remembered in this browser. The system's dark-mode setting is never
 * read: light is the default whatever it says (owner decision, P2c-R10).
 * index.html applies a stored dark choice to `<html data-theme>` before the
 * first paint, so a reload in dark never flashes light; this hook keeps the
 * attribute in step with the toggle from then on.
 */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(stored);

  useLayoutEffect(() => {
    const root = document.documentElement;
    if (root.getAttribute("data-theme") === theme) return;
    // 主题前景/背景须同时落地，不能在两套颜色之间插值成低对比度。
    root.setAttribute("data-theme-changing", "");
    root.setAttribute("data-theme", theme);
    // 同步提交受保护的新样式，恢复正常 hover/抽屉过渡后再允许下一次绘制。
    void root.offsetHeight;
    root.removeAttribute("data-theme-changing");
  }, [theme]);

  const toggle = useCallback(() => {
    setTheme((t) => {
      const next = t === "dark" ? "light" : "dark";
      try {
        localStorage.setItem(KEY, next);
      } catch {
        // Not remembered; it still applies to this page.
      }
      return next;
    });
  }, []);

  return [theme, toggle];
}
