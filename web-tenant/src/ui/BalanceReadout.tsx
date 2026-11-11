import { useId, type ReactNode } from "react";
import { fmtMicroCny } from "../lib/money";

export type ReadoutLevel = "organization" | "member" | "key";

export interface ReadoutStop {
  level: ReadoutLevel;
  label: string;
  remainingMicroCny: number | null;
}

/** What each level's remaining amount is, under its figure. */
const NOTE: Record<ReadoutLevel, string> = {
  organization: "组织余额与额度中较小的一项",
  member: "成员额度的剩余",
  key: "密钥额度的剩余",
};

/** An amount with its currency sign muted and the minus before it: `-¥0.011`. */
export function Money({ micro }: { micro: number | string | bigint }) {
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

/**
 * The portal's one distinctive moment: what the caller can still spend,
 * large, and beneath it one tile for each level that limits a request
 * (organisation, member, key), each with what it has left. The level that
 * binds is the outlined tile marked 以此为准, so the reader sees which level
 * stops the next request. `null` for the effective amount means no level
 * limits it.
 */
export function BalanceReadout({
  effectiveMicroCny,
  stops,
  binding,
  picker,
  children,
}: {
  effectiveMicroCny: number | null;
  stops: ReadoutStop[];
  binding: ReadoutLevel | null;
  /** A control that chooses what the readout is for (the key), shown beside the heading. */
  picker?: ReactNode;
  /** Notes under the levels. */
  children?: ReactNode;
}) {
  const heading = useId();
  return (
    <div className="panel readout" role="region" aria-labelledby={heading}>
      <div className="flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
        <div className="readout-figure min-w-0">
          <h3 id={heading} className="text-base font-semibold tracking-tight">
            可用额度
          </h3>
          <p className="readout-label mt-1 text-[13px] text-muted-foreground">现在还能花</p>
        </div>
        {picker}
      </div>
      <p className="readout-amt display" data-testid="readout-amount">
        {effectiveMicroCny === null ? <span className="readout-open">不限</span> : <Money micro={effectiveMicroCny} />}
      </p>
      <ol className="levels" aria-label="每一级还剩多少">
        {stops.map((s) => {
          const binds = s.level === binding;
          return (
            <li key={s.level} className={binds ? "bind" : undefined} data-level={s.level} data-binding={binds ? "true" : "false"}>
              <span className="lv">
                {s.label}
                {binds && <span className="bind-plate">以此为准</span>}
              </span>
              <span className="m">{s.remainingMicroCny === null ? "—" : <Money micro={s.remainingMicroCny} />}</span>
              <span className="k">{NOTE[s.level]}</span>
            </li>
          );
        })}
      </ol>
      {children}
    </div>
  );
}
