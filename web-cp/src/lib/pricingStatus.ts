import type { PricingStatus } from "./api";

/**
 * The pricing code behind an unpriced charge, as the operator reads it. The
 * engine's codes, the control plane's own, and `unknown` (a charge stored with
 * no code). A code not listed is shown as it came.
 */
export const REASONS: Record<string, string> = {
  missing_price: "未定价",
  invalid_price: "价格不可用",
  missing_tier_price: "缺少档位价",
  missing_dimension_price: "缺少计量价",
  inconsistent_usage: "用量不一致",
  unpriced_operation: "不支持计价的操作",
  undecodable_usage: "无法读取",
  unknown_api_key: "未知密钥",
  tenant_mismatch: "租户不符",
  overflow: "溢出",
  missing_event_id: "缺少事件编号",
  unknown_tenant: "未知租户",
  unknown_model: "未知模型",
  unknown: "未记录原因",
};

export const reasonLabel = (code: string): string => REASONS[code] ?? code;

const TABLES: Record<string, string> = { sell: "售价", cost: "成本", discount: "折扣" };

/** What 计价状态 and the Overview card show, derived once from the response. */
export function statusView(s: PricingStatus) {
  return {
    invalid: s.invalid_revisions.map((r) => ({
      key: `${r.table}/${r.revision_id}`,
      table: TABLES[r.table] ?? r.table,
      revision: String(r.revision_id),
      reason: r.reason,
    })),
    unpricedTotal: s.unpriced_last_24h.total,
    unpriced: Object.entries(s.unpriced_last_24h.by_code).map(([code, count]) => ({
      code,
      label: reasonLabel(code),
      count,
    })),
  };
}
