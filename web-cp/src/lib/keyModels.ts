/**
 * The models a key may call, as the Keys table shows them.
 *
 * Three shapes, because a key's grant is stored in one of three:
 *
 * - `allowed_models` (name patterns; the platform's own tenant only) wins
 *   outright and is shown as written.
 * - `model_ids === null` is every model the tenant grants, including ones
 *   granted later, so it names no list.
 * - otherwise the selected ids are shown by name.
 *
 * A selected id missing from the tenant's stored `allowed_model_ids` is
 * marked, because that label means "the tenant no longer grants this" — the
 * case an operator needs to see and fix.
 *
 * It must NOT be shown for an `all`-scope tenant: that scope means every
 * public model, including ones granted after the key was minted, so its
 * stored `allowed_model_ids` is empty by construction and every selected
 * model would be mislabelled. The scope decides, never the list alone.
 */
export function keyModels(
  key: { allowed_models?: string[] | null; model_ids?: string[] | null },
  tenant: { model_scope: "all" | "selected"; allowed_model_ids: string[] },
  names: Map<string, string>,
): string {
  if (key.allowed_models?.length) return key.allowed_models.join("、");
  if (key.model_ids === null || key.model_ids === undefined) return "全部授权模型";
  if (key.model_ids.length === 0) return "无";
  return key.model_ids
    .map((id) => {
      const granted = tenant.model_scope === "all" || tenant.allowed_model_ids.includes(id);
      return `${names.get(id) ?? id}${granted ? "" : "（租户未授权）"}`;
    })
    .join("、");
}
