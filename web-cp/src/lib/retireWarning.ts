import type { ApiError } from "./api.ts";
import { messageFor } from "./errors.ts";

/** A refusal as this module reads it: `counts` is the only field it adds to an `ApiError`. */
export type Refusal = Pick<ApiError, "message" | "code"> & { counts: Record<string, number> | null };

/**
 * The words a public model's retire is explained with, shared by every
 * surface that offers one: 价格 · 公共模型, an upstream account's 模型
 * section, and the model list. Kept beside its own test (this app's unit
 * runner reaches only `src/lib`), so the sentence cannot drift between them.
 *
 * Only what a `PublicModel` view carries is read, so a page that has not
 * read the counts cannot call this and print zeros: pass the view, or say
 * something else.
 */
export interface Retirable {
  name: string;
  tenants: number;
  keys: number;
  /** One per account with a channel row; a retired model's are all disabled. */
  served_by: { enabled: boolean; provider_key_id: string }[];
}

/** The counts a `409 public_model_in_use` carries. */
export type UseCounts = { tenants?: number; keys?: number; channels?: number };

/**
 * What the retire confirmation says will happen. While a grant or an enabled
 * serving account still holds the model the retire is refused, so it says
 * that, with the counts and what to clear. Otherwise the retire succeeds: it
 * says what goes — the routing document, the direct documents generated for
 * each account, and the accounts' mappings, which a later restore does not
 * bring back — and that the name stays the model's own.
 */
export function retireWarning(model: Retirable): string {
  const accounts = model.served_by.filter((s) => s.enabled).length;
  const holders = [
    ...(model.tenants > 0 || model.keys > 0 ? [`${model.tenants} 个租户、${model.keys} 个密钥的授权`] : []),
    ...(accounts > 0 ? [`${accounts} 个启用的账号`] : []),
  ];
  if (holders.length === 0) {
    const mappings = model.served_by.length > 0 ? `，以及 ${model.served_by.length} 个账号上的映射设置` : "";
    return `停用后不能恢复原有的账号映射：将删除它的路由文档和为各账号生成的直连文档${mappings}。名称「${model.name}」以后不能再用于公共模型，只能在「价格 → 公共模型」恢复它本身，恢复后要重新为账号勾选映射。`;
  }
  const clear = [
    ...(model.tenants > 0 || model.keys > 0 ? ["从租户与密钥中移除它"] : []),
    ...(accounts > 0 ? ["在上游账号的「模型」里取消勾选"] : []),
  ];
  return `仍有 ${holders.join("，以及 ")}指向它，停用会被拒绝，什么也不会删除：请先${clear.join("，并")}。`;
}

/**
 * `model` as it will stand once `providerKeyId`'s channel row is removed: a
 * surface that removes one account's mapping before retiring asks with this,
 * so what it says about the retire is about the state the retire will meet.
 */
export function withoutServing(model: Retirable, providerKeyId: string): Retirable {
  return { ...model, served_by: model.served_by.filter((s) => s.provider_key_id !== providerKeyId) };
}

/**
 * What 删除 says it will do, on 价格 · 公共模型. A delete takes the accounts'
 * mappings with it, so an enabled serving account never refuses it — that is
 * the difference from a retire, and why the sentence is its own. A live
 * grant or a live key selection still refuses it, and then nothing goes.
 */
export function deleteWarning(model: Retirable): string {
  if (model.tenants > 0 || model.keys > 0) {
    return `仍有 ${model.tenants} 个租户、${model.keys} 个密钥的授权指向它，删除会被拒绝，什么也不会删除：请先从租户与密钥中移除它。`;
  }
  const mappings =
    model.served_by.length > 0
      ? `先删除 ${model.served_by.length} 个账号对它的映射，再删除它的路由文档和各账号的直连文档`
      : "删除它的路由文档和为各账号生成的直连文档";
  return `删除后不能恢复原有的账号映射：将${mappings}。名称「${model.name}」以后不能再用于公共模型，只能在这里恢复它本身，恢复后要重新为账号勾选映射。`;
}

/**
 * What still names a public model, by name, so the operator knows what to
 * clear. Typed by the fields it reads rather than `ApiError`, which strip-only
 * TypeScript cannot load (see `retireOutcome.ts`); every caller passes a real one.
 */
export function inUseText(e: Refusal): string {
  const counts = (e.counts ?? {}) as UseCounts;
  return `${messageFor(e)}：租户 ${counts.tenants ?? 0}、密钥 ${counts.keys ?? 0}、启用的账号 ${counts.channels ?? 0}`;
}

/**
 * A refused 删除: the counts it carries, and the two places to clear. A
 * serving account is never why a delete is refused, so it is not named.
 */
export function deleteRefusalText(e: Refusal): string {
  const counts = (e.counts ?? {}) as UseCounts;
  return `${messageFor(e)}：${counts.tenants ?? 0} 个租户、${counts.keys ?? 0} 个密钥仍指向它。请在租户的模型授权、以及密钥的模型选择里移除它，再删除。`;
}
