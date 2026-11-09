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
export function Money({ micro }: { micro: number }) {
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
 * large, and beneath it a route line with a stop for each level that limits
 * a request (organisation, member, key), each with what it has left. The
 * level that binds is the filled stop marked 以此为准, so the reader sees
 * which level stops the next request. `null` for the effective amount means
 * no level limits it.
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
  /** A control that chooses what the readout is for (the key), shown under the heading. */
  picker?: ReactNode;
  /** Notes under the route line. */
  children?: ReactNode;
}) {
  const heading = useId();
  return (
    <div className="panel readout" role="region" aria-labelledby={heading}>
      <h2 id={heading}>可用额度</h2>
      {picker}
      <div className="readout-figure">
        <p className="readout-label">现在还能花</p>
        <p className="readout-amt display" data-testid="readout-amount">
          {effectiveMicroCny === null ? <span className="readout-open">不限</span> : <Money micro={effectiveMicroCny} />}
        </p>
      </div>
      <ol className="levels" aria-label="每一级还剩多少" style={{ "--n": stops.length } as React.CSSProperties}>
        {stops.map((s) => {
          const binds = s.level === binding;
          return (
            <li key={s.level} className={binds ? "bind" : undefined} data-level={s.level} data-binding={binds ? "true" : "false"}>
              <span className="lv">
                {s.label}
                {binds && <span className="bind-plate">以此为准</span>}
              </span>
              <span className="box" aria-hidden="true">
                <span className="stop" />
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
