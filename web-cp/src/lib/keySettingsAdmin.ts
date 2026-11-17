import type { AdminKeyChanges } from "./api";

interface Source {
  label: string;
  model_ids: string[] | null;
  allowed_models: string[] | null;
  group: { id: string } | null;
  rate_limit: { concurrency?: number; rpm?: number };
  disabled: boolean;
  expires_at: string | null;
}

interface Draft {
  label: string;
  scope: "inherited" | "selected" | "patterns";
  modelIds: string[];
  patterns: string[];
  groupId: string;
  rateLimit: { concurrency?: number; rpm?: number };
  disabled: boolean;
  expiresAt: string | null;
}

function sameList(left: string[] | null, right: string[] | null): boolean {
  return left === right || (left !== null && right !== null && left.length === right.length && left.every((value, index) => value === right[index]));
}

/** 仅发送实际改动；切换回ID授权时显式清除优先级更高的通配覆盖。 */
export function adminKeySettingsPatch(initial: Source, draft: Draft): AdminKeyChanges {
  const patch: AdminKeyChanges = {};
  if (draft.label !== initial.label) patch.label = draft.label;
  if (draft.scope !== "patterns") {
    const ids = draft.scope === "selected" ? draft.modelIds : null;
    const originalIds = initial.model_ids === null ? null : new Set(initial.model_ids);
    const selectedIds = ids === null ? null : new Set(ids);
    const sameIds = originalIds === selectedIds || (originalIds !== null && selectedIds !== null && originalIds.size === selectedIds.size && [...originalIds].every(id => selectedIds.has(id)));
    if (!sameIds) patch.model_ids = ids;
    if (initial.allowed_models !== null) patch.allowed_models = null;
  } else if (!sameList(initial.allowed_models, draft.patterns)) patch.allowed_models = draft.patterns;
  if (!initial.group && draft.groupId) patch.group_id = draft.groupId;
  if (draft.rateLimit.concurrency !== initial.rate_limit.concurrency || draft.rateLimit.rpm !== initial.rate_limit.rpm) {
    patch.rate_limit = draft.rateLimit.concurrency === undefined && draft.rateLimit.rpm === undefined ? null : draft.rateLimit;
  }
  if (draft.disabled !== initial.disabled) patch.disabled = draft.disabled;
  if (draft.expiresAt !== initial.expires_at) patch.expires_at = draft.expiresAt;
  return patch;
}
