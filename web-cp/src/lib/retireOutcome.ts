import type { ApiError } from "./api.ts";
import { isUnknownOutcome } from "./creditOutcome.ts";
import { messageFor } from "./errors.ts";

/**
 * 删除模型 on an upstream account's 模型 section is two writes: this
 * account's mapping goes first, then the public model is stopped. The
 * second can be refused after the first has succeeded, and that outcome is
 * not a failure — the mapping is gone, the model is not — so it is named as
 * its own kind and never reported as a plain error.
 *
 * A failed answer is only a refusal when the control plane itself answered
 * it with a `4xx` ([`isUnknownOutcome`]): then nothing that write asked for
 * happened. A lost answer (a dropped connection, a proxy's `5xx`, a body that
 * is not the control plane's JSON) says nothing about whether the write
 * committed, so it is `*_unknown`, and the caller reads both resources back
 * before saying anything about them ([`unknownText`]). Neither write is ever
 * sent again on the operator's behalf. `public_model_retired` at either step
 * is a known state, not a failure: the model is already retired, and a
 * retire removes every account's mapping with it.
 *
 * Only the shape of a refusal is read, without importing `ApiError` itself,
 * so this file loads directly under `node:test`; the callers pass real ones.
 */
export type RemoveThenRetire =
  /** Both writes succeeded: the mapping and the model are gone. */
  | { kind: "retired" }
  /** The model was already retired when a write reached it: its mappings and documents are gone. */
  | { kind: "already_retired" }
  /** The mapping removal was refused: nothing else was attempted, nothing changed. */
  | { kind: "removal_failed"; error: unknown }
  /** The mapping removal's answer was lost: it may or may not have happened, and the retire was not sent. */
  | { kind: "removal_unknown"; error: unknown }
  /** The mapping went, and the model stayed live because something still names it. */
  | { kind: "kept_in_use"; error: ApiError; counts: { tenants: number; keys: number; channels: number } }
  /** The mapping went, and the retire was refused for another reason. */
  | { kind: "kept_failed"; error: unknown }
  /** The mapping went, and the retire's answer was lost: the model may or may not be retired. */
  | { kind: "retire_unknown"; error: unknown };

/** The control plane's own error answer, by the fields this file reads. */
function asApiError(e: unknown): ApiError | null {
  const answer = typeof e === "object" && e !== null ? (e as { code?: unknown }) : null;
  return answer !== null && typeof answer.code === "string" ? (e as ApiError) : null;
}

/** A failure as the operator reads it: the Dashboard's text for an API error, anything else as it is. */
function reasonOf(e: unknown): string {
  const error = asApiError(e);
  return error === null ? String(e) : messageFor(error);
}

/** Whether `e` is the control plane saying the model is already retired. */
function isRetired(e: unknown): boolean {
  return !isUnknownOutcome(e) && asApiError(e)?.code === "public_model_retired";
}

/** Sends both writes in order and classifies how they ended. */
export async function removeThenRetire(
  remove: () => Promise<void>,
  retire: () => Promise<unknown>,
): Promise<RemoveThenRetire> {
  try {
    await remove();
  } catch (error) {
    if (isRetired(error)) return { kind: "already_retired" };
    return isUnknownOutcome(error) ? { kind: "removal_unknown", error } : { kind: "removal_failed", error };
  }
  try {
    await retire();
    return { kind: "retired" };
  } catch (e) {
    if (isRetired(e)) return { kind: "already_retired" };
    if (isUnknownOutcome(e)) return { kind: "retire_unknown", error: e };
    const error = asApiError(e);
    if (error !== null && error.code === "public_model_in_use") {
      const counts = (error.counts ?? {}) as { tenants?: number; keys?: number; channels?: number };
      return {
        kind: "kept_in_use",
        error,
        counts: { tenants: counts.tenants ?? 0, keys: counts.keys ?? 0, channels: counts.channels ?? 0 },
      };
    }
    return { kind: "kept_failed", error: e };
  }
}

/** The refusal as its own sentence, for the branch that changed nothing. */
export function removalFailedText(name: string, error: unknown): string {
  return `无法删除「${name}」：${reasonOf(error)}。本账号的映射没有改动，公共模型也没有停用。`;
}

/** The whole success, for a `status`: both removals happened. */
export function retiredText(name: string): string {
  return `已删除「${name}」：本账号的映射和它生成的模型都已移除。`;
}

/** The model was retired before this delete reached it, for a `status`: what the operator asked for already holds. */
export function alreadyRetiredText(name: string): string {
  return `「${name}」已经停用（可能是其他操作员刚刚完成的）：各账号对它的映射和它生成的模型都已移除。`;
}

/**
 * A partial outcome's words, for an `alert` (never a plain failure, and
 * never the success's voice): the mapping went, the model did not, and what
 * is left to clear or what refused it.
 */
export function keptText(
  name: string,
  outcome: Extract<RemoveThenRetire, { kind: "kept_in_use" | "kept_failed" }>,
): string {
  if (outcome.kind === "kept_failed") {
    return `本账号的映射已移除，但「${name}」没有停用，仍然存在：${reasonOf(outcome.error)}`;
  }
  const { tenants, keys, channels } = outcome.counts;
  const what = [
    ...(tenants > 0 || keys > 0 ? [`${tenants} 个租户、${keys} 个密钥的授权`] : []),
    ...(channels > 0 ? [`${channels} 个启用的账号`] : []),
  ];
  if (what.length === 0) {
    return `本账号的映射已移除，但「${name}」没有停用，仍然存在。请到「价格 → 公共模型」查看它的状态。`;
  }
  const clear = [
    ...(tenants > 0 || keys > 0 ? ["从租户与密钥中移除它"] : []),
    ...(channels > 0 ? ["在其他上游账号的「模型」里取消勾选"] : []),
  ];
  return `本账号的映射已移除，但「${name}」没有停用，仍然存在：还有 ${what.join("，以及 ")}指向它，要先${clear.join("，并")}，之后才能在「价格 → 公共模型」停用它。`;
}

/** What a read-back after an unknown outcome found: whether this account still maps the model, and whether it is retired. */
export interface Readback {
  served: boolean;
  retired: boolean;
}

/**
 * An unknown outcome's words, for an `alert`: which answer was lost, then
 * what reading both resources back found, or that the read failed too and
 * the outcome is still unknown. Nothing is claimed that the read-back did
 * not show.
 */
export function unknownText(
  name: string,
  outcome: Extract<RemoveThenRetire, { kind: "removal_unknown" | "retire_unknown" }>,
  readback: Readback | null,
): string {
  const lost =
    outcome.kind === "removal_unknown"
      ? `没能确认「${name}」的删除结果（${reasonOf(outcome.error)}），停用没有发出。`
      : `本账号的映射已移除，但没能确认「${name}」是否已停用（${reasonOf(outcome.error)}）。`;
  if (readback === null) {
    return `${lost}重新读取也失败了，结果仍未知：请稍后点「刷新」查看，再决定是否重试。`;
  }
  const mapping = readback.served ? "本账号仍映射它" : "本账号已不再映射它";
  const model = readback.retired ? "公共模型已停用" : "公共模型仍在用";
  return `${lost}重新读取后：${mapping}，${model}。`;
}
