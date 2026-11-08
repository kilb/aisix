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
  unsupported_variant: "含无法表达的计价条件",
  time_of_day: "分时段价格",
  invalid_card: "价格不足以构成价目卡",
};

/** A proposal's state. */
const STATUS_TEXT: Record<string, string> = {
  pending: "待处理",
  confirmed: "已确认",
  rejected: "已驳回",
  superseded: "已取代",
};

/** A flag in Chinese; one this build does not know is shown as it came. */
export function flagText(flag: string): string {
  return FLAG_TEXT[flag as ProposalFlag] ?? flag;
}

/** A state in Chinese; one this build does not know is shown as it came. */
export function statusText(status: string): string {
  return STATUS_TEXT[status] ?? status;
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
