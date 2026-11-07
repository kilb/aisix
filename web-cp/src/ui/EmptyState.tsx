import type { ReactNode } from "react";
import { Mark, type MarkName } from "./Icon";

/**
 * An empty list: a 32px muted mark, one sentence on what to do next, and
 * the one action that starts it, centred with 48px of space above and below.
 */
export function EmptyState({ text, action, mark = "route" }: { text: string; action?: ReactNode; mark?: MarkName }) {
  return (
    <div className="empty">
      <Mark name={mark} />
      <p>{text}</p>
      {action}
    </div>
  );
}
