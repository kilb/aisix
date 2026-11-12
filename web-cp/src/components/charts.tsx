import { useId, useState, type KeyboardEvent } from "react";
import { cn } from "../lib/cn";
import { formatCount, formatRatio } from "../lib/chartFormat";

export interface TrendPoint {
  label: string;
  /** 仅作几何；金额必须另传原始精度的 displayValue。 */
  value: number | null;
  displayValue?: string;
}

export interface TrendChartProps {
  points: readonly TrendPoint[];
  accessibleLabel: string;
  formatValue?: (value: number) => string;
  /** 只有调用方确认的当前期间可用强调色，末期不自动等同今天。 */
  currentIndex?: number;
  emptyLabel?: string;
  className?: string;
}

function knownNumber(value: number | null): value is number {
  return value !== null && Number.isFinite(value);
}

function pointDisplay(point: TrendPoint, formatValue: (value: number) => string): string {
  return point.displayValue ?? (knownNumber(point.value) ? formatValue(point.value) : "未知");
}

/** 整个绘图区是一个足够大的键盘目标，左右键逐期读取精确值。 */
export function TrendChart({ points, accessibleLabel, formatValue = formatCount, currentIndex, emptyLabel = "暂无期间数据", className }: TrendChartProps) {
  const [activeIndex, setActiveIndex] = useState<number | null>(null);
  const tooltipId = useId();
  const values = points.map(point => point.value).filter(knownNumber);
  if (points.length === 0 || values.length === 0) {
    const message = points.length === 0 ? emptyLabel : "期间数值未知，无法绘制趋势";
    return <div role="img" aria-label={`${accessibleLabel}；${message}`} className={cn("flex min-h-56 items-center justify-center text-caption text-muted", className)}>{message}</div>;
  }
  const minimum = Math.min(0, ...values);
  const maximum = Math.max(0, ...values);
  const range = maximum - minimum || 1;
  const y = (value: number) => 12 + (maximum - value) / range * 176;
  const baseline = y(0);
  const slot = 640 / points.length;
  const barWidth = Math.max(0.5, slot * 0.72);
  const selectedIndex = activeIndex === null ? null : Math.min(activeIndex, points.length - 1);
  const selected = selectedIndex === null ? undefined : points[selectedIndex];
  const labels = [...new Set([0, Math.floor((points.length - 1) / 2), points.length - 1])];
  function navigate(event: KeyboardEvent<HTMLButtonElement>) {
    const current = selectedIndex ?? points.length - 1;
    const next = event.key === "Home" ? 0 : event.key === "End" ? points.length - 1 : event.key === "ArrowLeft" ? Math.max(0, current - 1) : event.key === "ArrowRight" ? Math.min(points.length - 1, current + 1) : null;
    if (next !== null) {
      event.preventDefault();
      setActiveIndex(next);
    } else if (event.key === "Escape") {
      event.preventDefault();
      setActiveIndex(null);
    }
  }
  return (
    <div className={cn("min-w-0", className)}>
      <div className="relative">
        <svg role="img" aria-label={accessibleLabel} viewBox="0 0 640 200" preserveAspectRatio="none" className="block h-56 w-full overflow-visible">
          <g aria-hidden="true">
            {[12, 100, 188].map(at => <line key={at} x1="0" x2="640" y1={at} y2={at} className="stroke-ink/7" strokeWidth="1" vectorEffect="non-scaling-stroke" />)}
            {minimum < 0 && <line x1="0" x2="640" y1={baseline} y2={baseline} className="stroke-ink/30" strokeWidth="1" vectorEffect="non-scaling-stroke" />}
            {points.map((point, index) => {
              const x = index * slot + (slot - barWidth) / 2;
              if (!knownNumber(point.value)) return <path key={index} d={`M${x} ${baseline}h${barWidth}`} className="stroke-muted" strokeDasharray="2 3" vectorEffect="non-scaling-stroke" />;
              const height = Math.abs(y(point.value) - baseline);
              return <rect key={index} x={x} y={Math.min(y(point.value), baseline)} width={barWidth} height={height || 1} rx="4" className={index === currentIndex ? "fill-accent" : index >= points.length - 7 ? "fill-ink" : "fill-stone"} />;
            })}
            {selectedIndex !== null && <line x1={(selectedIndex + 0.5) * slot} x2={(selectedIndex + 0.5) * slot} y1="6" y2="194" className="stroke-muted" strokeDasharray="3 4" vectorEffect="non-scaling-stroke" />}
          </g>
        </svg>
        <button
          type="button"
          className="absolute inset-0 min-h-11 w-full cursor-crosshair rounded-chip border-0 bg-transparent p-0 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring"
          aria-label={selected ? `${selected.label}：${pointDisplay(selected, formatValue)}；按左右方向键切换期间` : "查看各期数据，按左右方向键切换期间"}
          aria-describedby={selected ? tooltipId : undefined}
          onFocus={() => setActiveIndex(points.length - 1)}
          onBlur={() => setActiveIndex(null)}
          onPointerMove={event => {
            const bounds = event.currentTarget.getBoundingClientRect();
            setActiveIndex(Math.max(0, Math.min(points.length - 1, Math.floor((event.clientX - bounds.left) / bounds.width * points.length))));
          }}
          onPointerLeave={event => { if (document.activeElement !== event.currentTarget) setActiveIndex(null); }}
          onKeyDown={navigate}
        />
        {selected && <div id={tooltipId} role="tooltip" className="pointer-events-none absolute inset-x-0 top-0 mx-auto w-fit max-w-full break-all rounded-chip border border-line bg-surface px-3 py-2 font-mono text-micro font-normal text-ink">{selected.label} · {pointDisplay(selected, formatValue)}</div>}
      </div>
      <div aria-hidden="true" className="mt-2 flex min-w-0 justify-between gap-2 font-mono text-micro text-muted">
        {labels.map(index => <span key={index} className="min-w-0 break-all">{points[index]?.label}</span>)}
      </div>
      <p className="sr-only">按 Tab 进入图表后，可用左右方向键、Home 和 End 查看各期原值。未知期间不作为零值绘制。</p>
    </div>
  );
}

