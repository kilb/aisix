import type { ApiError } from "./api.ts";
import { messageFor } from "./errors.ts";

/**
 * 删除模型 on an upstream account's 模型 section is two writes: this
 * account's mapping goes first, then the public model is stopped. The
 * second can be refused after the first has succeeded, and that outcome is
 * not a failure — the mapping is gone, the model is not — so it is named as
 * its own kind and never reported as a plain error.
 *
 * Only the shape of a refusal is read, without importing `ApiError` itself,
 * so this file loads directly under `node:test`; the callers pass real ones.
 */
export type RemoveThenRetire =
  /** Both writes succeeded: the mapping and the model are gone. */
  | { kind: "retired" }
  /** The mapping removal failed: nothing else was attempted, nothing changed. */
  | { kind: "removal_failed"; error: unknown }
  /** The mapping went, and the model stayed live because something still names it. */
  | { kind: "kept_in_use"; error: ApiError; counts: { tenants: number; keys: number; channels: number } }
  /** The mapping went, and the retire failed for another reason. */
  | { kind: "kept_failed"; error: unknown };

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

/** Sends both writes in order and classifies how they ended. */
export async function removeThenRetire(
  remove: () => Promise<void>,
  retire: () => Promise<unknown>,
): Promise<RemoveThenRetire> {
  try {
    await remove();
  } catch (error) {
    return { kind: "removal_failed", error };
  }
  try {
    await retire();
    return { kind: "retired" };
  } catch (e) {
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

/** The failure as its own sentence, for the branch that changed nothing. */
export function removalFailedText(name: string, error: unknown): string {
  return `无法删除「${name}」：${reasonOf(error)}。本账号的映射没有改动，公共模型也没有停用。`;
}

/** The whole success, for a `status`: both removals happened. */
export function retiredText(name: string): string {
  return `已删除「${name}」：本账号的映射和它生成的模型都已移除。`;
}

/**
 * A partial outcome's words, for an `alert` (never a plain failure, and
 * never the success's voice): the mapping went, the model did not, and what
 * is left to clear or what failed.
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
