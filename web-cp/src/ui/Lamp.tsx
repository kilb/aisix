import type { ReactNode } from "react";

export type LampState = "applied" | "propagating" | "pending" | "rejected" | "disabled";

/**
 * A signal lamp: a dot beside a word, never colour alone. Green is applied,
 * blue propagating (it pulses once as it appears), hollow pending, red
 * rejected, grey disabled. The one lamp for every state on every page.
 */
export function Lamp({ state, children, title }: { state: LampState; children: ReactNode; title?: string }) {
  return (
    <span className="lamp" data-lamp={state} title={title}>
      {children}
    </span>
  );
}
