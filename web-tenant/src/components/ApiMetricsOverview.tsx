import type { ApiMetricsResponse, MetricsScope } from "../lib/apiMetrics";
import { formatCount, formatDuration, formatRatio } from "../lib/chartFormat";
import { cn } from "../lib/cn";
import { fmtMicroCny } from "../lib/money";
import { fmtBeijing, metricPeriodLabel } from "../lib/time";
import { TrendChart } from "./charts";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, span } from "./page";

interface ApiMetricsOverviewProps {
  scope: MetricsScope;
  from: string;
  to: string;
  data: ApiMetricsResponse | null;
  error: string | null;
  onRetry?: () => void;
  className?: string;
}

/** 首页只显示同次全范围响应的四项摘要和两条趋势，完整诊断留在分析页。 */
export function ApiMetricsOverview({ scope, from, to, data, error, onRetry, className }: ApiMetricsOverviewProps) {
  const totals = data?.totals;
  const query = new URLSearchParams({ from, to });
  const analysisHref = `#${scope === "admin" ? "analytics" : "usage"}?${query}`;
  const trends = [
    { title: "请求趋势", points: data?.series.map(row => ({ label: metricPeriodLabel(row.period_start, data.bucket), value: row.requests, displayValue: formatCount(row.requests) })) ?? [], note: "请求数按最终记录统计，未知结果仍单列。" },
    { title: "首 token 延迟概况", points: data?.series.map(row => ({ label: metricPeriodLabel(row.period_start, data.bucket), value: row.first_token_latency_ms.avg, displayValue: formatDuration(row.first_token_latency_ms.avg) })) ?? [], note: "趋势使用实测平均值；无样本不以首帧或完整耗时补齐。" },
  ];
  return <BentoGrid className={cn(span.d12, "min-w-0", className)} role="group" aria-label="API 指标概览" aria-busy={!error && !data}>
    <div className={cn(span.d12, "flex flex-wrap items-center justify-between gap-3")}>
      <p className="text-caption text-muted">{fmtBeijing(data?.from ?? from)} — {fmtBeijing(data?.to ?? to)}（结束时间不含）</p>
      <a href={analysisHref} className="inline-flex min-h-11 min-w-11 items-center text-caption text-ink underline underline-offset-4">查看用量分析 →</a>
    </div>
    <Card className={span.d3} data-metric-kpi="requests">
      <h2 className="text-caption font-normal text-muted">请求数</h2>
      <Metric size="value" className="mt-3 break-all">{formatCount(totals?.requests)}</Metric>
      <p className="mt-3 text-caption text-muted">最终请求 · 计费尝试 {formatCount(totals?.billing_attempts)}</p>
      <p className="mt-2 text-caption text-muted">最终状态未知的尝试 {formatCount(totals?.unknown_final_attempts)}</p>
    </Card>
    <Card className={span.d3} data-metric-kpi="success">
      <h2 className="text-caption font-normal text-muted">成功率</h2>
      <Metric size="value" className="mt-3">{formatRatio(totals?.success_rate)}</Metric>
      <p className="mt-3 text-caption text-muted">成功 {formatCount(totals?.succeeded_requests)} · 失败 {formatCount(totals?.failed_requests)} · 未知 {formatCount(totals?.unknown_outcome_requests)}</p>
      <p className="mt-2 text-caption text-muted">已知结果样本（分母）{formatCount(totals?.known_outcome_requests)}；未知结果不进入分母。</p>
    </Card>
    <Card className={span.d3} data-metric-kpi="latency">
      <h2 className="text-caption font-normal text-muted">首 token 延迟 P50</h2>
      <Metric size="value" className="mt-3">{formatDuration(totals?.first_token_latency_ms.p50)}</Metric>
      <p className="mt-3 text-caption text-muted">实测样本 {formatCount(totals?.first_token_latency_ms.sample_count)} · 无实测值 {formatCount(totals?.first_token_latency_ms.missing_count)}</p>
      <p className="mt-2 text-caption text-muted">无值包含非流式与历史记录；P95/P99 见分析。</p>
    </Card>
    <Card className={span.d3} data-metric-kpi="cost">
      <h2 className="text-caption font-normal text-muted">已计价费用</h2>
      <Metric size="value" className="mt-3 break-all">{!totals ? "—" : totals.charged_micro_cny === null ? "未知" : fmtMicroCny(totals.charged_micro_cny)}</Metric>
      <p className="mt-3 text-caption text-muted">待计价尝试 {formatCount(totals?.unpriced_attempts)}</p>
      <p className="mt-2 text-caption text-muted">含可见已计费重试与历史调整；待计价不等于免费。</p>
    </Card>
    {error ? <div className={span.d12}><CardMessage onRetry={onRetry}><span role="alert">{error}</span></CardMessage></div> : !data ? <p className={cn(span.d12, "text-caption text-muted")} role="status">正在加载当前范围的 API 指标</p> : null}
    {trends.map(trend => <Card key={trend.title} className={span.d6}>
      <CardHeader title={trend.title} />
      <div className="min-h-64">{data ? <TrendChart points={trend.points} accessibleLabel={trend.title} emptyLabel="所选范围没有可用期间数据" /> : <div aria-hidden="true" className="h-56 rounded-cell bg-ink/7" />}</div>
      <p className="mt-4 text-caption text-muted">{trend.note}按北京时间汇总，精确值可用键盘逐期查看。</p>
    </Card>)}
  </BentoGrid>;
}
