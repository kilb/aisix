// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import type { ReactNode } from "react";

export type LampState = "applied" | "propagating" | "pending" | "rejected" | "disabled";

/**
 * A signal lamp: a dot beside a word, never colour alone. Green is applied,
 * blue propagating (it pulses once as it appears), hollow pending, red
 * rejected, grey disabled. The one lamp for every state on every page. The
 * dot is drawn by CSS, so the element's text is exactly `children`; the
 * state is `data-lamp`, never `data-state`, which the Dashboard's specs
 * reserve for the rollout badge.
 */
export function Lamp({ state, children, title }: { state: LampState; children: ReactNode; title?: string }) {
  return (
    <span className="lamp" data-lamp={state} title={title}>
      {children}
    </span>
  );
}
