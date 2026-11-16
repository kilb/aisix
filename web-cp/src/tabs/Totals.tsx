import { useEffect, useState } from "react";
import type { TabProps } from "../App";
import { Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader } from "../components/page";
import { fetchApiMetrics, metricsRangeError, tokenTrendValue, type ApiMetricsResponse } from "../lib/apiMetrics";
import { formatCount } from "../lib/chartFormat";
import { errorText } from "../lib/fmt";
import { fmtMicroCny } from "../lib/money";
import { beijingInputValue, fmtBeijing, parseBeijingInput } from "../lib/time";
import { fetchCostTotals, type CostTotals } from "../lib/totals";
import { useHashQuery } from "../lib/useHashQuery";
import { Button } from "../ui/Button";
import "./totals.css";

interface Reading<T> { data: T | null; error: string | null }
interface TotalsAnswer {
  key: string;
  usage: Reading<ApiMetricsResponse>;
  cost: Reading<CostTotals>;
}

/** 输入与成本各自保留正式聚合口径；同窗独立查询，不宣称一致快照。 */
export function Totals(_props: TabProps) {
  const [query, update] = useHashQuery();
  const [fallback] = useState(() => {
    const to = new Date();
    return { from: new Date(to.getTime() - 86_400_000).toISOString(), to: to.toISOString() };
  });
  const from = query.get("from") ?? fallback.from;
  const to = query.get("to") ?? fallback.to;
  const [revision, setRevision] = useState(0);
  const [answer, setAnswer] = useState<TotalsAnswer>({ key: "", usage: { data: null, error: null }, cost: { data: null, error: null } });
  const key = JSON.stringify([from, to, revision]);
  const rangeError = metricsRangeError(from, to)?.replace("；下方历史明细仍按原范围显示", "");
  useEffect(() => {
    if (rangeError) return;
    let current = true;
    setAnswer({ key, usage: { data: null, error: null }, cost: { data: null, error: null } });
    void Promise.allSettled([fetchApiMetrics("admin", { from, to }), fetchCostTotals(from, to)]).then(([usage, cost]) => {
      if (!current) return;
      setAnswer({
        key,
        usage: usage.status === "fulfilled" ? { data: usage.value, error: null } : { data: null, error: errorText(usage.reason) },
        cost: cost.status === "fulfilled" ? { data: cost.value, error: null } : { data: null, error: errorText(cost.reason) },
      });
    });
    return () => { current = false; };
  }, [key, from, to, rangeError]);

  const usage = !rangeError && answer.key === key ? answer.usage.data : null;
  const cost = !rangeError && answer.key === key ? answer.cost.data : null;
  const usageError = rangeError ?? (answer.key === key ? answer.usage.error : null);
  const costError = rangeError ?? (answer.key === key ? answer.cost.error : null);
  const loading = !rangeError && (!usage && !usageError || !cost && !costError);
  const retry = () => setRevision(value => value + 1);
  const dateValue = (value: string) => Number.isFinite(Date.parse(value)) ? beijingInputValue(new Date(value)) : value;
  const changeDate = (name: string, value: string) => update({ from, to, [name]: parseBeijingInput(value) ?? value });
  function selectRange(days: number) {
    const end = new Date();
    update({ from: new Date(end.getTime() - days * 86_400_000).toISOString(), to: end.toISOString() });
  }
  const tokens = usage?.totals.tokens;
  return <div className="totals-page">
    <PageHeader title="总输入与总成本" description="全站统计 · 选择时间窗口，分别查看已记录输入与已知上游成本。" />
    <section className="totals-scope" aria-label="统计时间范围">
      <div className="totals-dates">
        <label className="f"><span>开始时间（UTC+8）</span><input type="datetime-local" step="1" value={dateValue(from)} onChange={event => changeDate("from", event.target.value)} /></label>
        <label className="f"><span>结束时间（不含）</span><input type="datetime-local" step="1" value={dateValue(to)} onChange={event => changeDate("to", event.target.value)} /></label>
      </div>
      <div className="totals-actions">
        <div className="totals-presets" aria-label="快捷时间范围">
          <Button size="sm" kind="quiet" onClick={() => selectRange(1)}>最近 24 小时</Button>
          <Button size="sm" kind="quiet" onClick={() => selectRange(7)}>最近 7 天</Button>
          <Button size="sm" kind="quiet" onClick={() => selectRange(30)}>最近 30 天</Button>
        </div>
        <Button size="sm" onClick={retry} busy={loading} busyLabel="正在刷新统计" disabled={Boolean(rangeError)}>刷新</Button>
      </div>
    </section>
    {rangeError && <p className="totals-error" role="alert">{rangeError}</p>}
    <p className="totals-window">{fmtBeijing(from)} — {fmtBeijing(to)}（UTC+8，结束时间不含），最多查询 92 天。</p>
    {loading && <span role="status" className="sr-only">正在加载总量统计</span>}
    <div className="totals-metrics">
      <Card className="totals-metric" aria-label="总输入统计" aria-busy={!usage && !usageError}>
        <CardHeader title="总输入 token" description="已记录用量的输入合计，含已计费重试" />
        {usage ? <>
          <Metric size="figure" className="totals-value" >{formatCount(tokenTrendValue(tokens, "input_tokens"))}</Metric>
          <dl className="totals-facts">
            <div><dt>有用量记录的尝试</dt><dd>{formatCount(tokens?.known_usage_attempts)}</dd></div>
            <div><dt>缺少用量记录的尝试</dt><dd>{formatCount(tokens?.unknown_usage_attempts)}</dd></div>
          </dl>
          {tokens?.unknown_usage_attempts !== null && tokens?.unknown_usage_attempts !== undefined && tokens.unknown_usage_attempts > 0 && <p className="totals-warning" role="status">存在未记录用量，当前仅为已记录输入小计。</p>}
        </> : usageError ? <CardMessage onRetry={rangeError ? undefined : retry}>{usageError}</CardMessage> : <MetricSkeleton className="totals-skeleton" />}
        <p className="totals-note">原始输入 + 额外缓存读取 + 额外缓存创建。输入内缓存子集已包含在原始输入中，不重复相加；缺少用量不按零计算。</p>
      </Card>
      <Card className="totals-metric" aria-label="总成本统计" aria-busy={!cost && !costError}>
        <CardHeader title="已计费记录的已知成本" description="历史上游成本，人民币" />
        {cost ? <>
          <Metric size="figure" className="totals-value">{cost.knownCostMicroCny === null ? "—" : fmtMicroCny(cost.knownCostMicroCny)}</Metric>
          <dl className="totals-facts">
            <div><dt>成本已知的记录</dt><dd>{formatCount(cost.rowsWithCost)}</dd></div>
            <div><dt>成本缺失的记录</dt><dd>{formatCount(cost.rowsWithoutCost)}</dd></div>
          </dl>
          {cost.rowsWithoutCost !== null && cost.rowsWithoutCost > 0n ? <p className="totals-warning" role="status">{formatCount(cost.rowsWithoutCost)} 笔记录缺失成本，当前仅为已知成本小计，不代表完整总成本。</p> : cost.knownCostMicroCny === null ? <p className="totals-warning" role="status">金额或覆盖数据不完整，暂不显示总额。</p> : cost.rowsWithCost === 0n ? <p className="totals-note">此窗口没有已收费或已重定价记录。</p> : null}
        </> : costError ? <CardMessage onRetry={rangeError ? undefined : retry}>{costError}</CardMessage> : <MetricSkeleton className="totals-skeleton" />}
        <p className="totals-note">仅含已收费或存在费用调整的记录。未定价、豁免及无应收费用的调用不在此成本范围内；未知成本不按零计算。</p>
      </Card>
    </div>
    <section className="totals-method" aria-label="统计口径">
      <h2>统计口径</h2>
      <p>输入覆盖可见的最终记录及已计费重试；成本覆盖已收费或已重定价记录，两项统计的人口不同。成本来自当笔记录，不使用当前费率重新估算。</p>
      <p>两项统计使用相同时间窗口，分别独立查询；记录持续写入或重定价时，结果可能存在短暂差异。成本仅在运维控制台提供，不向租户展示。</p>
    </section>
  </div>;
}
