// Copied from web/src/lib/useTheme.ts; comments translated, storage key renamed.
import { useCallback, useLayoutEffect, useState } from "react";

export type Theme = "dark" | "light";
const KEY = "aisix-cp-theme";

/** The stored choice; storage that cannot be read counts as no choice. */
function stored(): Theme | null {
  try {
    const saved = localStorage.getItem(KEY);
    return saved === "light" || saved === "dark" ? saved : null;
  } catch {
    return null;
  }
}

/**
 * The theme choice. Light unless the viewer chose dark with the toggle: the
 * owner wants the Dashboard on a light background whatever the operating
 * system's setting, so `prefers-color-scheme` is never read. Dark stays
 * one click away, and the choice is kept per viewer.
 */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(() => (stored() === "dark" ? "dark" : "light"));

  // index.html applies a stored dark choice before the first paint; this
  // keeps <html data-theme> in step with the toggle from then on.
  useLayoutEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
  }, [theme]);

  const toggle = useCallback(() => {
    setTheme((t) => {
      const next = t === "dark" ? "light" : "dark";
      try {
        localStorage.setItem(KEY, next);
      } catch {
        // Storage refused: the choice holds for this page only.
      }
      return next;
    });
  }, []);

  return [theme, toggle];
}
