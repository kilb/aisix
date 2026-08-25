import { useEffect, useMemo, useState } from "react";
import * as api from "../lib/api";
import { fmtCompact, fmtUsd, listOf, resError } from "../lib/fmt";
import { Chart, type Series } from "../components/Chart";
import { Reading } from "../components/Reading";
import { Balance } from "../components/Balance";
import type { DocState } from "../lib/useDoc";
import type { TabId } from "../App";

type Totals = { req: string; tok: string; spend: string };

export function Overview({ doc }: { doc: DocState; onGoto: (t: TabId) => void }) {
  // "…" = 还在读，"—" = 读失败。两者必须能分开：把失败显示成 0 会让人
  // 以为网关没流量。
  const [totals, setTotals] = useState<Totals>({ req: "…", tok: "…", spend: "…" });
  const [series, setSeries] = useState<Series[] | null>(null);
  /** 上限窗口内的花费。null = 还没读到。 */
  const [windowSpend, setWindowSpend] = useState<number | null>(null);
  const [chartError, setChartError] = useState<string | null>(null);

  /**
   * 配置里的花费上限。取窗口相同的那些相加 —— 多条策略同时生效，任一超限
   * 即拒，所以「离上限还有多远」看的是它们的总额在同一窗口下的位置。
   *
   * 只认 day/hour/minute：second 窗口下花费上限不生效（网关会报出来），
   * 拿它画量程是在展示一个不存在的约束。
   */
  const ceiling = useMemo(() => {
    const rows =
      (doc.doc?.rate_limit_policies as Record<string, unknown>[] | undefined) ?? [];
    for (const win of ["day", "hour", "minute"]) {
      const total = rows
        .filter((p) => p.window === win && p.max_spend_micro_usd != null)
        .reduce((n, p) => n + Number(p.max_spend_micro_usd), 0);
      if (total > 0) return { total, window: win };
    }
    return null;
  }, [doc.doc]);

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
      // 窗口内的花费。**不能**拿累计总量跟日上限比 —— 那个数是自开始抓取
      // 起累加的，跟「今天花了多少」没有关系。`increase` 才是这段时间的增量。
      {
        // 没配上限时按天读。给不出「离上限还有多远」，「今天花了多少」照样
        // 是运维要的数 —— 读到了却不显示，是白扔一条真实读数。
        const span =
          ceiling?.window === "hour" ? "1h" : ceiling?.window === "minute" ? "1m" : "1d";
        try {
          const r = await api.promTop(
            `sum(increase(aisix_llm_spend_micro_usd_total[${span}]))`,
          );
          if (live) setWindowSpend(r.length ? (r[0]?.v ?? 0) : 0);
        } catch {
          if (live) setWindowSpend(null);
        }
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
  }, [ceiling]);

  const models = listOf(doc.res?.models);
  const keys = listOf(doc.res?.api_keys);
  const pks = listOf(doc.res?.provider_keys);
  const inventoryDown =
    resError(doc.res?.models) ??
    resError(doc.res?.provider_keys) ??
    resError(doc.res?.api_keys);

  return (
    <>
      <div className="panel">
        <h2>网关读数</h2>
        <p className="hint">累计值来自 Prometheus，自开始抓取起算。</p>
        <div className="entries">
          <Reading label="请求总数" value={totals.req} foot="经 LLM 端点" />
          <Reading label="Token 总量" value={totals.tok} foot="输入 + 输出（含缓存）" />
            {/* 花费归表盘所有，侧边不再重复。读不到就说读不到 ——把 0 印成大号数字，跟「真的配了
                0 条」在屏幕上一模一样，而这两件事要采取的动作完全相反。 */}
            <Reading
              label="已配置"
              value={
                doc.res === null || inventoryDown
                  ? "—"
                  : `${models.length} / ${pks.length} / ${keys.length}`
              }
              foot={
                doc.res === null || inventoryDown
                  ? "网关不可达，读不到清单"
                  : "模型 / 供应商 / 调用方密钥"
              }
            />
        </div>
      </div>

      <div className="panel">
        <h2>结账</h2>
        <p className="hint">花费对着配置里的上限收口。上限来自限流策略，不是估算。</p>
        <Balance
          spendMicro={windowSpend}
          ceilingMicro={ceiling?.total ?? null}
          window={ceiling?.window ?? "day"}
        />
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