export interface RankBarItem {
  label: string;
  /** 归一化几何，不参与显示值的计算。 */
  value: number;
  displayValue: string;
}

export interface RankBarsProps {
  items: readonly RankBarItem[];
  accessibleLabel: string;
  emptyLabel?: string;
  className?: string;
}

/** 排行保留全部传入条目，截取与样本范围由调用方明确说明。 */
export function RankBars({ items, accessibleLabel, emptyLabel = "暂无排行数据", className }: RankBarsProps) {
  const values = items.map(item => item.value).filter(Number.isFinite);
  const magnitude = Math.max(0, ...values.map(Math.abs));
  const signed = values.some(value => value < 0);
  const baseline = signed ? 50 : 0;
  const available = signed ? 50 : 100;
  return (
    <div className={cn("min-w-0", className)}>
      <div role="img" aria-label={items.length ? accessibleLabel : `${accessibleLabel}；${emptyLabel}`}>
        {items.length === 0 ? <p className="py-6 text-caption text-muted">{emptyLabel}</p> : (
          <ol aria-hidden="true" className="m-0 grid list-none gap-4 p-0">
            {items.map((item, index) => {
              const width = Number.isFinite(item.value) && magnitude > 0 ? Math.abs(item.value) / magnitude * available : 0;
              return (
                <li key={index} className="min-w-0">
                  <div className="mb-2 flex min-w-0 flex-wrap items-baseline justify-between gap-x-4 gap-y-1 text-caption">
                    <span className="min-w-0 break-all text-ink">{item.label}</span>
                    <span className="min-w-0 break-all font-mono font-normal tabular-nums text-ink">{item.displayValue}</span>
                  </div>
                  <svg aria-hidden="true" viewBox="0 0 100 10" preserveAspectRatio="none" className="block h-2.5 w-full overflow-visible">
                    <rect width="100" height="10" rx="5" className="fill-ink/7" />
                    <rect x={item.value < 0 ? baseline - width : baseline} width={width} height="10" rx="5" className={index === 0 ? "fill-ink" : "fill-graphite"} />
                    {signed && <line x1="50" x2="50" y1="-2" y2="12" className="stroke-muted" vectorEffect="non-scaling-stroke" />}
                  </svg>
                </li>
              );
            })}
          </ol>
        )}
      </div>
      {items.length > 0 && <dl className="sr-only">{items.map((item, index) => <div key={index}><dt>{item.label}</dt><dd>{item.displayValue}</dd></div>)}</dl>}
    </div>
  );
}

export interface BudgetProgressProps {
  value: number | null;
  accessibleLabel: string;
  label?: string;
  valueLabel?: string;
  className?: string;
}

export function BudgetProgress({ value, accessibleLabel, label, valueLabel, className }: BudgetProgressProps) {
  const ratio = value === null || !Number.isFinite(value) ? null : Math.max(0, Math.min(1, value));
  const display = valueLabel ?? (ratio === null ? "未知" : formatRatio(ratio));
  return (
    <div className={cn("min-w-0", className)}>
      {(label || valueLabel) && <div className="mb-2 flex min-w-0 flex-wrap items-baseline justify-between gap-x-4 gap-y-1 text-caption text-ink"><span>{label}</span><span className="min-w-0 break-all font-mono font-normal tabular-nums">{display}</span></div>}
      <svg role={ratio === null ? "img" : "progressbar"} aria-label={ratio === null ? `${accessibleLabel}；${display}` : accessibleLabel} aria-valuemin={ratio === null ? undefined : 0} aria-valuemax={ratio === null ? undefined : 100} aria-valuenow={ratio === null ? undefined : ratio * 100} aria-valuetext={display} viewBox="0 0 100 10" preserveAspectRatio="none" className="block h-2.5 w-full overflow-visible">
        <rect width="100" height="10" rx="5" className="fill-ink/7" />
        {ratio !== null && <rect width={ratio * 100} height="10" rx="5" className="fill-ink" />}
      </svg>
      {ratio === null && !label && !valueLabel && <p className="mt-2 font-mono text-micro text-muted">未知</p>}
    </div>
  );
}
