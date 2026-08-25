export interface Series {
  metric: Record<string, string>;
  values: [number, number][];
}

const W = 880;
const H = 200;
const PL = 56;
const PB = 20;
const PT = 12;
const PR = 10;
// 最多五条：再多线会互相压住、图例也读不完，不如让调用方先聚合。
const HUES = ["var(--accent)", "var(--ok)", "var(--warn)", "var(--live)", "var(--ink-2)"];

/**
 * 多序列折线图。自绘 SVG —— 一个图表库的体积超过整个界面。
 *
 * 三个刻意的取舍：
 * - 首条序列带面积填充。运维先问的是「量级对不对」，体量比线的位置更快
 *   回答这个问题；多条都填会互相盖住，所以只填第一条。
 * - 每条线的右端画一个点。时间序列上运维真正在问的是「现在多少」，而端点
 *   是那个值在图上的位置。
 * - 图例里的名字原样显示。它们是路由、模型名、api_key_id —— 大小写敏感的
 *   标识符，改一个字母就指向了别的东西。
 */
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
  const baseline = y(0);

  const pathOf = (s: Series) =>
    s.values.map(([t, v], i) => `${i ? "L" : "M"}${x(t).toFixed(1)} ${y(v).toFixed(1)}`).join(" ");

  return (
    <div className="chart">
      <div className="scroll">
        {/* 不锁 height：同时给定 width:100% 和固定 height 时，默认的
            preserveAspectRatio="…meet" 会取两者中较小的缩放并把内容居中，
            于是图在宽面板上两侧留出大片空白。让宽度驱动、高度按比例来。 */}
        <svg viewBox={`0 0 ${W} ${H}`} role="img" aria-label="时间序列">
          <defs>
            <linearGradient id="areaFade" x1="0" y1="0" x2="0" y2="1">
              <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.16" />
              <stop offset="100%" stopColor="var(--accent)" stopOpacity="0.01" />
            </linearGradient>
          </defs>

          {[0, 0.25, 0.5, 0.75, 1].map((f) => {
            const yy = PT + f * (H - PT - PB);
            const label = f === 0 || f === 0.5 || f === 1;
            return (
              <g key={f}>
                <line
                  x1={PL}
                  x2={W - PR}
                  y1={yy}
                  y2={yy}
                  className={f === 1 ? "zero-line" : "grid-line"}
                />
                {label && (
                  <text x={PL - 9} y={yy + 3.5} textAnchor="end" className="axis">
                    {fmt(vMax * (1 - f))}
                  </text>
                )}
              </g>
            );
          })}

          {/* 首条序列的面积。多条都填会互相盖住，所以只填一条。 */}
          {shown[0] && (
            <path
              d={`${pathOf(shown[0])} L${x(tMax).toFixed(1)} ${baseline.toFixed(1)} L${x(tMin).toFixed(1)} ${baseline.toFixed(1)} Z`}
              fill="url(#areaFade)"
              stroke="none"
            />
          )}

          {shown.map((s, i) => (
            <path
              key={i}
              d={pathOf(s)}
              fill="none"
              stroke={HUES[i % HUES.length]}
              strokeWidth="1.5"
              strokeLinejoin="round"
              strokeLinecap="round"
            />
          ))}

          {/* 端点：「现在」的那个值在图上的位置。 */}
          {shown.map((s, i) => {
            const last = s.values.at(-1);
            if (!last) return null;
            return (
              <circle
                key={i}
                cx={x(last[0])}
                cy={y(last[1])}
                r="2.6"
                fill={HUES[i % HUES.length]}
                className="now"
              />
            );
          })}
        </svg>
      </div>

      <div className="legend">
        {shown.map((s, i) => {
          const last = s.values.at(-1);
          return (
            <span key={i} className="item">
              <span className="swatch" style={{ background: HUES[i % HUES.length] }} />
              {/* 原样显示：这些是大小写敏感的标识符。 */}
              {Object.values(s.metric)[0] ?? "(全部)"}
              {last && <span style={{ color: "var(--faint)" }}>{fmt(last[1])}</span>}
            </span>
          );
        })}
      </div>
    </div>
  );
}
