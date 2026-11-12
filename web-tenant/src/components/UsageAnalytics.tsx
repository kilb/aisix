import type { UsageSummaryRow } from "../lib/api";
import { exactRatio, normalizeExactValues, summarizeUsage } from "../lib/chartFormat";
import { fmtMicroCny, periodLabel, todayUtc } from "../lib/money";
import { RankBars, TrendChart } from "./charts";
import { Card, CardFooter, CardHeader, CardMessage, ChartSkeleton, Metric, MetricSkeleton } from "./page";

type Summary = ReturnType<typeof summarizeUsage>;
type Props = {
  rows: UsageSummaryRow[] | null;
  error: string | null;
  rangeLabel: string;
  group?: "day" | "month";
  number: number;
  className: string;
  onRetry?: () => void;
};

/** 图形只接收归一化位置，所有金额继续使用原始精确整数展示。 */
function trendPoints(summary: Summary, field: "requests" | "chargedMicroCny", group: "day" | "month") {
  const values = normalizeExactValues(summary.periods.map((period) => period[field]));
  return summary.periods.map((period, index) => ({
    label: periodLabel(period.label, group),
    value: values[index] ?? null,
    displayValue: period[field] === null ? "未知" : field === "requests" ? period[field].toLocaleString("en-US") : fmtMicroCny(period[field]),
  }));
}

/** 只有真实当前期间使用强调色，历史查询的末期不自动视为今天。 */
function currentPeriod(summary: Summary, group: "day" | "month"): number | undefined {
  const length = group === "day" ? 10 : 7;
  const index = summary.periods.findIndex((period) => period.label.slice(0, length) === todayUtc().slice(0, length));
  return index < 0 ? undefined : index;
}

function DataState({ rows, error, onRetry, chart = false }: Pick<Props, "rows" | "error" | "onRetry"> & { chart?: boolean }) {
  if (error) return <CardMessage onRetry={onRetry}>{error}</CardMessage>;
  if (rows === null) return <><MetricSkeleton />{chart && <ChartSkeleton />}</>;
  return <CardMessage>所选范围内没有用量。产生调用后，这里会显示实际数据。</CardMessage>;
}

export function UsageRequestCard({ rows, error, rangeLabel, group = "day", number, className, onRetry }: Props) {
  const summary = rows === null ? null : summarizeUsage(rows);
  return (
    <Card className={className}>
      <CardHeader number={number} title="调用尝试次数" description={rangeLabel} />
      {error || rows === null || rows.length === 0 ? <DataState rows={rows} error={error} onRetry={onRetry} chart /> : summary && <>
        <Metric size="hero" className="break-all">{summary.requests === null ? "未知" : summary.requests.toLocaleString("en-US")}</Metric>
        <CardFooter>
          {summary.hasMergedPeriods ? <CardMessage>部分期间明细已合并，无法完整绘制趋势。合并的用量仍计入总数，请查看用量汇总。</CardMessage> :
            <TrendChart points={trendPoints(summary, "requests", group)} currentIndex={currentPeriod(summary, group)} accessibleLabel={`${rangeLabel}可见调用尝试次数趋势，合计${summary.requests === null ? "未知" : summary.requests.toLocaleString("en-US")}次，按实际有记录的期间汇总`} />}
          {!summary.hasMergedPeriods && <p className="mt-3 text-caption text-muted">显示实际有记录的期间，缺失期间未填零。图形使用相对刻度。</p>}
          <p className="mt-3 text-caption text-muted">包含最终尝试，以及已计费或重新计价的重试；不是去重后的最终请求数。</p>
        </CardFooter>
      </>}
    </Card>
  );
}

export function UsageCostCard({ rows, error, rangeLabel, number, className, onRetry }: Props) {
  const summary = rows === null ? null : summarizeUsage(rows);
  const cost = summary?.chargedMicroCny == null ? "未知" : fmtMicroCny(summary.chargedMicroCny);
  return (
    <Card tone="inverse" className={className}>
      <CardHeader number={number} title="费用" description={rangeLabel} />
      {error || rows === null || rows.length === 0 ? <DataState rows={rows} error={error} onRetry={onRetry} /> : summary && <>
        <Metric size={cost.length > 14 ? "value" : "figure"} className="break-all">{cost}</Metric>
        <CardFooter>
          <p className="text-caption">{summary.estimatedRequests === null ? "估算调用数未知" : summary.estimatedRequests > 0n ? `含 ${summary.estimatedRequests.toLocaleString("en-US")} 次估算调用` : "所选范围内不含估算调用"}</p>
          <p className="mt-3 text-caption">按用量发生期间归属。历史费率、折扣与调整可在调用明细中核查。</p>
        </CardFooter>
      </>}
    </Card>
  );
}

