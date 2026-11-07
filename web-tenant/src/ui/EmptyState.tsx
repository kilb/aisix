import type { ReactNode } from "react";
import { Mark, type MarkName } from "./Icon";

// The portal's copy of the Dashboard's EmptyState (P2c-R4).

/**
 * An empty list: a 32px muted mark, one sentence on what to do next, and,
 * when the page has none in view already, the one action that starts it.
 */
export function EmptyState({ mark, text, action }: { mark: MarkName; text: string; action?: ReactNode }) {
  return (
    <div className="empty">
      <Mark name={mark} />
      <p>{text}</p>
      {action}
    </div>
  );
}
