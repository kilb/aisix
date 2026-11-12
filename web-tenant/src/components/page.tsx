import type { ComponentProps, ReactNode } from "react";
import { cn } from "../lib/cn";
import { Button } from "../ui/Button";

/** 双端稳定页面原语：布局与呈现负责在这里，权限、数据和金额计算仍归业务页面。 */
export function PageHeader({
  title, description, dataLead, actions, className,
}: {
  title: ReactNode;
  description?: ReactNode;
  eyebrow?: ReactNode;
  dataLead?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <header className={cn("page-header bento-page-header", className)}>
      <div className="min-w-0">
        <h1 className="text-title font-semibold">{title}</h1>
        {(dataLead ?? description) != null && (
          <div className="bento-lead mt-2 text-lead font-normal text-ink-soft">{dataLead ?? description}</div>
        )}
        {dataLead != null && description != null && <div className="mt-1.5 text-caption text-muted">{description}</div>}
      </div>
      {actions && <div className="bento-page-actions flex flex-wrap items-center gap-2">{actions}</div>}
    </header>
  );
}

/** 断点和列宽均为静态 class，构建时可完整扫描。 */
export const span = {
  d3: "col-span-12 tablet:col-span-6 desktop:col-span-3",
  d4: "col-span-12 tablet:col-span-6 desktop:col-span-4",
  d5: "col-span-12 tablet:col-span-6 desktop:col-span-5",
  d5Wide: "col-span-12 tablet:col-span-12 desktop:col-span-5",
  d6: "col-span-12 tablet:col-span-6 desktop:col-span-6",
  d7: "col-span-12 tablet:col-span-12 desktop:col-span-7",
  d8: "col-span-12 tablet:col-span-12 desktop:col-span-8",
  d12: "col-span-12 tablet:col-span-12 desktop:col-span-12",
} as const;

export function BentoGrid({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("bento-grid grid grid-cols-12 gap-(--card-gap)", className)} />;
}

export function Card({ className, tone = "surface", ...props }: ComponentProps<"section"> & { tone?: "surface" | "inverse" }) {
  return <section {...props} data-tone={tone} className={cn("panel bento-card", className)} />;
}

export function CardHeader({
  title, description, actions, className, as: Heading = "h2",
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
  number?: number;
  as?: "h2" | "h3";
}) {
  return (
    <div className={cn("bento-card-header flex flex-wrap items-center justify-between gap-x-4 gap-y-2", className)}>
      <div className="min-w-0">
        <Heading className="flex items-baseline gap-2 text-stat font-semibold text-foreground">
          <span>{title}</span>
        </Heading>
        {description && <p className="mt-1.5 text-caption text-muted">{description}</p>}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </div>
  );
}

export function CardFooter({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("bento-card-footer mt-auto", className)} />;
}

type MetricSize = "hero" | "display" | "figure" | "value";
const metricSize = { hero: "text-hero", display: "text-display", figure: "text-figure", value: "text-value" } as const;

/** 不解析或缩写金额；调用方传入精确 formatter 的结果。 */
export function Metric({ children, size = "value", className }: { children: ReactNode; size?: MetricSize; className?: string }) {
  return <div className={cn("metric font-medium tabular-nums", metricSize[size], className)}>{children}</div>;
}

export function MetricSkeleton({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} aria-hidden="true" className={cn("bento-metric-skeleton rounded-cell bg-foreground/7", className)} />;
}

export function ChartSkeleton({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} aria-hidden="true" className={cn("bento-chart-skeleton rounded-cell bg-foreground/7", className)} />;
}

export function CardMessage({ children, onRetry }: { children: ReactNode; onRetry?: () => void }) {
  return (
    <div className="bento-card-message flex min-h-24 flex-col items-start justify-center gap-3 text-caption text-muted">
      <div>{children}</div>
      {onRetry && <Button onClick={onRetry}>重试</Button>}
    </div>
  );
}

export function Split({ main, aside, className }: {
  main: ReactNode;
  aside?: ReactNode;
  /** 兼容旧调用；详情宽度统一由 7+5 列网格决定，不再固定像素宽度。 */
  asideWidth?: string;
  className?: string;
}) {
  if (!aside) return <div className={className}>{main}</div>;
  return (
    <BentoGrid className={cn("bento-split items-start", className)}>
      <div className={cn("min-w-0", span.d7)}>{main}</div>
      <aside className={cn("min-w-0 desktop:sticky desktop:top-6", span.d5Wide)}>{aside}</aside>
    </BentoGrid>
  );
}

export function Grid({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("grid gap-(--card-gap) tablet:grid-cols-2 desktop:grid-cols-3", className)} />;
}

export function StatGrid({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("bento-stat-grid mb-(--card-gap) grid gap-(--card-gap) tablet:grid-cols-2 desktop:grid-cols-4", className)} />;
}

export function Stat({ label, value, foot, className }: { label: ReactNode; value: ReactNode; foot?: ReactNode; className?: string }) {
  return (
    <div className={cn("panel bento-stat", className)}>
      <div className="flex items-baseline gap-3 text-caption text-muted">
        <span>{label}</span>
      </div>
      <Metric className="mt-3">{value}</Metric>
      {foot && <div className="mt-2 text-caption text-muted">{foot}</div>}
    </div>
  );
}

export function Toolbar({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("mb-4 flex flex-wrap items-end gap-3", className)} />;
}

export function DescList({ items, className }: { items: { label: ReactNode; value: ReactNode; key?: string }[]; className?: string }) {
  return (
    <dl className={cn("grid gap-x-8 gap-y-0 tablet:grid-cols-2", className)}>
      {items.map((it, i) => (
        <div key={it.key ?? i} className="flex min-w-0 items-baseline justify-between gap-6 border-b border-divider py-3">
          <dt className="shrink-0 text-muted">{it.label}</dt>
          <dd className="min-w-0 text-right">{it.value}</dd>
        </div>
      ))}
    </dl>
  );
}
