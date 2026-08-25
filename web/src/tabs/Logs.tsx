import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "../lib/api";
import { listOf } from "../lib/fmt";
import type { DocState } from "../lib/useDoc";

function StatusText({ s }: { s: unknown }) {
  const n = Number(s);
  const c = n >= 500 ? "crit" : n >= 400 ? "warn" : "ok";
  return <span style={{ color: `var(--${c})` }}>{String(s ?? "")}</span>;
}

export function Logs({ doc }: { doc: DocState }) {
  const [keyId, setKeyId] = useState("");
  const [limit, setLimit] = useState(100);
  const [rows, setRows] = useState<Record<string, unknown>[] | null>(null);
  const [err, setErr] = useState<string | null>(null);

  /** `key_hash` → 网关侧的 `api_key_id`（指标和日志用的都是它）。 */
  const options = useMemo(() => {
    const byHash = new Map<string, string>();
    for (const k of listOf(doc.res?.api_keys)) {
      if (typeof k.key_hash === "string" && typeof k.id === "string") {
        byHash.set(k.key_hash, k.id);
      }
    }
    return ((doc.doc?.api_keys as Record<string, unknown>[] | undefined) ?? []).map((k) => ({
      id: byHash.get(String(k.key_hash ?? "")) ?? "",
      name: String(k.display_name ?? "（未命名）"),
    }));
  }, [doc.doc, doc.res]);

  const load = useCallback(async () => {
    setErr(null);
    setRows(null);
    try {
      setRows(await api.logs(keyId, limit));
    } catch (e) {
      setErr(e instanceof Error ? e.message : "读取失败");
    }
  }, [keyId, limit]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="panel">
      <h2>逐请求日志</h2>
      <p className="hint">来自网关写进 journald 的结构化访问日志，每个请求一行。</p>
      <div className="grid g2">
        <label className="f">
          <span>筛选密钥</span>
          <select value={keyId} onChange={(e) => setKeyId(e.target.value)}>
            <option value="">全部</option>
            {options.map((o) => (
              <option key={o.id || o.name} value={o.id}>
                {o.name}
              </option>
            ))}
          </select>
        </label>
        <label className="f">
          <span>条数</span>
          <select value={limit} onChange={(e) => setLimit(Number(e.target.value))}>
            <option>50</option>
            <option>100</option>
            <option>200</option>
          </select>
        </label>
      </div>
      <button className="act" onClick={() => void load()}>
        读取
      </button>
      <div className="note warn">
        两个限制照实说：<strong>流式请求</strong>的这一行是在 SSE 开始推送之前写的，
        所以 token 数为空；<strong>journald 有留存上限</strong>，不是无限历史。
        要长期留存需要配 observability exporter。
      </div>
      <div style={{ marginTop: 14 }}>
        {err ? (
          <div className="note crit">{err}</div>
        ) : rows === null ? (
          <p className="hint">读取中…</p>
        ) : rows.length === 0 ? (
          <p className="hint">没有匹配的记录。</p>
        ) : (
          <div className="scroll">
            <table>
              <thead>
                <tr>
                  <th>时间</th>
                  <th>路径</th>
                  <th className="r">状态</th>
                  <th className="r">耗时</th>
                  <th>模型</th>
                  <th className="r">Token</th>
                  <th>request id</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((x, i) => {
                  const tokens = String(x.total_tokens ?? "");
                  return (
                    <tr key={i}>
                      <td className="num" style={{ fontSize: 11 }}>
                        {String(x._ts ?? "").replace("T", " ").slice(0, 19)}
                      </td>
                      <td className="num" style={{ fontSize: 12 }}>
                        {String(x.path ?? "")}
                      </td>
                      <td className="r num">
                        <StatusText s={x.status} />
                      </td>
                      <td className="r num">{String(x.latency_ms ?? "")}ms</td>
                      <td>{String(x.model ?? "")}</td>
                      {/* 流式请求这一行没有 token 数，显示 "—" 而不是 0：
                          0 会被读成「这个请求没消耗 token」。 */}
                      <td className="r num">{tokens && tokens !== "0" ? tokens : "—"}</td>
                      <td className="num" style={{ fontSize: 11, color: "var(--muted)" }}>
                        {String(x.request_id ?? "").slice(0, 8)}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}
