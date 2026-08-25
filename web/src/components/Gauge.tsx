import { fmtUsd } from "../lib/fmt";

/**
 * 花费表盘。
 *
 * 这个界面里唯一一件放胆的东西，其余都保持安静。它存在的理由不是好看：
 * 运维真正在问的不是「花了多少」，而是**离上限还有多远** —— 而这个上限
 * 是配置里真实存在的数（`RateLimitPolicy.max_spend_micro_usd`），不是我
 * 编出来的刻度。一个数字答不了这个问题，一段弧一眼就答了。
 *
 * 没配上限时它不假装有量程：弧退成一条空轨，旁边写明「未设上限」。给一个
 * 不存在的上限画进度条，比不画更糟。
 */

// 240° 的弧，缺口在**正下方** —— 表盘的惯例，也给中间的数字留出落脚处。
//
// 角度按「从 12 点起顺时针」计，因为那是读表的方式；换算成屏幕坐标时
// y 轴朝下，所以是 x = cx + r·sin θ、y = cy − r·cos θ。用别的参数化很容易
// 把缺口转到侧面去，那样弧看起来就是一个画坏的圆。
const R = 82;
const CX = 100;
const CY = 100;
const SPAN = 240;
const START = 240; // 左下

function at(deg: number): [number, number] {
  const rad = (deg * Math.PI) / 180;
  return [CX + R * Math.sin(rad), CY - R * Math.cos(rad)];
}

function arc(fromDeg: number, toDeg: number): string {
  const [x1, y1] = at(fromDeg);
  const [x2, y2] = at(toDeg);
  const large = Math.abs(toDeg - fromDeg) > 180 ? 1 : 0;
  return `M${x1.toFixed(2)} ${y1.toFixed(2)} A${R} ${R} 0 ${large} 1 ${x2.toFixed(2)} ${y2.toFixed(2)}`;
}

/**
 * 窗口的中文说法。界面通篇中文，说明文字里夹一个 `DAY` 是把配置字段直接
 * 端到了人眼前 —— 用户管的是「一天」，不是那个枚举值。
 */
const WIN: Record<string, { per: string; span: string }> = {
  day: { per: "天", span: "近 24 小时" },
  hour: { per: "小时", span: "近 1 小时" },
  minute: { per: "分钟", span: "近 1 分钟" },
};

/** 把金额拆成「前导零」和「有效位」—— 见 Reading 里的说明。 */
function splitLead(text: string): [string, string] {
  const m = /^([^1-9]*)(.*)$/.exec(text);
  return m ? [m[1] ?? "", m[2] ?? ""] : ["", text];
}

export function Gauge({
  spendMicro,
  ceilingMicro,
  window: win,
}: {
  spendMicro: number | null;
  /** 配置里的上限总额。null = 没有配 —— 此时不画量程。 */
  ceilingMicro: number | null;
  /** 读数覆盖的窗口。没配上限时也要说清 —— 不带窗口的花费数字读不出意思。 */
  window: string | null;
}) {
  const pending = spendMicro === null;
  const ratio =
    ceilingMicro && spendMicro !== null ? Math.min(1, spendMicro / ceilingMicro) : 0;
  const [lead, sig] = splitLead(pending ? "—" : fmtUsd(spendMicro));
  const w = WIN[win ?? "day"] ?? WIN.day!;

  // 越接近上限越警示。这是运维要的信号，不是配色趣味。
  // 没上限就没有「接近」可言，读数回到中性色 —— 染成绿色等于报了个没测的平安。
  const tone = !ceilingMicro || pending
    ? "idle"
    : ratio >= 0.9
      ? "crit"
      : ratio >= 0.7
        ? "warn"
        : "ok";

  return (
    <div className={`gauge gauge-${tone}`}>
      <svg viewBox="4 8 192 178" role="img" aria-label="花费与上限">
        <defs>
          <linearGradient id="gaugeFill" x1="0" y1="1" x2="1" y2="0">
            <stop offset="0%" stopColor="var(--gauge-a)" />
            <stop offset="100%" stopColor="var(--gauge-b)" />
          </linearGradient>
        </defs>

        {/* 空轨：整个量程 */}
        <path d={arc(START, START + SPAN)} className="gauge-track" />

        {/* 已用的那一段 */}
        {ceilingMicro && ratio > 0 && (
          <path d={arc(START, START + SPAN * ratio)} className="gauge-fill" />
        )}

        <text x={CX} y={CY + 2} textAnchor="middle" className="gauge-val">
          <tspan className="lead">{lead}</tspan>
          {sig}
        </text>
        <text x={CX} y={CY + 26} textAnchor="middle" className="gauge-cap">
          {ceilingMicro ? (
            <>
              上限 <tspan className="q">{fmtUsd(ceilingMicro)}</tspan> / {w.per}
            </>
          ) : (
            `${w.span} · 未设上限`
          )}
        </text>
      </svg>
    </div>
  );
}
