/** 展示层格式化。每条的取舍都来自旧界面踩过的坑，原因随代码保留。 */

export function fmtInt(n: unknown): string {
  return (Number(n) || 0).toLocaleString("en-US");
}

export function fmtCompact(n: unknown): string {
  const v = Number(n) || 0;
  if (v >= 1e9) return `${(v / 1e9).toFixed(2)}B`;
  if (v >= 1e6) return `${(v / 1e6).toFixed(2)}M`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}k`;
  return fmtInt(v);
}

/**
 * 花费以 micro-USD 计数，除回美元。
 *
 * 精度随量级走：LLM 单次调用常在千分之一美元量级，固定两位小数会把真实
 * 花费显示成 `$0.00`，而那和「没有花费」在界面上无法区分。
 */
export function fmtUsd(micro: unknown): string {
  const usd = (Number(micro) || 0) / 1e6;
  if (usd === 0) return "$0";
  if (usd < 0.01) {
    return `$${usd.toFixed(6).replace(/0+$/, "").replace(/\.$/, "")}`;
  }
  if (usd < 1) return `$${usd.toFixed(4)}`;
  return `$${usd.toLocaleString("en-US", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })}`;
}

/** 正整数或 null。空框表示「不设」，不是 0。 */
export function intOrNull(v: unknown): number | null {
  const s = String(v ?? "").trim();
  if (!s) return null;
  const n = Number(s);
  return Number.isInteger(n) && n > 0 ? n : null;
}

/**
 * 逗号分隔的模型列表。
 *
 * 空列表等于「拒绝所有模型」，而运维清空这个框想表达的几乎一定是
 * 「不限制」。所以空则回到 `["*"]`。
 */
export function modelsOrAll(v: unknown): string[] {
  const list = String(v ?? "")
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);
  return list.length ? list : ["*"];
}

/**
 * 管理 API 的每项资源各自成败：整体仍是 200，读不到的那一项是 `{error}`。
 *
 * 所以 `listOf` 对「读不到」和「真的是空的」给出同一个空数组，界面会把前者
 * 显示成后者 —— 而这两件事要采取的动作完全相反。凡是按资源清单渲染的地方
 * 都得先问这一句。
 *
 * 返回的字符串只作信号用，**不要显示**：它带着管理 API 的内网地址。
 */
export function resError(v: unknown): string | null {
  if (v && typeof v === "object" && !Array.isArray(v)) {
    const e = (v as { error?: unknown }).error;
    if (typeof e === "string" && e) return e;
  }
  return null;
}

/**
 * 管理 API 每条是 `{id, revision, value:{...}}` 信封，拆平成
 * `{id, revision, ...字段}`。
 *
 * 不拆的话 `display_name` 恒为 undefined，界面会安静地退化成显示裸 id。
 */
export function listOf(v: unknown): Record<string, unknown>[] {
  const arr = Array.isArray(v)
    ? v
    : v && typeof v === "object" && Array.isArray((v as { items?: unknown[] }).items)
      ? ((v as { items: unknown[] }).items)
      : [];
  return arr.map((e) => {
    if (e && typeof e === "object") {
      const rec = e as Record<string, unknown>;
      if (rec.value && typeof rec.value === "object") {
        return { id: rec.id, revision: rec.revision, ...(rec.value as Record<string, unknown>) };
      }
      return rec;
    }
    return {} as Record<string, unknown>;
  });
}
