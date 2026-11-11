/**
 * Whether a price card the JSON view holds can be shown as fields.
 *
 * The JSON view takes any document that parses, so `{"default":
 * {"long_context": [null]}}` is a legal document there and no legal card.
 * The field view reads every tier's `base` and every band's `rates`, so
 * showing one of those as fields throws while rendering and takes the whole
 * Dashboard with it (nothing above it catches a render error).
 *
 * This is a shape check only: it says whether the field view can read the
 * document, never whether the control plane would accept it. A card that
 * fails it stays in the JSON view with the problem named, so the operator
 * corrects the text they typed instead of losing it.
 */

/** Why `card` cannot be shown as fields, or null when it can. */
export function cardShapeProblem(card: unknown): string | null {
  if (!isObject(card)) return '价格卡应是一个对象：形如 {"default": {"base": {...}}}';
  for (const [tier, value] of Object.entries(card)) {
    const at = `/${segment(tier)}`;
    if (!isObject(value)) return `${at}：档位应是一个对象`;
    if (value.base !== undefined) {
      if (!isObject(value.base)) return `${at}/base：基础价应是一个对象，每个价格为字符串`;
      const problem = rateProblem(value.base, `${at}/base`);
      if (problem !== null) return problem;
    }
    if (value.long_context === undefined) continue;
    if (!Array.isArray(value.long_context)) return `${at}/long_context：长上下文档应是一个数组`;
    for (const [n, band] of (value.long_context as unknown[]).entries()) {
      const where = `${at}/long_context/${n}`;
      if (!isObject(band)) return `${where}：每个长上下文档应是一个对象，含 above_input_tokens 与 rates`;
      if (band.above_input_tokens !== null && typeof band.above_input_tokens === "object") {
        return `${where}/above_input_tokens：阈值不能是对象或数组`;
      }
      if (band.rates !== undefined) {
        if (!isObject(band.rates)) return `${where}/rates：长上下文档的价格应是一个对象`;
        const problem = rateProblem(band.rates, `${where}/rates`);
        if (problem !== null) return problem;
      }
    }
  }
  return null;
}

// 只检查字段读取所需的类型，金额是否合法仍由控制面决定。
function rateProblem(rates: Record<string, unknown>, at: string): string | null {
  for (const [dimension, rate] of Object.entries(rates)) {
    if (typeof rate !== "string") return `${at}/${segment(dimension)}：价格应是字符串`;
  }
  return null;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** One JSON Pointer segment, as the control plane writes a refusal's path. */
function segment(s: string): string {
  return s.replace(/~/g, "~0").replace(/\//g, "~1");
}
