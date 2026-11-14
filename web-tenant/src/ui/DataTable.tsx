// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import type { ReactNode } from "react";
import { cn } from "../lib/cn";
import "./record-table.css";

/**
 * 表头44px、数据行至少48px，行内操作保留44px命中区。
 * 名称与说明可换行，数字和操作各自对齐；必要的横向滚动只发生在表格区域。
 * 分页记录自然撑开页面；scroll 保留旧调用接口，不限制内容高度。
 */
export function DataTable({
  label,
  compact = false,
  scroll = false,
  className,
  tableClassName,
  children,
}: {
  label?: string;
  compact?: boolean;
  scroll?: boolean;
  className?: string;
  tableClassName?: string;
  children: ReactNode;
}) {
  return (
    <div className={cn("dt-wrap", scroll && "dt-scroll", className)} role="region" aria-label={label ? `${label}表格滚动区域` : "表格滚动区域"} tabIndex={0}>
      <table className={cn("dt", compact && "dt-compact", tableClassName)} aria-label={label}>
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
    <div className="loading" role="status">
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
