import type { ReactNode } from "react";

/**
 * A table: a sticky 36px header in 12px muted labels, 44px rows (36px
 * compact) that hover in Concrete, numbers right-aligned in tabular figures
 * (`className="num"` or `"right"` on the cell), no zebra stripes. The
 * children are its `thead` and `tbody`. `scroll` gives a long list its own
 * scroll area, so the header stays in view.
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
 * `载入中…` text that assistive technology and the specs read.
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
      <p>载入中…</p>
    </div>
  );
}
