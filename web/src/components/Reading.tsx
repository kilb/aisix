/**
 * 账页上的一条分录。
 *
 * 项目在左，数字右对齐压在栏线上，注释缩在项目下面一行 —— 账簿本来的排法。
 *
 * 数字按有效位分层：`$0.000` 的前导零退成灰墨，`247` 留在黑墨。
 *
 * 这不是排版趣味，而是这个产品的算术要求。LLM 单次调用的花费常在千分之一
 * 美分量级，等宽两位小数会把它渲染成 `$0.00` —— 而那和「一分钱没花」在纸
 * 上完全一样。压低前导零、留住有效位，读数才在「小」和「零」之间有区别。
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
}: {
  label: string;
  value: string;
  foot: string;
}) {
  // "…" 是还在读，"—" 是读失败。两者都不该按数字排版。
  const pending = value === "…" || value === "—";
  // 复合读数（`3 / 1 / 2`）的斜杠退成灰墨，否则会被读成一个数。
  const parts = value.includes(" / ") ? value.split(" / ") : null;
  const [lead, sig] = splitLead(value);

  return (
    <div className="entry">
      <span className="lab">{label}</span>
      <span className={`val${pending ? " absent" : ""}`}>
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
      </span>
      <span className="foot">{foot}</span>
    </div>
  );
}
