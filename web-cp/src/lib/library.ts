import type { RateCard, SyncCandidate, SyncItem, SyncRequestItem, SyncState } from "./api.ts";
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
 * matched — an ambiguous model's with the candidate `chosen` for it — and the
 * card shown, in the order listed, at most `cap` of them. Nothing else is
 * ever sent, so a sync writes only what the preview showed.
 */
export function syncRequest(
  items: SyncItem[],
  cap: number,
  chosen: Record<string, SyncCandidate | undefined> = {},
): SyncRequestItem[] {
  const sent: SyncRequestItem[] = [];
  for (const item of items) {
    // An ambiguous model is written only from the candidate chosen for it,
    // and any other only when it would create or update.
    const from = item.state === "ambiguous" ? chosen[item.model_id] : undefined;
    const source = from ?? item.entry;
    const state = from ? from.state : item.state;
    if ((state !== "create" && state !== "update") || !source?.card) continue;
    sent.push({ model_id: item.model_id, provider: source.provider, model: source.model, card: source.card });
    if (sent.length === cap) break;
  }
  return sent;
}

/** One long-context band of a card: its tier, threshold, and its input and output rates. */
export interface LongContextLine {
  tier: string;
  above_input_tokens: number;
  input: string;
  output: string;
}

/**
 * Every long-context band a card carries, one per tier and band, in tier
 * order: what a request past the threshold is billed at, which a baseline
 * without the band does not carry.
 */
export function longContextLines(card: RateCard | null): LongContextLine[] {
  if (!card) return [];
  const lines: LongContextLine[] = [];
  for (const tier of Object.keys(card).sort((a, b) => (a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b)))) {
    const part = (card[tier] ?? {}) as {
      long_context?: { above_input_tokens?: unknown; rates?: Record<string, unknown> }[];
    };
    for (const band of Array.isArray(part.long_context) ? part.long_context : []) {
      const above = Number(band.above_input_tokens);
      if (!Number.isFinite(above)) continue;
      lines.push({
        tier,
        above_input_tokens: above,
        input: String(band.rates?.input ?? ""),
        output: String(band.rates?.output ?? ""),
      });
    }
  }
  return lines;
}

/** A threshold as the Dashboard writes it: `272K` above a thousand, the count below it. */
export function thresholdText(tokens: number): string {
  return tokens >= 1000 ? `${Number((tokens / 1000).toFixed(1))}K` : String(tokens);
}

/** A long-context band in one line, as a candidate's tag carries it: `>272K 输入：$5 / $22.5 每百万`. */
export function longContextText(line: LongContextLine): string {
  const tier = line.tier === "default" ? "" : `${tierText(line.tier)} `;
  return `${tier}>${thresholdText(line.above_input_tokens)} 输入：$${line.input} / $${line.output} 每百万`;
}

/**
 * What a sync would change about the long-context bands, in Chinese: `null`
 * when both sides carry one, or neither does. Display only — the state is
 * decided by comparing the cards, as before.
 */
export function longContextChange(card: RateCard | null, current: RateCard | null): string | null {
  const entry = longContextLines(card).length > 0;
  const now = longContextLines(current).length > 0;
  if (entry === now) return null;
  return entry ? "当前基准价缺少长上下文档，同步后将补上" : "价格库条目没有长上下文档，同步将去掉它";
}
