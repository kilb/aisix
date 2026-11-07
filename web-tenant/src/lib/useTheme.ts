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
 * The choice is applied to `<html data-theme>` before the first paint, so a
 * reload in dark never flashes light.
 */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(stored);

  useLayoutEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
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
