export interface QueryResult<T> {
  key: string;
  rows: T[] | null;
  next: number | null;
  error: string | null;
}

/** 控件切换的同一次 render 即拒绝旧查询的金额、错误和续读游标。 */
export function currentQueryResult<T>(result: QueryResult<T> | null, key: string): QueryResult<T> | null {
  return result?.key === key ? result : null;
}
