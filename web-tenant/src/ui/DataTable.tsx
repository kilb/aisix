import type { ReactNode } from "react";

// The portal's copy of the Dashboard's DataTable (P2c-R4).

/**
 * A list as a table: 36px header with muted 12px labels, 44px rows (56px
 * with a note line), hover in Concrete, numbers right-aligned in tabular
 * figures (`className="r"` on their cells), no zebra stripes. It bleeds to
 * the edges of the list panel it sits in, and scrolls sideways inside its
 * own box when a phone is narrower than its columns. `children` are the
 * `thead` and `tbody`.
 */
export function DataTable({ label, children }: { label?: string; children: ReactNode }) {
  return (
    <div className="scroll">
      <table className="tbl" aria-label={label}>
        {children}
      </table>
    </div>
  );
}

/**
 * What a list shows while it loads: three skeleton rows, purely visual, and
 * the words `载入中…` for assistive technology.
 */
export function ListLoading() {
  return (
    <>
      <div className="skeleton" aria-hidden="true">
        {[0, 1, 2].map((i) => (
          <div className="skeleton-row" key={i}>
            <span />
            <span />
            <span />
          </div>
        ))}
      </div>
      <p className="vh">载入中…</p>
    </>
  );
}
