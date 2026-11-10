import type { ProposalFlag } from "./api.ts";

/**
 * The review page's words for what a proposal is. Kept beside their own test
 * (this app's unit runner reaches only `src/lib`): the flag names are the
 * control plane's taxonomy, and every one of them has to reach an operator in
 * Chinese.
 */

/** A proposal's flag: what keeps it from becoming a baseline as it stands. */
const FLAG_TEXT: Record<ProposalFlag, string> = {
  bad_number: "价格数字无法读取",
  unsupported_field: "含无法计价的收费项",
  truncated_field: "含分段过多无法完整换算的收费项",
  unsupported_variant: "含无法表达的计价条件",
  time_of_day: "分时段价格",
  invalid_card: "价格不足以构成价目卡",
};

/**
 * The flags that keep a proposal from being confirmed. `unsupported_field`
 * is not one of them: it names a price the rate card cannot meter, which
 * was dropped from an otherwise complete card — see the proposal's `dropped`
 * for what exactly.
 */
const BLOCKING_FLAGS: ReadonlySet<string> = new Set<ProposalFlag>([
  "bad_number",
  "truncated_field",
  "unsupported_variant",
  "time_of_day",
  "invalid_card",
]);

/** Whether `proposal` is held back by a flag, rather than only carrying a dropped price. */
export function flagsBlock(flags: string[]): boolean {
  return flags.some((flag) => !Object.hasOwn(FLAG_TEXT, flag) || BLOCKING_FLAGS.has(flag));
}

/** A proposal's state. */
const STATUS_TEXT: Record<string, string> = {
  pending: "待处理",
  confirmed: "已确认",
  rejected: "已驳回",
  superseded: "已取代",
};

/** A flag in Chinese; one this build does not know is shown as it came. */
export function flagText(flag: string): string {
  return (Object.hasOwn(FLAG_TEXT, flag) ? FLAG_TEXT[flag as ProposalFlag] : undefined) ?? flag;
}

/**
 * The dataset's own price field names, by what they price. A name this build
 * does not know is shown as the dataset writes it, so nothing is hidden.
 */
const FIELD_TEXT: Record<string, string> = {
  storage_searches_kcount: "文件搜索存储",
  audio_hours: "音频时长",
  input_audio_hours: "输入音频时长",
  input_image_mtok: "输入图像 token",
  output_image_mtok: "输出图像 token",
  cache_image_read_mtok: "图像缓存读 token",
  cache_audio_read_mtok: "音频缓存读 token",
  output_reasoning_mtok: "推理 token",
  input_document_kpages: "文档输入页数",
  input_annotated_document_kpages: "带标注文档输入页数",
  input_video_mtok: "输入视频 token",
  output_video_mtok: "输出视频 token",
  output_citation_mtok: "引用 token",
  input_mchars: "输入字符数",
  requests_kcount: "请求次数",
  input_text_messages_kcount: "文本消息条数",
};

/**
 * One price field a card left off, in Chinese: what it prices, the dataset's
 * own name for it, and that the gateway does not meter it — the operator is
 * attesting a bill, and has to know this price is not part of it.
 */
export function droppedFieldText(field: string): string {
  const name = Object.hasOwn(FIELD_TEXT, field) ? FIELD_TEXT[field] : field;
  return `${name}（数据集字段 ${field}，网关不计量）`;
}

/** A state in Chinese; one this build does not know is shown as it came. */
export function statusText(status: string): string {
  return (Object.hasOwn(STATUS_TEXT, status) ? STATUS_TEXT[status] : undefined) ?? status;
}

/**
 * The dimensions the price comparison renders, by the card's own key. Anything
 * a card carries beyond these is refused below rather than shown as covered.
 */
export const COMPARED_DIMENSIONS: [string, string][] = [
  ["input", "输入"],
  ["output", "输出"],
  ["cache_read", "缓存读"],
  ["cache_write", "缓存写"],
  ["cache_write_1h", "1 小时缓存写"],
  ["web_search", "联网搜索"],
];

/** Every rate a card may carry (`Rates`), by the key the engine writes. */
const DIMENSION_TEXT: Record<string, string> = {
  ...Object.fromEntries(COMPARED_DIMENSIONS),
  reasoning: "推理",
  audio_input: "音频输入",
  audio_output: "音频输出",
  image_input: "图像输入",
  image_output: "图像输出",
};

const COMPARED = new Set(COMPARED_DIMENSIONS.map(([d]) => d));

/** A rate's name in Chinese; one this build does not know is shown as it came. */
export function dimensionText(dimension: string): string {
  return (Object.hasOwn(DIMENSION_TEXT, dimension) ? DIMENSION_TEXT[dimension] : undefined) ?? dimension;
}

/** A tier name as the engine spells it (`ServiceTier`); `default` is the one compared. */
const TIER_TEXT: Record<string, string> = {
  default: "标准",
  batch: "批量",
};

/** A tier's name in Chinese; one this build does not know is shown as it came. */
export function tierText(tier: string): string {
  return (Object.hasOwn(TIER_TEXT, tier) ? TIER_TEXT[tier] : undefined) ?? tier;
}

/**
 * The parts of a card the price comparison cannot show, in Chinese: any tier
 * but `default`, any long-context band, and any rate outside
 * [`COMPARED_DIMENSIONS`]. A card carrying one of these bills rates the
 * operator never saw, so the page refuses to confirm it as it stands and names
 * the part here, rather than widening the table to every dimension a card may
 * ever carry.
 *
 * The card's own text is the only source, so a part this comparison does not
 * know — a tier or rate a later engine adds — is reported, never skipped.
 */
export function unshownParts(card: Record<string, unknown> | null): string[] {
  if (!card) return [];
  const parts = new Set<string>();
  for (const [tier, rates] of Object.entries(card)) {
    if (tier !== "default") parts.add(`档位「${TIER_TEXT[tier] ?? tier}」的价格`);
    const tierRates = (rates ?? {}) as { base?: Record<string, unknown>; long_context?: unknown };
    for (const dimension of Object.keys(tierRates.base ?? {})) {
      if (!COMPARED.has(dimension)) parts.add(`价格项「${DIMENSION_TEXT[dimension] ?? dimension}」`);
    }
    if (Array.isArray(tierRates.long_context) && tierRates.long_context.length > 0) {
      parts.add("长上下文分段价");
    }
  }
  return [...parts];
}

/**
 * The dataset's `price_comments` by `provider/model`, read from the uploaded
 * file itself: a proposal does not carry them, and they are what an operator
 * reads to judge an entry the importer could not map — an input-only price
 * whose comment says why. A body that is not the dataset yields nothing, and
 * a provider- or model-wide comment is not taken for an entry's.
 */
export function datasetNotes(text: string): Record<string, string> {
  const notes: Record<string, string> = {};
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return notes;
  }
  if (!Array.isArray(parsed)) return notes;
  for (const provider of parsed as { id?: unknown; models?: unknown }[]) {
    if (!provider || typeof provider.id !== "string" || !Array.isArray(provider.models)) continue;
    for (const model of provider.models as { id?: unknown; price_comments?: unknown }[]) {
      if (!model || typeof model.id !== "string") continue;
      if (typeof model.price_comments === "string" && model.price_comments !== "") {
        notes[`${provider.id}/${model.id}`] = model.price_comments;
      }
    }
  }
  return notes;
}
