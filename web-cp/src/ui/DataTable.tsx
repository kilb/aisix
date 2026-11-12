// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import type { ReactNode } from "react";

/**
 * 表头44px、普通行至少56px、compact至少48px，容纳44px操作控件；
 * 灰色 mono 标签与中性 hover，无斑马条纹。
 * Numbers sit right-aligned in tabular figures (`className="num"` on the
 * cell and its header), as do a row's actions (`"right"`). The children
 * are its `thead` and `tbody`. It scrolls sideways inside its own box when
 * the screen is narrower than its columns; `scroll` also gives a long list
 * its own vertical scroll area, so the header stays in view.
 */
export function DataTable({
  label,
  compact = false,
  scroll = false,
  children,
}: {
  label?: string;
  compact?: boolean;
  scroll?: boolean;
  children: ReactNode;
}) {
  return (
    <div className={scroll ? "dt-wrap dt-scroll" : "dt-wrap"}>
      <table className={compact ? "dt dt-compact" : "dt"} aria-label={label}>
        {children}
      </table>
    </div>
  );
}

const WIDTHS = [
  [40, 22, 12],
  [52, 18, 8],
  [32, 22, 12],
];

/**
 * A list on its way: three rows of skeleton, purely visual, and the
 * `载入中…` status text, visually hidden, that assistive technology and the
 * specs read.
 */
export function Loading() {
  return (
    <div className="loading">
      <div className="skeleton" aria-hidden="true">
        {WIDTHS.map((row, i) => (
          <span key={i}>
            {row.map((w, j) => (
              <i key={j} style={{ width: `${w}%` }} />
            ))}
          </span>
        ))}
      </div>
      <p className="vh">载入中…</p>
    </div>
  );
}
