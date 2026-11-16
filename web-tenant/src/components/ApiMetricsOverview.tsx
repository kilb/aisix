import { useId } from "react";
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
  const id = useId();
  const totals = data?.totals;
  const costText = !totals ? "—" : totals.charged_micro_cny === null ? "未知" : fmtMicroCny(totals.charged_micro_cny);
  const query = new URLSearchParams({ from, to });
  const analysisHref = `#${scope === "admin" ? "analytics" : "usage"}?${query}`;
  const trends = [
    { title: "请求趋势", points: data?.series.map(row => ({ label: metricPeriodLabel(row.period_start, data.bucket), value: row.requests, displayValue: formatCount(row.requests) })) ?? [], note: "请求数按最终记录统计，未知结果仍单列。" },
    { title: "首 token 延迟概况", points: data?.series.map(row => ({ label: metricPeriodLabel(row.period_start, data.bucket), value: row.first_token_latency_ms.avg, displayValue: formatDuration(row.first_token_latency_ms.avg) })) ?? [], note: "趋势使用实测平均值；无样本不以首帧或完整耗时补齐。" },
  ];
  return <BentoGrid className={cn(span.d12, "metrics-overview min-w-0", className)} role="group" aria-label="API 指标概览" aria-busy={!error && !data}>
    <div className={cn(span.d12, "metric-range")}>
      <p className="text-caption text-fg-3">{fmtBeijing(data?.from ?? from)} — {fmtBeijing(data?.to ?? to)}（UTC+8，结束时间不含）</p>
      <a href={analysisHref} className="metric-scope-action">查看用量分析</a>
    </div>
    <div className={cn(span.d12, "overview-kpis")}>
    <Card padding="sm" data-metric-kpi="requests" aria-labelledby={`${id}-requests`}>
      <h2 id={`${id}-requests`}>请求数</h2>
      <Metric size="value" className="break-all">{formatCount(totals?.requests)}</Metric>
      <p className="mt-3 text-caption text-fg-3">最终请求 · 计费尝试 {formatCount(totals?.billing_attempts)}</p>
      <p className="mt-2 text-caption text-fg-3">最终状态未知的尝试 {formatCount(totals?.unknown_final_attempts)}</p>
    </Card>
    <Card tone="lime" padding="sm" data-metric-kpi="success" aria-labelledby={`${id}-success`}>
      <h2 id={`${id}-success`}>成功率</h2>
      <Metric size="value">{formatRatio(totals?.success_rate)}</Metric>
      <p className="mt-3 text-caption text-fg-3">成功 {formatCount(totals?.succeeded_requests)} · 失败 {formatCount(totals?.failed_requests)} · 未知 {formatCount(totals?.unknown_outcome_requests)}</p>
      <p className="mt-2 text-caption text-fg-3">已知结果样本（分母）{formatCount(totals?.known_outcome_requests)}；未知结果不进入分母。</p>
    </Card>
    <Card padding="sm" data-metric-kpi="latency" aria-labelledby={`${id}-latency`}>
      <h2 id={`${id}-latency`}>首 token 延迟 P50</h2>
      <Metric size="value">{formatDuration(totals?.first_token_latency_ms.p50)}</Metric>
      <p className="mt-3 text-caption text-fg-3">实测样本 {formatCount(totals?.first_token_latency_ms.sample_count)} · 无实测值 {formatCount(totals?.first_token_latency_ms.missing_count)}</p>
      <p className="mt-2 text-caption text-fg-3">无值包含非流式与历史记录；P95/P99 见分析。</p>
    </Card>
    <Card padding="sm" data-metric-kpi="cost" aria-labelledby={`${id}-cost`} className={costText.length > 10 ? "col-span-2 tablet:col-span-1" : undefined}>
      <h2 id={`${id}-cost`}>已计价费用</h2>
      <Metric size="value" className="min-w-0 max-w-full overflow-x-auto whitespace-nowrap text-heading tablet:text-value">{costText}</Metric>
      <p className="mt-3 text-caption text-fg-3">待计价尝试 {formatCount(totals?.unpriced_attempts)}</p>
      <p className="mt-2 text-caption text-fg-3">含可见已计费重试与历史调整；待计价不等于免费。</p>
    </Card>
    </div>
    {error ? <div className={span.d12}><CardMessage onRetry={onRetry}><span role="alert">{error}</span></CardMessage></div> : !data ? <p className={cn(span.d12, "text-caption text-fg-3")} role="status">正在加载当前范围的 API 指标</p> : null}
    {trends.map((trend, index) => <Card key={trend.title} tone={index === 0 ? "lilac" : "default"} className={cn(span.d6, "overview-trend")}>
      <CardHeader title={trend.title} />
      <div className="metric-trend-slot">{data ? <TrendChart points={trend.points} accessibleLabel={trend.title} emptyLabel="所选范围没有可用期间数据" /> : <div aria-hidden="true" className="metric-chart-skeleton" />}</div>
      <p className="mt-4 text-caption text-fg-3">{trend.note}按期间汇总，精确值可用键盘逐期查看。</p>
    </Card>)}
  </BentoGrid>;
}
