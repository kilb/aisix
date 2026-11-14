import type { ReactNode } from "react";
import { RankBars } from "../../components/charts";
import { CardMessage, Metric, MetricSkeleton } from "../../components/page";
import { fmtInt } from "../../lib/fmt";
import "./OperationsBento.css";

type Breakdown = { label: string; value: number; displayValue: string };

/** 仅汇总本次完整读取到的集合，不把缺失数据补成零。 */
export function countGroups(values: readonly string[]): Breakdown[] {
  const counts = new Map<string, number>();
  for (const value of values) counts.set(value, (counts.get(value) ?? 0) + 1);
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([label, value]) => ({ label, value, displayValue: fmtInt(value) }));
}

/** 配置概况共用一个工作区，主次计数与分布保持真实读取范围。 */
export function InventorySummary({
  title, total, totalLabel, breakdown, breakdownLabel, secondaryTitle,
  secondaryValue, secondaryFoot, children, error, secondaryError, onRetry,
}: {
  title: string;
  total: number | null;
  totalLabel: string;
  breakdown: Breakdown[];
  breakdownLabel: string;
  secondaryTitle: string;
  secondaryValue: number | null;
  secondaryFoot: ReactNode;
  children?: ReactNode;
  error?: boolean;
  secondaryError?: boolean;
  onRetry?: () => void;
}) {
  return (
    <section className="ops-summary" aria-label={title}>
      <h2>{title}</h2>
      <div className="ops-summary-grid">
        <div className="ops-summary-total">
          <p className="ops-summary-label">{totalLabel}</p>
          {total === null ? (
            error ? <CardMessage onRetry={onRetry}>数据加载失败，请重试。</CardMessage> : <MetricSkeleton />
          ) : <Metric size="figure">{fmtInt(total)}</Metric>}
          {error && total !== null && <p className="ops-summary-note" role="status">刷新失败，保留最近成功读取的结果。</p>}
        </div>
        <div className="ops-summary-breakdown">
          <p className="ops-summary-label">{breakdownLabel}</p>
          {total !== null && <RankBars items={breakdown} accessibleLabel={breakdownLabel} emptyLabel="当前集合为空。" />}
        </div>
        <div className="ops-summary-secondary">
          <p className="ops-summary-label">{secondaryTitle}</p>
          {secondaryValue === null
            ? secondaryError ? <CardMessage>数据暂不可用，请查看下方错误提示。</CardMessage> : <MetricSkeleton />
            : <Metric size="figure">{fmtInt(secondaryValue)}</Metric>}
          <p className="ops-summary-note">{secondaryFoot}</p>
          {children}
        </div>
      </div>
    </section>
  );
}
