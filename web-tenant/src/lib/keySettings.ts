import type { KeyLimits, TenantKey, UpdateKey } from "./api.ts";

export interface KeySettings {
  label: string;
  modelIds: string[] | null;
  groupId: string;
  rateLimit: KeyLimits;
  disabled: boolean;
  expiresAt: string | null;
}

type CurrentSettings = Pick<TenantKey, "label" | "model_ids" | "group" | "rate_limit" | "disabled" | "expires_at">;

/** 只发送实际变化；避免重发旧授权、不可变分组或将空覆盖误解为保持原值。 */
export function keySettingsPatch(current: CurrentSettings, next: KeySettings): UpdateKey {
  const patch: UpdateKey = {};
  if (current.label !== next.label) patch.label = next.label;
  const sameModels = current.model_ids === null || next.modelIds === null
    ? current.model_ids === next.modelIds
    : current.model_ids.length === next.modelIds.length && current.model_ids.every(id => next.modelIds?.includes(id));
  if (!sameModels) patch.model_ids = next.modelIds;
  if (current.group === null && next.groupId) patch.group_id = next.groupId;
  if (current.rate_limit.concurrency !== next.rateLimit.concurrency || current.rate_limit.rpm !== next.rateLimit.rpm) {
    patch.rate_limit = next.rateLimit.concurrency === undefined && next.rateLimit.rpm === undefined ? null : next.rateLimit;
  }
  if (current.disabled !== next.disabled) patch.disabled = next.disabled;
  const sameExpiry = current.expires_at === null || next.expiresAt === null
    ? current.expires_at === next.expiresAt
    // 表单只显示到秒，修改别的字段不能顺带截掉已保存的毫秒。
    : Math.floor(Date.parse(current.expires_at) / 1000) === Math.floor(Date.parse(next.expiresAt) / 1000);
  if (!sameExpiry) patch.expires_at = next.expiresAt;
  return patch;
}
