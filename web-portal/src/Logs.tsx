import { useCallback, useEffect, useState } from "react";
import * as api from "./lib/api";

/**
 * 本人的调用记录。
 *
 * 过滤在服务端：`user_id` 来自会话，前端给不出任何能改变结果的参数。这里连
 * 「查谁的」这个概念都不存在。
 */
export function Logs() {
  const [rows, setRows] = useState<api.LogRow[] | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [limit, setLimit] = useState(50);

  const load = useCallback(async () => {
    try {
      const r = await api.logs(limit);
      setRows(r.rows);
      setNote(r.note);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : "读取失败");
      setRows([]);
    }
  }, [limit]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <section className="panel">
      <h2>调用记录</h2>
      <div className="row">
        <label className="f">
          <span>条数</span>
          <select value={limit} onChange={(e) => setLimit(Number(e.target.value))}>
            {[20, 50, 100, 200].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        <button className="ghost" onClick={() => void load()}>
          刷新
        </button>
      </div>

      {err && <div className="note crit">{err}</div>}
      {note && <div className="note warn">{note}</div>}

      {rows === null ? (
        <p className="hint">读取中…</p>
      ) : rows.length === 0 ? (
        <p className="hint">还没有调用记录。</p>
      ) : (
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>时间</th>
                <th>模型</th>
                <th>路径</th>
                <th className="right">状态</th>
                <th className="right">耗时</th>
                <th className="right">Token</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r, i) => (
                <tr key={`${r.ts}-${i}`}>
                  <td className="num" style={{ fontSize: 11 }}>
                    {r.ts.replace("T", " ").slice(0, 19)}
                  </td>
                  <td>{r.model ?? ""}</td>
                  <td className="num" style={{ fontSize: 11 }}>
                    {r.path ?? ""}
                  </td>
                  <td className="right num">{r.status ?? ""}</td>
                  <td className="right num">{r.latency_ms != null ? `${r.latency_ms} ms` : ""}</td>
                  {/* 流式请求这一行在推流开始前写下，token 数为空 —— 上面的
                      提示会说明，这里不要用 0 冒充。 */}
                  <td className="right num">{r.total_tokens ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
