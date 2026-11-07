import type { Theme } from "../lib/useTheme";
import { Button } from "./Button";

/** The header's theme switch: names the theme it switches to. */
export function ThemeToggle({ theme, onToggle }: { theme: Theme; onToggle: () => void }) {
  const next = theme === "dark" ? "浅色" : "深色";
  return (
    <Button
      kind="quiet"
      size="sm"
      icon={theme === "dark" ? "sun" : "moon"}
      onClick={onToggle}
      aria-label={`切换到${next}`}
      title={`切换到${next}`}
    >
      {next}
    </Button>
  );
}
