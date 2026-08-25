import { useEffect, useMemo, useState } from "react";
import * as api from "../lib/api";
import { fmtCompact, fmtInt, fmtUsd, listOf } from "../lib/fmt";
import { Chart, type Series } from "../components/Chart";
import type { DocState } from "../lib/useDoc";

interface Row {
  req: number;
  tok: number;
  spend: number;
}

const ZERO: Row = { req: 0, tok: 0, spend: 0 };

/** 把三个 promTop 结果按同一个分组键合并。 */
async function group(
  queries: [string, string, string],
  keyOf: (l: Record<string, string>) => string,
): Promise<Map<string, Row>> {
  const [req, tok, spend] = await Promise.all(queries.map((q) => api.promTop(q)));
  const by = new Map<string, Row>();
  const put = (rows: api.PromSample[] | undefined, f: keyof Row) => {
    for (const r of rows ?? []) {
      const k = keyOf(r.l);
      by.set(k, { ...(by.get(k) ?? ZERO), [f]: r.v });
    }
  };
  put(req, "req");
  put(tok, "tok");
  put(spend, "spend");
  return by;
}

function sorted(by: Map<string, Row>): [string, Row][] {
  return [...by.entries()].sort((a, b) => b[1].tok - a[1].tok);
}

export function Usage({ doc }: { doc: DocState }) {
  const [keyRows, setKeyRows] = useState<[string, Row][] | null>(null);
  const [keyErr, setKeyErr] = useState<string | null>(null);
  const [modelRows, setModelRows] = useState<[string, Row][] | null>(null);
  const [modelErr, setModelErr] = useState<string | null>(null);
  const [tokByModel, setTokByModel] = useState<Series[] | null>(null);
  const [tokByKey, setTokByKey] = useState<Series[] | null>(null);

  /**
   * `api_key_id` → 显示名，两跳。
   *
   * 指标里只有 id；管理 API 有 id 和 `key_hash`，但**没有名字** —— 文件模式
   * 下 `display_name` 是「文件侧身份」，校验前就被剥离，所以名字只存在于
   * resources.yaml。`key_hash` 是两边都有的字段，用它把两侧接起来。
   */
  const keyName = useMemo(() => {
    const hashToName = new Map<string, string>();
    for (const k of (doc.doc?.api_keys as Record<string, unknown>[] | undefined) ?? []) {
      const h = k.key_hash;
      if (typeof h === "string") hashToName.set(h, String(k.display_name ?? ""));
    }
    const out = new Map<string, string>();
    for (const k of listOf(doc.res?.api_keys)) {
      const id = k.id;
      const h = k.key_hash;
      if (typeof id === "string" && typeof h === "string") {
        const name = hashToName.get(h);
        if (name) out.set(id, name);
      }
    }
    return out;
  }, [doc.doc, doc.res]);

  useEffect(() => {
    let live = true;
    const fail = (e: unknown) => (e instanceof Error ? e.message : "读取失败");
    void (async () => {
      try {
        const by = await group(
          [
            "sum by (api_key_id) (aisix_llm_requests_total)",
            "sum by (api_key_id) (aisix_llm_total_tokens_total)",
            "sum by (api_key_id) (aisix_llm_spend_micro_usd_total)",
          ],
          (l) => l.api_key_id ?? "(未知)",
        );
        if (live) setKeyRows(sorted(by));
      } catch (e) {
        if (live) setKeyErr(fail(e));
      }
      try {
        // 组合键用真正的元组编码，不用不可见分隔符：旧界面在这里塞了两个
        // NUL 字节，结果整个文件被 grep 当成二进制。
        const by = await group(
          [
            "sum by (model, provider) (aisix_llm_requests_total)",
            "sum by (model, provider) (aisix_llm_total_tokens_total)",
            "sum by (model, provider) (aisix_llm_spend_micro_usd_total)",
          ],
          (l) => JSON.stringify([l.model ?? "?", l.provider ?? "?"]),
        );
        if (live) setModelRows(sorted(by));
      } catch (e) {
        if (live) setModelErr(fail(e));
      }
      for (const [q, set] of [
        ["sum by (model) (rate(aisix_llm_total_tokens_total[10m]))", setTokByModel],
        ["sum by (api_key_id) (rate(aisix_llm_total_tokens_total[10m]))", setTokByKey],
      ] as const) {
        try {
          const rows = await api.promRange(q, 24);
          if (live) set(rows.map((r) => ({ metric: r.l, values: r.points })));
        } catch {
          if (live) set([]);
        }
      }
    })();
    return () => {
      live = false;
    };
  }, []);

  const maxTok = Math.max(1, ...(keyRows ?? []).map(([, v]) => v.tok));

  return (
    <>
      <div className="panel">
        <h2>按调用方密钥</h2>
        <p className="hint">
          指标只带 <code>api_key_id</code>，名称由控制台对照配置补上。
        </p>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>密钥</th>
                <th className="right">请求</th>
                <th className="right">Token</th>
                <th className="right">花费</th>
              </tr>
            </thead>
            <tbody>
              {keyErr ? (
                <tr>
                  <td colSpan={4}>读取失败：{keyErr}</td>
                </tr>
              ) : keyRows === null ? (
                <tr>
                  <td colSpan={4}>读取中…</td>
                </tr>
              ) : keyRows.length === 0 ? (
                <tr>
                  <td colSpan={4} style={{ color: "var(--ink-3)" }}>
                    尚无用量数据。
                  </td>
                </tr>
              ) : (
                keyRows.map(([id, v]) => (
                  <tr key={id}>
                    <td>
                      <strong>{keyName.get(id) ?? "（未命名）"}</strong>
                      <div className="bar">
                        <i style={{ width: `${((v.tok / maxTok) * 100).toFixed(1)}%` }} />
                      </div>
                      <div
                        className="foot num"
                        style={{ fontSize: 11, color: "var(--ink-3)" }}
                      >
                        {id}
                      </div>
                    </td>
                    <td className="right num">{fmtInt(v.req)}</td>
                    <td className="right num">{fmtCompact(v.tok)}</td>
                    <td className="right num">{fmtUsd(v.spend)}</td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="panel">
        <h2>按模型</h2>
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>模型</th>
                <th>供应商</th>
                <th className="right">请求</th>
                <th className="right">Token</th>
                <th className="right">花费</th>
              </tr>
            </thead>
            <tbody>
              {modelErr ? (
                <tr>
                  <td colSpan={5}>读取失败：{modelErr}</td>
                </tr>
              ) : modelRows === null ? (
                <tr>
                  <td colSpan={5}>读取中…</td>
                </tr>
              ) : modelRows.length === 0 ? (
                <tr>
                  <td colSpan={5} style={{ color: "var(--ink-3)" }}>
                    尚无用量数据。
                  </td>
                </tr>
              ) : (
                modelRows.map(([k, v]) => {
                  const [m, p] = JSON.parse(k) as [string, string];
                  return (
                    <tr key={k}>
                      <td>
                        <strong>{m}</strong>
                      </td>
                      <td>{p}</td>
                      <td className="right num">{fmtInt(v.req)}</td>
                      <td className="right num">{fmtCompact(v.tok)}</td>
                      <td className="right num">{fmtUsd(v.spend)}</td>
                    </tr>
                  );
                })
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="panel">
        <h2>近 24 小时 Token 速率（按模型）</h2>
        {tokByModel === null ? (
          <p className="hint">读取中…</p>
        ) : tokByModel.length ? (
          <Chart series={tokByModel} fmt={(v) => `${v.toFixed(1)} tok/s`} />
        ) : (
          <p className="hint">该时段没有 token 流量。</p>
        )}
      </div>

      <div className="panel">
        <h2>近 24 小时 Token 速率（按调用方密钥）</h2>
        <p className="hint">
          图例是 <code>api_key_id</code>：指标里只有 id，曲线上无法替换成名称。
        </p>
        {tokByKey === null ? (
          <p className="hint">读取中…</p>
        ) : tokByKey.length ? (
          <Chart series={tokByKey} fmt={(v) => `${v.toFixed(1)} tok/s`} />
        ) : (
          <p className="hint">该时段没有 token 流量。</p>
        )}
      </div>
    </>
  );
}
