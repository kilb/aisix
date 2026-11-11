import type { ComponentProps, ReactNode } from "react";
import { cn } from "../lib/cn";

/**
 * The page layout kit (2026-11-11 rebuild): every page is a PageHeader,
 * then its content laid out with these pieces, so pages share one rhythm.
 *
 * - PageHeader: the title, a one-paragraph description and the page's
 *   actions on the right.
 * - Card: a white card with a 1px border (keeps the `panel` class the
 *   specs and the older stylesheets know). CardHeader puts a title,
 *   description and actions on one row; a DataTable straight inside a Card
 *   runs edge to edge.
 * - Split: the main column and an aside beside it from 1280px (a create
 *   form, a detail, a summary), stacked below that.
 * - Grid: equal columns that fill the width (2 / 3 / 4 at sm / lg / 2xl).
 * - StatGrid + Stat: the figures a page leads with.
 * - Toolbar: filters and actions above a list, on one wrapping row.
 * - DescList: label / value pairs, two columns where there is room.
 */

export function PageHeader({
  title,
  description,
  actions,
  className,
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("page-header mb-6 flex flex-wrap items-start justify-between gap-x-8 gap-y-3", className)}>
      <div className="min-w-0 max-w-3xl">
        <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
        {description && <p className="hint mt-1.5 text-[14px] leading-relaxed text-muted-foreground">{description}</p>}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </div>
  );
}

export function Card({ className, ...props }: ComponentProps<"section">) {
  return <section {...props} className={cn("panel", className)} />;
}

export function CardHeader({
  title,
  description,
  actions,
  className,
  as: Heading = "h3",
}: {
  title: ReactNode;
  description?: ReactNode;
  actions?: ReactNode;
  className?: string;
  as?: "h2" | "h3";
}) {
  return (
    <div className={cn("mb-4 flex flex-wrap items-start justify-between gap-x-6 gap-y-2", className)}>
      <div className="min-w-0">
        <Heading className="text-base font-semibold tracking-tight">{title}</Heading>
        {description && <p className="hint mt-1 text-[13px] leading-relaxed text-muted-foreground">{description}</p>}
      </div>
      {actions && <div className="flex flex-wrap items-center gap-2">{actions}</div>}
    </div>
  );
}

export function Split({
  main,
  aside,
  asideWidth = "380px",
  className,
}: {
  main: ReactNode;
  aside?: ReactNode;
  /** The aside's width from 1280px. */
  asideWidth?: string;
  className?: string;
}) {
  if (!aside) return <div className={className}>{main}</div>;
  return (
    <div
      className={cn("grid items-start gap-6 xl:grid-cols-[minmax(0,1fr)_var(--aside)]", className)}
      style={{ "--aside": asideWidth } as React.CSSProperties}
    >
      <div className="min-w-0">{main}</div>
      <aside className="min-w-0 xl:sticky xl:top-20">{aside}</aside>
    </div>
  );
}

export function Grid({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("grid gap-4 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4", className)} />;
}

export function StatGrid({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      {...props}
      className={cn("mb-6 grid gap-4 sm:grid-cols-2 xl:grid-cols-4", className)}
    />
  );
}

export function Stat({
  label,
  value,
  foot,
  className,
}: {
  label: ReactNode;
  value: ReactNode;
  foot?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("rounded-xl border border-border bg-card p-5 shadow-xs", className)}>
      <div className="text-[13px] font-medium text-muted-foreground">{label}</div>
      <div className="mt-2 text-3xl font-semibold tracking-tight tabular-nums">{value}</div>
      {foot && <div className="mt-1.5 text-[13px] text-muted-foreground">{foot}</div>}
    </div>
  );
}

export function Toolbar({ className, ...props }: ComponentProps<"div">) {
  return <div {...props} className={cn("mb-4 flex flex-wrap items-end gap-3", className)} />;
}

export function DescList({
  items,
  className,
}: {
  items: { label: ReactNode; value: ReactNode; key?: string }[];
  className?: string;
}) {
  return (
    <dl className={cn("grid gap-x-8 gap-y-0 sm:grid-cols-2", className)}>
      {items.map((it, i) => (
        <div key={it.key ?? i} className="flex items-baseline justify-between gap-6 border-b border-divider py-3">
          <dt className="shrink-0 text-muted-foreground">{it.label}</dt>
          <dd className="min-w-0 text-right font-medium">{it.value}</dd>
        </div>
      ))}
    </dl>
  );
}
