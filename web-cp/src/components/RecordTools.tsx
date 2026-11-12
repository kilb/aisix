import { useEffect, useRef, useState, type ReactNode } from "react";
import { cursorPageAfterLoad } from "../lib/operationalRecords";
import type { RecordPageSize } from "../lib/recordTable";
import { RecordTableToolbar, RecordTablePager } from "../ui/RecordTable";

interface RecordState {
  query: string;
  setQuery: (query: string) => void;
  page: number;
  setPage: (page: number) => void;
  pageSize: RecordPageSize;
  setPageSize: (size: RecordPageSize) => void;
  total: number;
  filteredCount: number;
  pageCount: number;
}

export function RecordTools({ table, label, loaded = false, children }: { table: RecordState; label: string; loaded?: boolean; children?: ReactNode }) {
  return (
    <>
      <RecordTableToolbar search={{
        value: table.query,
        onChange: table.setQuery,
        label: `${label}${loaded ? "：在已加载记录中搜索" : "搜索"}`,
        placeholder: loaded ? "在已加载记录中搜索" : "搜索名称或公开编号",
      }}>
        {children}
      </RecordTableToolbar>
      {table.filteredCount === 0 && <p className="hint" role="status">{loaded ? "没有匹配的已加载记录；可继续读取更早记录。" : "没有匹配的记录。"}</p>}
    </>
  );
}

// 游标只用于继续读取；上页从已加载的记录读取，绝不把已加载数量当作服务端总数。
export function RecordPager({ table, label, hasMore, onMore, busy = false, resetKey }: { table: RecordState; label: string; hasMore?: boolean; onMore?: () => Promise<void>; busy?: boolean; resetKey?: string }) {
  const [waiting, setWaiting] = useState(false);
  const reading = useRef(false);
  const scope = JSON.stringify([table.query, table.pageSize, resetKey]);
  const currentScope = useRef(scope);
  currentScope.current = scope;
  const [requested, setRequested] = useState<{ page: number; scope: string } | null>(null);
  useEffect(() => {
    if (requested === null) return;
    if (requested.scope === scope) table.setPage(Math.min(requested.page, table.pageCount));
    setRequested(null);
  }, [requested, scope, table.pageCount, table.setPage]);
  async function move(page: number) {
    if (page <= table.pageCount || !onMore || !hasMore) {
      table.setPage(page);
      return;
    }
    if (reading.current || busy) return;
    reading.current = true;
    setWaiting(true);
    try {
      await onMore();
      if (currentScope.current === scope) setRequested({ page: cursorPageAfterLoad(table.page, table.pageSize, table.filteredCount, page), scope });
    } finally {
      reading.current = false;
      setWaiting(false);
    }
  }
  const cursor = onMore !== undefined;
  return (
    <RecordTablePager
      page={table.page}
      pageSize={table.pageSize}
      onPageChange={(page) => void move(page)}
      onPageSizeChange={table.setPageSize}
      {...(cursor ? { loadedCount: table.total, hasNext: table.page < table.pageCount || hasMore === true } : { total: table.filteredCount })}
      busy={busy || waiting}
      label={label}
    />
  );
}
