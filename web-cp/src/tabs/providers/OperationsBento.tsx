import type { ReactNode } from "react";
import { RankBars } from "../../components/charts";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, span } from "../../components/page";
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

/** 连接与配置页共用的当前集合概览；所有值由页面的真实响应提供。 */
export function InventorySummary({
  title,
  total,
  totalLabel,
  breakdown,
  breakdownLabel,
  secondaryTitle,
  secondaryValue,
  secondaryFoot,
  children,
  error,
  secondaryError,
  onRetry,
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
    <BentoGrid className="ops-summary" aria-label={title}>
      <Card className={span.d7}>
        <CardHeader number={1} title={title} />
        {total === null ? (
          error ? <CardMessage onRetry={onRetry}>数据加载失败，请重试。</CardMessage> : <MetricSkeleton />
        ) : (
          <>
            <div className="ops-metric-line">
              <Metric size="hero">{fmtInt(total)}</Metric>
              <p className="text-caption text-muted">{totalLabel}</p>
            </div>
            <CardFooter>
              <p className="mb-3 font-mono text-micro text-muted">{breakdownLabel}</p>
              <RankBars items={breakdown} accessibleLabel={breakdownLabel} emptyLabel="当前集合为空。" />
            </CardFooter>
          </>
        )}
        {error && total !== null && <p className="mt-3 text-caption text-ink-soft" role="status">刷新失败，保留最近成功读取的结果。</p>}
      </Card>
      <Card className={span.d5Wide} tone="inverse">
        <CardHeader number={2} title={secondaryTitle} />
        {secondaryValue === null ? secondaryError ? <CardMessage>数据暂不可用，请查看下方错误提示。</CardMessage> : <MetricSkeleton /> : <Metric size="display">{fmtInt(secondaryValue)}</Metric>}
        <p className="mt-4 text-caption text-on-inverse-muted">{secondaryFoot}</p>
        {children && <CardFooter>{children}</CardFooter>}
      </Card>
    </BentoGrid>
  );
}
