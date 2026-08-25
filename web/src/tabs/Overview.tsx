import { useEffect, useState } from "react";
import * as api from "../lib/api";
import { fmtCompact, fmtUsd, listOf } from "../lib/fmt";
import { Chart, type Series } from "../components/Chart";
import type { DocState } from "../lib/useDoc";
import type { TabId } from "../App";

type Totals = { req: string; tok: string; spend: string };

export function Overview({ doc }: { doc: DocState; onGoto: (t: TabId) => void }) {
  // "…" = 还在读，"—" = 读失败。两者必须能分开：把失败显示成 0 会让人
  // 以为网关没流量。
  const [totals, setTotals] = useState<Totals>({ req: "…", tok: "…", spend: "…" });
  const [series, setSeries] = useState<Series[] | null>(null);
  const [chartError, setChartError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const [req, tok, spend] = await Promise.all([
          api.promTop("sum(aisix_llm_requests_total)"),
          api.promTop("sum(aisix_llm_total_tokens_total)"),
          api.promTop("sum(aisix_llm_spend_micro_usd_total)"),
        ]);
        if (!live) return;
        setTotals({
          req: req.length ? fmtCompact(req[0]?.v) : "0",
          tok: tok.length ? fmtCompact(tok[0]?.v) : "0",
          spend: spend.length ? fmtUsd(spend[0]?.v) : "$0.00",
        });
      } catch {
        if (live) setTotals({ req: "—", tok: "—", spend: "—" });
      }
      try {
        const rows = await api.promRange(
          "sum by (endpoint) (rate(aisix_llm_requests_total[10m]))",
          24,
        );
        if (live) setSeries(rows.map((r) => ({ metric: r.l, values: r.points })));
      } catch (e) {
        if (live) setChartError(e instanceof Error ? e.message : "读取失败");
      }
    })();
    return () => {
      live = false;
    };
  }, []);

  const models = listOf(doc.res?.models);
  const keys = listOf(doc.res?.api_keys);
  const pks = listOf(doc.res?.provider_keys);

  return (
    <>
      <div className="panel">
        <h2>网关读数</h2>
        <p className="hint">累计值来自 Prometheus，自开始抓取起算。</p>
        <div className="grid g3">
          <div className="read">
            <div className="lab">请求总数</div>
            <div className="val num">{totals.req}</div>
            <div className="foot">经 LLM 端点</div>
          </div>
          <div className="read">
            <div className="lab">Token 总量</div>
            <div className="val num">{totals.tok}</div>
            <div className="foot">输入 + 输出（含缓存）</div>
          </div>
          <div className="read">
            <div className="lab">累计花费</div>
            <div className="val num">{totals.spend}</div>
            <div className="foot">按模型定价折算</div>
          </div>
          <div className="read">
            <div className="lab">已配置</div>
            <div className="val num">
              {models.length} / {pks.length} / {keys.length}
            </div>
            <div className="foot">模型 / 供应商 / 调用方密钥</div>
          </div>
        </div>
      </div>

      <div className="panel">
        <h2>近 24 小时请求</h2>
        <p className="hint">按端点分。没有曲线说明该时段没有流量。</p>
        {chartError ? (
          <div className="note crit">曲线读取失败：{chartError}</div>
        ) : series === null ? (
          <p className="hint">读取中…</p>
        ) : series.length ? (
          <Chart series={series} fmt={(v) => `${v.toFixed(3)} req/s`} />
        ) : (
          <p className="hint">该时段没有 LLM 请求。</p>
        )}
      </div>
    </>
  );
}
