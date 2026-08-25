/**
 * 一块只读的仪表。
 *
 * 数字按有效位分层：`$0.000` 退成弱色，`247` 留在墨色。
 *
 * 这不是排版趣味，而是这个产品的算术要求。LLM 单次调用的花费常在千分之一
 * 美分量级，等宽两位小数会把它渲染成 `$0.00` —— 而那和「一分钱没花」在屏幕
 * 上完全一样。压低前导零、提亮有效位，读数才在「小」和「零」之间有区别。
 */

/** 把货币符号和前导零切出来。第一个非零数字之后都是有效位。 */
function splitLead(text: string): [lead: string, sig: string] {
  const m = /^([^1-9]*)(.*)$/.exec(text);
  return m ? [m[1] ?? "", m[2] ?? ""] : ["", text];
}

export function Reading({
  label,
  value,
  foot,
  money = false,
}: {
  label: string;
  value: string;
  foot: string;
  /** 花费类读数用锈色 —— 它是唯一真的在扣钱的量。 */
  money?: boolean;
}) {
  // "…" 是还在读，"—" 是读失败。两者都不该按数字排版。
  const pending = value === "…" || value === "—";
  // 复合读数（`3 / 1 / 2`）的斜杠退成弱色，否则会被读成一个数。
  const parts = value.includes(" / ") ? value.split(" / ") : null;
  const [lead, sig] = splitLead(value);

  return (
    <div className={`read${money ? " money" : ""}`}>
      <div className="lab">{label}</div>
      <div className={`val${pending ? " absent" : ""}`}>
        {pending ? (
          value
        ) : parts ? (
          parts.map((p, i) => (
            <span key={i}>
              {i > 0 && <span className="sep">/</span>}
              {p}
            </span>
          ))
        ) : (
          <>
            {lead && <span className="lead">{lead}</span>}
            {sig}
          </>
        )}
      </div>
      <div className="foot">{foot}</div>
    </div>
  );
}
