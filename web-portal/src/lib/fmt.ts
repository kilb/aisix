/**
 * 金额一律 micro-USD 整数。显示时按有效位分层：这个产品的花费到千分之一
 * 美分，两位小数会把真实花费渲染成 $0.00 —— 那和「一分钱没花」在屏幕上
 * 无法区分。
 */
export function usd(micro: number | null): string {
  if (micro === null) return "—";
  const v = micro / 1_000_000;
  const abs = Math.abs(v);
  const digits = abs === 0 ? 2 : abs < 0.01 ? 6 : abs < 1 ? 4 : 2;
  return `${v < 0 ? "−" : ""}$${abs.toFixed(digits)}`;
}

export function count(n: number | null): string {
  if (n === null) return "—";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(Math.round(n));
}

/** 把金额切成「前导零」与「有效位」，让弱色压住前者。 */
export function splitLead(text: string): [string, string] {
  const m = /^([^1-9]*)(.*)$/.exec(text);
  return m ? [m[1] ?? "", m[2] ?? ""] : ["", text];
}
