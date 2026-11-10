import type { RateCard, SyncItem, SyncRequestItem, SyncState } from "./api.ts";
import { dimensionText, tierText } from "./importReview.ts";

/**
 * The price library's words for a sync's preview. Kept beside their own test
 * (this app's unit runner reaches only `src/lib`): the states are the control
 * plane's, and every one of them has to reach an operator in Chinese.
 */

/** What a sync would do to one model. */
const STATE_TEXT: Record<SyncState, string> = {
  create: "将新增",
  update: "将更新",
  same: "一致",
  flagged: "需手动录入",
  unmatched: "无匹配",
  ambiguous: "多个匹配",
};

/** A state in Chinese; one this build does not know is shown as it came. */
export function syncStateText(state: string): string {
  return (Object.hasOwn(STATE_TEXT, state) ? STATE_TEXT[state as SyncState] : undefined) ?? state;
}

/**
 * The order the preview lists states in: what a sync writes, then what needs
 * a person, then what needs nothing. A state this build does not know goes
 * last.
 */
const STATE_ORDER: string[] = ["create", "update", "ambiguous", "flagged", "unmatched", "same"];

/** `items` by state in [`STATE_ORDER`], each state keeping the order it came in. */
export function byState(items: SyncItem[]): SyncItem[] {
  const rank = (s: string) => {
    const i = STATE_ORDER.indexOf(s);
    return i === -1 ? STATE_ORDER.length : i;
  };
  return [...items].sort((a, b) => rank(a.state) - rank(b.state));
}

/** One map of rates as `输入 $2.5 · 输出 $10`, each rate as the card wrote it. */
function rates(map: Record<string, unknown>): string {
  return Object.entries(map)
    .map(([dimension, rate]) => `${dimensionText(dimension)} $${String(rate)}`)
    .join(" · ");
}

/**
 * Every rate a card carries, one line per tier and per long-context band, in
 * USD per million units: the whole bill a sync would write, so nothing an
 * operator attests is off screen. The `default` tier comes first, the others
 * by name; a part this build does not know is shown by its own key.
 */
export function cardLines(card: RateCard | null): string[] {
  if (!card) return [];
  const tiers = Object.keys(card).sort((a, b) => (a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b)));
  const lines: string[] = [];
  for (const tier of tiers) {
    const parts = (card[tier] ?? {}) as {
      base?: Record<string, unknown>;
      long_context?: { above_input_tokens?: unknown; rates?: Record<string, unknown> }[];
    };
    lines.push(`${tierText(tier)}：${rates(parts.base ?? {})}`);
    for (const band of Array.isArray(parts.long_context) ? parts.long_context : []) {
      lines.push(`${tierText(tier)} · 输入超过 ${String(band.above_input_tokens)} tokens：${rates(band.rates ?? {})}`);
    }
  }
  return lines;
}

/**
 * What a sync sends for `items`: each `create` and `update` with the entry it
 * matched and the card shown, in the order listed, at most `cap` of them.
 * Nothing else is ever sent, so a sync writes only what the preview showed.
 */
export function syncRequest(items: SyncItem[], cap: number): SyncRequestItem[] {
  return items
    .filter((i) => (i.state === "create" || i.state === "update") && i.entry !== null && i.entry.card !== null)
    .slice(0, cap)
    .map((i) => ({
      model_id: i.model_id,
      provider: i.entry!.provider,
      model: i.entry!.model,
      card: i.entry!.card!,
    }));
}
