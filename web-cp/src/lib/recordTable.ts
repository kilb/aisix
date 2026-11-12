import { useReducer } from "react";

export type RecordPageSize = 10 | 25 | 50;
export const RECORD_PAGE_SIZES: readonly RecordPageSize[] = [10, 25, 50];

export interface RecordTableState {
  query: string;
  page: number;
  pageSize: RecordPageSize;
  resetKey?: string;
}

export type RecordTableAction =
  | { type: "query"; query: string }
  | { type: "page"; page: number }
  | { type: "pageSize"; pageSize: RecordPageSize }
  | { type: "sync"; page: number; resetKey?: string };

export function recordTableReducer(state: RecordTableState, action: RecordTableAction): RecordTableState {
  switch (action.type) {
    case "query": return { ...state, query: action.query, page: 1 };
    case "page": return { ...state, page: action.page };
    case "pageSize": return { ...state, pageSize: action.pageSize, page: 1 };
    case "sync": return { ...state, page: action.page, resetKey: action.resetKey };
  }
}

/** 只搜索调用方显式提供的公开字段，绝不序列化整条资源。 */
export function filterRecords<T>(items: readonly T[], query: string, searchText?: (row: T) => string | readonly string[]): T[] {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle || !searchText) return [...items];
  return items.filter((row) => {
    const value = searchText(row);
    const fields = typeof value === "string" ? [value] : value;
    return fields.some((field) => field.toLocaleLowerCase().includes(needle));
  });
}

/** 父记录先分页；它的展开详情仍由调用方与该记录一起渲染。 */
export function paginateRecords<T>(items: readonly T[], page: number, pageSize: RecordPageSize): {
  rows: T[]; total: number; page: number; pageCount: number; start: number; end: number;
} {
  const total = items.length;
  const pageCount = Math.max(1, Math.ceil(total / pageSize));
  const current = Math.min(pageCount, Math.max(1, Math.trunc(page) || 1));
  const offset = (current - 1) * pageSize;
  return { rows: items.slice(offset, offset + pageSize), total, page: current, pageCount, start: total === 0 ? 0 : offset + 1, end: Math.min(offset + pageSize, total) };
}

export function useRecordTable<T>({ items, searchText, resetKey, initialPageSize = 25 }: {
  items: readonly T[];
  searchText?: (row: T) => string | readonly string[];
  resetKey?: string;
  initialPageSize?: RecordPageSize;
}) {
  const [state, dispatch] = useReducer(recordTableReducer, { query: "", page: 1, pageSize: initialPageSize, resetKey });
  const matching = filterRecords(items, state.query, searchText);
  const result = paginateRecords(matching, state.resetKey === resetKey ? state.page : 1, state.pageSize);
  // 同步条件变化和末页删除，避免先显示旧页或以后恢复已越界的页码。
  if (state.resetKey !== resetKey || state.page !== result.page) {
    dispatch({ type: "sync", page: result.page, resetKey });
  }
  return {
    ...result,
    total: items.length,
    filteredCount: matching.length,
    query: state.query,
    pageSize: state.pageSize,
    setQuery: (query: string) => dispatch({ type: "query", query }),
    setPage: (page: number) => dispatch({ type: "page", page }),
    setPageSize: (pageSize: RecordPageSize) => dispatch({ type: "pageSize", pageSize }),
  };
}