export function UsageModelsCard({ rows, error, rangeLabel, number, className, onRetry }: Props) {
  const summary = rows === null ? null : summarizeUsage(rows);
  const ranked = summary?.models.filter((model) => model.chargedMicroCny !== null).sort((a, b) => {
    const left = a.chargedMicroCny ?? 0n;
    const right = b.chargedMicroCny ?? 0n;
    return left > right ? -1 : left < right ? 1 : a.label.localeCompare(b.label);
  }) ?? [];
  const shown = ranked.slice(0, 5);
  const values = normalizeExactValues(shown.map((model) => model.chargedMicroCny));
  const total = summary?.chargedMicroCny ?? null;
  const canShowShare = total !== null && total > 0n && summary?.models.every((model) => model.chargedMicroCny !== null && model.chargedMicroCny >= 0n);
  return (
    <Card className={className}>
      <CardHeader number={number} title="模型费用分布" description={rangeLabel} />
      {error || rows === null || rows.length === 0 ? <DataState rows={rows} error={error} onRetry={onRetry} chart /> : summary && <>
        <div className="flex flex-wrap items-baseline gap-2">
          <Metric size="value">{summary.models.filter((model) => model.label !== "" && !(model.id === null && model.label === "其他")).length.toLocaleString("en-US")}</Metric>
          <span className="text-caption text-muted">可识别模型</span>
        </div>
        {ranked.length !== summary.models.length || total === null ? <CardMessage>部分模型费用未知，无法完整计算占比。请查看用量汇总。</CardMessage> : <RankBars
          items={shown.map((model, index) => {
            const share = canShowShare ? exactRatio(model.chargedMicroCny, total) : null;
            return { label: model.label || "未授权或未知模型", value: values[index] ?? 0, displayValue: `${model.chargedMicroCny === null ? "未知" : fmtMicroCny(model.chargedMicroCny)}${share === null ? "" : ` · ${(share * 100).toFixed(1)}%`}` };
          })}
          accessibleLabel={`${rangeLabel}费用最高的 ${shown.length} 个模型分组及其费用占比`}
        />}
        <CardFooter>
          <p className="text-caption text-muted">{ranked.length > shown.length ? `展示费用前 ${shown.length} 个分组，共 ${ranked.length} 个。` : "按各模型已计费用量汇总。"}{!canShowShare && total !== null ? "费用含负数调整或合计为零时，不计算占比。" : ""}{summary.hasMergedPeriods ? "“其他”为跨期合并分组，不计入可识别模型数。" : ""}</p>
        </CardFooter>
      </>}
    </Card>
  );
}

export function UsageCostTrendCard({ rows, error, rangeLabel, group = "day", number, className, onRetry }: Props) {
  const summary = rows === null ? null : summarizeUsage(rows);
  return (
    <Card className={className}>
      <CardHeader number={number} title="费用趋势" description={rangeLabel} />
      {error || rows === null || rows.length === 0 ? <DataState rows={rows} error={error} onRetry={onRetry} chart /> : summary && <>
        {summary.hasMergedPeriods ? <CardMessage>部分期间明细已合并，无法完整绘制趋势。合并费用仍计入总额，请查看用量汇总。</CardMessage> :
          <TrendChart points={trendPoints(summary, "chargedMicroCny", group)} currentIndex={currentPeriod(summary, group)} accessibleLabel={`${rangeLabel}费用趋势，合计${summary.chargedMicroCny === null ? "未知" : fmtMicroCny(summary.chargedMicroCny)}，按实际有记录的期间汇总`} />}
        <CardFooter><p className="text-caption text-muted">按用量发生时间归属，重新计价后的调整归入原期间。只显示有记录的期间；图形使用相对刻度，金额保留原始精度。</p></CardFooter>
      </>}
    </Card>
  );
}
