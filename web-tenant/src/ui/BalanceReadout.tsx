import { useId, type ReactNode } from "react";
import { fmtMicroCny } from "../lib/money";
import { cn } from "../lib/cn";
import { Card, CardHeader, Metric, span } from "../components/page";

export type ReadoutLevel = "organization" | "member" | "key";

export interface ReadoutStop {
  level: ReadoutLevel;
  label: string;
  remainingMicroCny: number | string | bigint | null;
}

/** 各约束层级剩余额度的说明。 */
const NOTE: Record<ReadoutLevel, string> = {
  organization: "组织余额与额度中较小的一项",
  member: "成员额度的剩余",
  key: "密钥额度的剩余",
};

/** 金额保持精确，货币符号弱化，负号放在符号之前。 */
export function Money({ micro }: { micro: number | string | bigint }) {
  if ((typeof micro === "number" && !Number.isSafeInteger(micro)) || (typeof micro === "string" && !/^-?\d+$/.test(micro))) return <>未知</>;
  const text = fmtMicroCny(micro);
  const negative = text.startsWith("-");
  return (
    <span className="money">
      {negative ? "-" : ""}
      <span className="cur">¥</span>
      {text.replace(/^-?¥/, "")}
    </span>
  );
}

/** 实际可用额度与每一级约束；null 仅表示没有限额。 */
export function BalanceReadout({
  effectiveMicroCny,
  stops,
  binding,
  picker,
  children,
}: {
  effectiveMicroCny: number | string | bigint | null;
  stops: ReadoutStop[];
  binding: ReadoutLevel | null;
  /** 选择要查看的密钥。 */
  picker?: ReactNode;
  /** 各层级之后的说明。 */
  children?: ReactNode;
}) {
  const heading = useId();
  return (
    <Card tone="inverse" className={cn(span.d4, "tablet:col-span-12")} role="region" aria-labelledby={heading}>
      <div id={heading}><CardHeader number={2} title="可用额度" /></div>
      <p className="text-caption text-on-inverse-muted">现在还能花</p>
      {picker}
      <div data-testid="readout-amount"><Metric size="display" className="break-all">
        {effectiveMicroCny === null ? "不限" : <Money micro={effectiveMicroCny} />}
      </Metric></div>
      <ol className="grid gap-3" aria-label="每一级还剩多少">
        {stops.map((s) => {
          const binds = s.level === binding;
          return (
            <li key={s.level} className="grid min-w-0 gap-2 border-t border-ink/12 pt-4" data-level={s.level} data-binding={binds ? "true" : "false"}>
              <span className="flex flex-wrap items-center justify-between gap-2 text-meta font-mono text-on-inverse-muted">
                {s.label}
                {binds && <span className="rounded-full bg-ink/12 px-3 py-1 text-caption text-ink">以此为准</span>}
              </span>
              <span className="break-all text-stat font-normal tabular-nums">{s.remainingMicroCny === null ? "—" : <Money micro={s.remainingMicroCny} />}</span>
              <span className="text-caption text-on-inverse-muted">{NOTE[s.level]}</span>
            </li>
          );
        })}
      </ol>
      {children}
    </Card>
  );
}
