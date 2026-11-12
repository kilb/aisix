import type { ChannelModelSaved, CooldownSettings, PublicModel, ServedBy } from "./api.ts";
import { sameRouting } from "./publicRouting.ts";
import { cooldownProblem } from "./cooldown.ts";

/**
 * A public model's routing targets, as 模型与路由 edits them in place.
 *
 * Each target is one upstream account's mapping of the public model (a
 * channel model): the upstream name the account is asked for, its priority
 * and its weight. The form writes a target with
 * `PUT /admin/provider-keys/{account}/channel-models/{public model}`, which
 * creates or updates the mapping and regenerates the routing document in the
 * same transaction, so the targets the gateway reads are always the
 * accounts' mappings.
 */

/** One target as typed: the numbers as text, so a cleared box is not a zero. */
export interface TargetDraft {
  /** The upstream name; empty means the public model's own name is sent. */
  upstream: string;
  priority: string;
  weight: string;
  cooldown?: CooldownSettings | null;
  cooldownProblem?: string | null;
}

/** What one target write sends; an absent field keeps what the mapping stores. */
export interface TargetBody {
  upstream_model?: string | null;
  priority?: number;
  weight?: number;
  enabled?: boolean;
  cooldown?: CooldownSettings | null;
}

/** The fields of the draft each write names, in the order a row shows them. */
export type TargetField = "upstream" | "priority" | "weight" | "cooldown";

/** The smallest and largest value a priority or weight can be sent as (a 32-bit integer). */
const MIN_I32 = -2_147_483_648;
const MAX_I32 = 2_147_483_647;

/** A new target's numbers before the operator types: what the account's own 模型 section starts an unserved row at. */
export const NEW_TARGET: TargetDraft = { upstream: "", priority: "0", weight: "1" };

/** The accounts a public model routes to now: the enabled mappings. A disabled one is not a target. */
export function targetsOf(view: PublicModel): ServedBy[] {
  return view.served_by.filter((s) => s.enabled);
}

/** The draft a stored mapping starts as. */
export function draftOf(s: Pick<ServedBy, "upstream_model" | "priority" | "weight" | "cooldown">): TargetDraft {
  return { upstream: s.upstream_model ?? "", priority: String(s.priority), weight: String(s.weight), ...(s.cooldown == null ? {} : { cooldown: s.cooldown }) };
}

/**
 * The draft a row shows: the stored mapping as it reads now, with only the
 * fields the operator typed laid over it.
 *
 * A field the operator has not touched follows every later read, so a reload
 * after another operator's change shows that change, and a save never sends
 * the value it had when the row was first edited.
 */
export function shownDraft(
  stored: Pick<ServedBy, "upstream_model" | "priority" | "weight" | "cooldown">,
  typed: Partial<TargetDraft> | undefined,
): TargetDraft {
  return { ...draftOf(stored), ...typed };
}

function upstreamOf(typed: string): string | null {
  const name = typed.trim();
  return name === "" ? null : name;
}

/**
 * The write that takes `stored` to `draft`: only the fields that differ, so a
 * field left alone is never rewritten. Null when nothing differs.
 */
export function changedBody(draft: TargetDraft, stored: ServedBy): TargetBody | null {
  const body: TargetBody = {};
  const upstream = upstreamOf(draft.upstream);
  if (upstream !== (stored.upstream_model ?? null)) body.upstream_model = upstream;
  if (draft.priority.trim() !== String(stored.priority)) body.priority = Number(draft.priority);
  if (draft.weight.trim() !== String(stored.weight)) body.weight = Number(draft.weight);
  if (!sameRouting({ cooldown: draft.cooldown ?? null }, { cooldown: stored.cooldown ?? null })) body.cooldown = draft.cooldown ?? null;
  return Object.keys(body).length === 0 ? null : body;
}

/** The write that makes an account a target: every field, and the mapping enabled. */
export function addedBody(draft: TargetDraft): TargetBody {
  return {
    upstream_model: upstreamOf(draft.upstream),
    priority: Number(draft.priority),
    weight: Number(draft.weight),
    enabled: true,
    ...(draft.cooldown === undefined ? {} : { cooldown: draft.cooldown }),
  };
}

/**
 * What cannot be sent at all, by field: an empty number, or one past a
 * 32-bit integer. Every other rule (a weight's range, the upstream name's
 * shape) is the control plane's, and its refusal is shown at the field.
 */
export function unsendable(draft: TargetDraft, stored?: ServedBy): Partial<Record<TargetField, string>> {
  const problems: Partial<Record<TargetField, string>> = {};
  if (draft.cooldownProblem) problems.cooldown = draft.cooldownProblem;
  if (!stored || !sameRouting({ cooldown: draft.cooldown ?? null }, { cooldown: stored.cooldown ?? null })) {
    const problem = cooldownProblem(draft.cooldown);
    if (problem) problems.cooldown = problem;
  }
  for (const [field, label] of [
    ["priority", "优先级"],
    ["weight", "权重"],
  ] as const) {
    const typed = draft[field].trim();
    const value = Number(typed);
    if (typed === "") problems[field] = `请填写${label}`;
    else if (!Number.isInteger(value)) problems[field] = `${label}必须是整数`;
    else if (value < MIN_I32 || value > MAX_I32) problems[field] = `${label}超出可以保存的范围`;
  }
  return problems;
}

/**
 * `view` with the account's mapping as its write answered it: what the form
 * shows when the read-back after a save fails, so a row that was written is
 * neither shown as it was nor sent again. Ordered as the control plane
 * orders them (priority, then weight, highest first).
 */
export function withSaved(
  view: PublicModel,
  accountId: string,
  saved: ChannelModelSaved,
  account: { display_name: string; groups: string[] },
): PublicModel {
  const before = view.served_by.find((s) => s.provider_key_id === accountId);
  const entry: ServedBy = {
    provider_key_id: accountId,
    display_name: before?.display_name ?? account.display_name,
    groups: before?.groups ?? account.groups,
    upstream_model: saved.upstream_model,
    priority: saved.priority,
    weight: saved.weight,
    enabled: saved.enabled,
    ...(saved.cooldown === undefined ? {} : { cooldown: saved.cooldown }),
  };
  const served_by = [...view.served_by.filter((s) => s.provider_key_id !== accountId), entry].sort(
    (a, b) => b.priority - a.priority || b.weight - a.weight,
  );
  return { ...view, served_by };
}
