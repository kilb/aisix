export interface Series {
  metric: Record<string, string>;
  values: [number, number][];
}

const W = 880;
const H = 190;
const PL = 54;
const PB = 22;
const PT = 10;
const PR = 8;
// 最多五条：再多线会互相压住、图例也读不完，不如让调用方先聚合。
const HUES = ["var(--accent)", "var(--ok)", "var(--warn)", "var(--crit)", "var(--ink-2)"];

/** 多序列折线图。自绘 SVG —— 一个图表库的体积超过整个界面。 */
export function Chart({ series, fmt }: { series: Series[]; fmt: (v: number) => string }) {
  let tMin = Infinity;
  let tMax = -Infinity;
  let vMax = 0;
  for (const s of series) {
    for (const [t, v] of s.values) {
      if (t < tMin) tMin = t;
      if (t > tMax) tMax = t;
      if (v > vMax) vMax = v;
    }
  }
  // 全零和读不到要分开说：前者是「这段时间确实没流量」。
  if (!Number.isFinite(tMin) || vMax <= 0) {
    return <p className="hint">该时段全部为零。</p>;
  }

  const x = (t: number) => PL + ((t - tMin) / Math.max(1, tMax - tMin)) * (W - PL - PR);
  const y = (v: number) => PT + (1 - v / vMax) * (H - PT - PB);
  const shown = series.slice(0, HUES.length);

  return (
    <>
      <div className="scroll">
        <svg viewBox={`0 0 ${W} ${H}`} width="100%" height={H} role="img" aria-label="时间序列">
          {[0, 0.5, 1].map((f) => {
            const yy = PT + f * (H - PT - PB);
            return (
              <g key={f}>
                <line x1={PL} x2={W - PR} y1={yy} y2={yy} stroke="var(--line)" strokeWidth="1" />
                <text
                  x={PL - 7}
                  y={yy + 4}
                  textAnchor="end"
                  fontSize="10"
                  fontFamily="var(--mono)"
                  fill="var(--muted)"
                >
                  {fmt(vMax * (1 - f))}
                </text>
              </g>
            );
          })}
          {shown.map((s, i) => (
            <path
              key={i}
              d={s.values
                .map(([t, v], j) => `${j ? "L" : "M"}${x(t).toFixed(1)} ${y(v).toFixed(1)}`)
                .join(" ")}
              fill="none"
              stroke={HUES[i % HUES.length]}
              strokeWidth="1.6"
              strokeLinejoin="round"
              strokeLinecap="round"
            />
          ))}
        </svg>
      </div>
      <div style={{ display: "flex", gap: 6, flexWrap: "wrap", marginTop: 10 }}>
        {shown.map((s, i) => (
          <span key={i} className="pill" style={{ borderColor: "transparent" }}>
            <span className="dot" style={{ background: HUES[i % HUES.length] }} />
            {Object.values(s.metric)[0] ?? "(全部)"}
          </span>
        ))}
      </div>
    </>
  );
}
