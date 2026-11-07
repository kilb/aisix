import type { ReactNode } from "react";

// The portal's copy of the Dashboard's Lamp (P2c-R4).

export type LampState = "applied" | "propagating" | "pending" | "rejected" | "disabled";

/**
 * A state as a signal lamp: a dot before the state's word, never colour
 * alone. The word is in the text colour; only the dot takes the signal
 * colour (green applied, blue propagating, pulsing once; hollow pending;
 * red rejected; grey disabled). The dot is drawn by CSS, so the element's
 * text is exactly `children`.
 */
export function Lamp({ state, children }: { state: LampState; children: ReactNode }) {
  return <span className={`lamp lamp-${state}`}>{children}</span>;
}
