import type { ReactNode } from "react";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { RECORD_PAGE_SIZES, type RecordPageSize } from "../lib/recordTable";
import { Button } from "./Button";
import "./record-table.css";

export type { RecordPageSize } from "../lib/recordTable";

export interface RecordTableToolbarProps {
  search?: { value: string; onChange: (value: string) => void; label: string; placeholder?: string };
  children?: ReactNode;
}

export interface RecordTablePagerProps {
  page: number;
  pageSize: RecordPageSize;
  onPageChange: (page: number) => void;
  onPageSizeChange: (size: RecordPageSize) => void;
  total?: number;
  loadedCount?: number;
  hasNext?: boolean;
  busy?: boolean;
  label?: string;
}

/** 搜索字段由业务页面白名单决定，其他筛选保留原控件和后端语义。 */
export function RecordTableToolbar({ search, children }: RecordTableToolbarProps) {
  return (
    <div className="record-table-toolbar">
      {search && (
        <label className="f record-table-search">
          <span>{search.label}</span>
          <input type="search" autoComplete="off" value={search.value} placeholder={search.placeholder} onChange={(event) => search.onChange(event.target.value)} />
        </label>
      )}
      {children}
    </div>
  );
}

/** 未知总量只展示已加载数量；上下页由调用方持有的游标链决定。 */
export function RecordTablePager({ page, pageSize, onPageChange, onPageSizeChange, total, loadedCount, hasNext, busy = false, label = "记录分页" }: RecordTablePagerProps) {
  const pageCount = total === undefined ? undefined : Math.max(1, Math.ceil(total / pageSize));
  const current = Math.min(pageCount ?? Infinity, Math.max(1, Math.trunc(page) || 1));
  const next = hasNext ?? (pageCount !== undefined && current < pageCount);
  const start = total === 0 ? 0 : (current - 1) * pageSize + 1;
  const end = total === undefined ? undefined : Math.min(current * pageSize, total);
  return (
    <nav className="record-table-pager" aria-label={label} aria-busy={busy || undefined}>
      <p className="record-table-count" role="status" aria-live="polite" aria-atomic="true">
        {total === undefined ? <>第 {current} 页{loadedCount !== undefined && <> · 已加载 {loadedCount.toLocaleString("zh-CN")} 条</>}</> : <>显示 {start}–{end} 条，共 {total.toLocaleString("zh-CN")} 条</>}
      </p>
      <div className="record-table-navigation">
        <label className="record-table-page-size">
          <span>每页条数</span>
          <select aria-label={`${label}每页条数`} value={pageSize} disabled={busy} onChange={(event) => onPageSizeChange(Number(event.target.value) as RecordPageSize)}>
            {RECORD_PAGE_SIZES.map((size) => <option key={size} value={size}>{size}</option>)}
          </select>
        </label>
        <div className="record-table-page-controls">
          <Button size="sm" kind="quiet" iconOnly className="record-table-page-button" aria-label="上一页" title="上一页" disabled={busy || current <= 1} onClick={() => onPageChange(current - 1)}><ChevronLeft aria-hidden="true" /></Button>
          <span className="record-table-page-position" aria-hidden="true">{current}{pageCount !== undefined && <span> / {pageCount}</span>}</span>
          <Button size="sm" kind="quiet" iconOnly className="record-table-page-button" aria-label="下一页" title="下一页" disabled={busy || !next} onClick={() => onPageChange(current + 1)}><ChevronRight aria-hidden="true" /></Button>
        </div>
      </div>
    </nav>
  );
}
