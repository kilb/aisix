import { useEffect, useState } from "react";
import type { TabProps } from "../App";
import { Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader } from "../components/page";
import { formatCount } from "../lib/chartFormat";
import { CostCoverage, ProfitComparison } from "../components/ProfitSummaryCharts";
import { ProfitAccounts } from "../components/ProfitAccounts";
import { errorText } from "../lib/fmt";
import { fmtMicroCny } from "../lib/money";
import { beijingInputValue, beijingRange, fmtBeijing, parseBeijingInput, todayBeijing } from "../lib/time";
import { fetchProfitSummary, profitRangeError, type ProfitSummary } from "../lib/totals";
import { useHashQuery } from "../lib/useHashQuery";
import { Button } from "../ui/Button";
import "./totals.css";

interface TotalsAnswer { key: string; data: ProfitSummary | null; error: string | null }

/** 同一数据库快照汇算收入和成本，保留旧 totals 链接的显式时间范围。 */
export function Totals(_props: TabProps) {
  const [query, update] = useHashQuery();
  const [fallback] = useState(() => {
    const to = new Date();
    return { from: new Date(to.getTime() - 86_400_000).toISOString(), to: to.toISOString() };
  });
  const from = query.get("from") ?? undefined;
  const to = query.get("to") ?? undefined;
  const allTime = from === undefined && to === undefined;
  const [revision, setRevision] = useState(0);
  const [answer, setAnswer] = useState<TotalsAnswer>({ key: "", data: null, error: null });
  const key = JSON.stringify([from, to, revision]);
  const rangeError = profitRangeError(from, to);
  useEffect(() => {
    if (rangeError) return;
    let current = true;
    setAnswer({ key, data: null, error: null });
    void fetchProfitSummary(from, to).then(data => {
      if (current) setAnswer({ key, data, error: null });
    }, (error: unknown) => {
      if (current) setAnswer({ key, data: null, error: errorText(error) });
    });
    return () => { current = false; };
  }, [key, from, to, rangeError]);

  const data = !rangeError && answer.key === key ? answer.data : null;
  const error = rangeError ?? (answer.key === key ? answer.error : null);
  const loading = !data && !error;
  const retry = () => setRevision(value => value + 1);
  const dateValue = (value: string) => Number.isFinite(Date.parse(value)) ? beijingInputValue(new Date(value)) : value;
  const changeDate = (name: string, value: string) => update({ from: from ?? fallback.from, to: to ?? fallback.to, [name]: parseBeijingInput(value) ?? value });
  function selectRange(days: number) {
    const end = new Date();
    update({ from: new Date(end.getTime() - days * 86_400_000).toISOString(), to: end.toISOString() });
  }
  function selectDay(daysAgo: number) {
    const day = todayBeijing(new Date(Date.now() - daysAgo * 86_400_000));
    update(beijingRange(day, day));
  }
  const complete = data !== null && data.rowsWithoutCost === 0n && data.unpricedRows === 0n;
  const costKnown = data !== null && (data.rowsWithCost > 0n || complete);
  const pending = error ? <CardMessage onRetry={rangeError ? undefined : retry}>{error}</CardMessage> : <MetricSkeleton className="totals-skeleton" />;
  return <div className="totals-page">
    <PageHeader title="本利汇算" description="全站调用的总售价、历史上游成本与利润率，人民币。" />
    <section className="totals-scope" aria-label="统计时间范围">
      <div className="totals-dates">
        <label className="f"><span>开始时间（UTC+8）</span><input type="datetime-local" step="1" disabled={allTime} value={dateValue(from ?? fallback.from)} onChange={event => changeDate("from", event.target.value)} /></label>
        <label className="f"><span>结束时间（不含）</span><input type="datetime-local" step="1" disabled={allTime} value={dateValue(to ?? fallback.to)} onChange={event => changeDate("to", event.target.value)} /></label>
      </div>
      <div className="totals-actions">
        <div className="totals-presets" aria-label="快捷时间范围">
          <Button size="sm" kind="quiet" aria-pressed={allTime} onClick={() => update({ from: undefined, to: undefined })}>全部时间</Button>
          <Button size="sm" kind="quiet" aria-pressed={!allTime} onClick={() => update({ from: from ?? fallback.from, to: to ?? fallback.to })}>指定时间段</Button>
          <Button size="sm" kind="quiet" onClick={() => selectDay(0)}>今天</Button>
          <Button size="sm" kind="quiet" onClick={() => selectDay(1)}>昨天</Button>
          <Button size="sm" kind="quiet" onClick={() => selectRange(1)}>最近 24 小时</Button>
          <Button size="sm" kind="quiet" onClick={() => selectRange(7)}>最近 7 天</Button>
          <Button size="sm" kind="quiet" onClick={() => selectRange(30)}>最近 30 天</Button>
        </div>
        <Button size="sm" onClick={retry} busy={loading} busyLabel="正在刷新统计" disabled={Boolean(rangeError)}>刷新</Button>
      </div>
    </section>
    {rangeError && <p className="totals-error" role="alert">{rangeError}</p>}
    <p className="totals-window">{allTime ? "全部时间 · 人民币会计历史。" : `${fmtBeijing(from ?? "")} — ${fmtBeijing(to ?? "")}（UTC+8，结束时间不含）`}</p>
    {loading && <span role="status" className="sr-only">正在加载总量统计</span>}
    <div className="totals-visuals">
      <Card className="totals-comparison-card" aria-busy={loading}>
        <CardHeader title="收入与成本对照" description="同一统计快照 · 人民币" />
        {data ? <ProfitComparison data={data} /> : pending}
      </Card>
      <Card className="totals-coverage-card" aria-busy={loading}>
        <CardHeader title="成本记录覆盖" description="已计费记录的历史成本完整度" />
        {data ? <CostCoverage data={data} /> : pending}
      </Card>
    </div>
    <div className="totals-metrics" aria-label="汇算明细">
      <section className="totals-metric" aria-label="总收入统计" aria-busy={loading}>
        <CardHeader title="总售价 · 调用收入" description="已计价收费，含净费用调整" />
        {data ? <>
          <Metric size="figure" className="totals-value">{fmtMicroCny(data.revenueMicroCny)}</Metric>
          <dl className="totals-facts">
            <div><dt>计价记录</dt><dd>{formatCount(data.rows)}</dd></div>
            <div><dt>待计价记录</dt><dd>{formatCount(data.unpricedRows)}</dd></div>
            <div><dt>已含费用调整</dt><dd>{fmtMicroCny(data.adjustedMicroCny)}</dd></div>
          </dl>
          {data.unpricedRows > 0n && <p className="totals-warning" role="status">{formatCount(data.unpricedRows)} 笔记录仍待计价，当前为已计价收入小计，完成计价后可能变化。</p>}
        </> : pending}
        <p className="totals-note">收入包含已计费重试，费用调整已计入且不重复相加。充值流水不计入调用收入。</p>
      </section>
      <section className="totals-metric" aria-label="总成本统计" aria-busy={loading}>
        <CardHeader title={complete ? "总成本" : "已知成本小计"} description="已计费记录的历史上游成本" />
        {data ? <>
          <Metric size="figure" className="totals-value">{costKnown ? fmtMicroCny(data.knownCostMicroCny) : "—"}</Metric>
          <dl className="totals-facts">
            <div><dt>成本已知的记录</dt><dd>{formatCount(data.rowsWithCost)}</dd></div>
            <div><dt>成本缺失的记录</dt><dd>{formatCount(data.rowsWithoutCost)}</dd></div>
            <div><dt>缺失成本对应收入</dt><dd>{fmtMicroCny(data.revenueWithoutCostMicroCny)}</dd></div>
          </dl>
          {!complete && <p className="totals-warning" role="status">{data.rowsWithoutCost > 0n ? `${formatCount(data.rowsWithoutCost)} 笔已计费记录缺失成本。` : "仍有待计价记录。"}当前仅为已知成本小计，不代表完整总成本。</p>}
        </> : pending}
        <p className="totals-note">成本仅含已收费或存在费用调整的记录。未知成本不按零计算，不使用当前费率估算历史成本。</p>
      </section>
      <section className="totals-metric" aria-label="利润率统计" aria-busy={loading}>
        <CardHeader title="完整利润率" description="利润 ÷ 总售价" />
        {data ? <>
          <Metric size="figure" className="totals-value">{data.marginPct === null ? "—" : `${data.marginPct}%`}</Metric>
          <dl className="totals-facts">
            <div><dt>完整利润</dt><dd>{data.profitMicroCny === null ? "—" : fmtMicroCny(data.profitMicroCny)}</dd></div>
            <div><dt>已知成本部分收入</dt><dd>{fmtMicroCny(data.revenueWithCostMicroCny)}</dd></div>
            <div><dt>已知成本部分利润</dt><dd>{data.knownProfitMicroCny === null ? "—" : fmtMicroCny(data.knownProfitMicroCny)}</dd></div>
            <div><dt>已知成本部分利润率</dt><dd>{data.knownMarginPct === null ? "—" : `${data.knownMarginPct}%`}</dd></div>
          </dl>
          {!complete ? <p className="totals-warning" role="status">收入或成本尚不完整，完整利润与利润率未知；已知部分只对成本已知的同一批记录汇算。</p> : data.marginPct === null ? <p className="totals-note">总售价不大于零时，利润率无定义。</p> : null}
        </> : pending}
        <p className="totals-note">利润率按收入加权汇算，不平均各模型的百分比；亏损保留负值。</p>
      </section>
    </div>
    {data && <ProfitAccounts accounts={data.accounts} />}
    <section className="totals-method" aria-label="统计口径">
      <h2>统计口径</h2>
      <p>本页仅汇总人民币会计历史。总售价汇总已计价调用收费与净费用调整，以原调用的用量发生时间归期；费用调整的入账时间不改变归期。成本采用同范围内当笔已计费记录的历史上游成本。</p>
      <p>收入、成本与覆盖笔数来自同一统计快照。成本缺失或仍有待计价记录时，不推算完整利润；已知部分的利润以成本已知记录的收入减去其成本。</p>
    </section>
  </div>;
}
