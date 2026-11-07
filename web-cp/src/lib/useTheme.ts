// Copied from web/src/lib/useTheme.ts; comments translated, storage key renamed.
import { useCallback, useEffect, useState } from "react";

export type Theme = "dark" | "light";
const KEY = "aisix-cp-theme";

/**
 * The theme choice. Dark by default rather than following the system.
 *
 * A product decision, not an oversight: the console is a meter room, and a
 * meter room is dark, lit by its own instruments. Following the system would
 * keep most users from ever seeing it as it is meant to look.
 *
 * But a default is not a mandate: once a user has chosen explicitly, the
 * choice sticks.
 */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = localStorage.getItem(KEY);
    return saved === "light" || saved === "dark" ? saved : "dark";
  });

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
  }, [theme]);

  const toggle = useCallback(() => {
    setTheme((t) => {
      const next = t === "dark" ? "light" : "dark";
      localStorage.setItem(KEY, next);
      return next;
    });
  }, []);

  return [theme, toggle];
}
