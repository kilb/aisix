/** 只统计未退役公共模型的已保存映射；禁用映射仍是映射，不等于正在服务。 */
export function providerMappingCounts(models: readonly {
  id: string;
  retired_at: string | null;
  served_by: readonly { provider_key_id: string; enabled: boolean }[];
}[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const model of models) {
    if (model.retired_at !== null) continue;
    for (const account of model.served_by) {
      counts.set(account.provider_key_id, (counts.get(account.provider_key_id) ?? 0) + 1);
    }
  }
  return counts;
}
