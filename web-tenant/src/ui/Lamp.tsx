import type { ReactNode } from "react";

export type LampState = "applied" | "propagating" | "pending" | "rejected" | "disabled";

/** 状态同时使用形状、文字和颜色；data-lamp 保留现有部署状态合同。 */
export function Lamp({ state, children, title }: { state: LampState; children: ReactNode; title?: string }) {
  return (
    <span className="lamp" data-lamp={state} title={title}>
      <svg className="lamp-icon" viewBox="0 0 16 16" aria-hidden="true">
        {state === "pending" ? (
          <><path d="M8 1.6 14.6 13a1 1 0 0 1-.9 1.5H2.3a1 1 0 0 1-.9-1.5Z" /><path className="lamp-icon-mark" d="M8 5.8v3.7m0 2.3v.2" /></>
        ) : (
          <><circle cx="8" cy="8" r="7" />{state === "applied" ? <path className="lamp-icon-mark" d="m4.6 8 2.2 2.3 4.5-4.6" /> : state === "rejected" ? <path className="lamp-icon-mark" d="m5.5 5.5 5 5m0-5-5 5" /> : <path className="lamp-icon-mark" d={state === "disabled" ? "M5 8h6" : "M8 4.5V8l2.2 1.5"} />}</>
        )}
      </svg>
      {children}
    </span>
  );
}
