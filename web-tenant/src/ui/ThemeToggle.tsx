import type { Theme } from "../lib/useTheme";
import { Icon } from "./Icon";

/** The header's theme switch: names the theme it switches to. */
export function ThemeToggle({ theme, onToggle }: { theme: Theme; onToggle: () => void }) {
  const next = theme === "dark" ? "浅色" : "深色";
  return (
    <button type="button" className="btn btn-quiet btn-sm" onClick={onToggle} aria-label={`切换到${next}`} title={`切换到${next}`}>
      <Icon name={theme === "dark" ? "sun" : "moon"} />
      {next}
    </button>
  );
}
