import type { ChargeFilter, PricingStatus, UnpricedDetail } from "./api";

/**
 * The pricing code behind an unpriced charge, as the operator reads it. The
 * engine's codes, the control plane's own, and `unknown` (a charge stored with
 * no code). A code not listed is shown as it came.
 */
export const REASONS: Record<string, string> = {
  missing_price: "未定价",
  invalid_price: "价格不可用",
  // A key whose group sets no rate for the model: the operator sets one in 价格 · 分组.
  missing_rate: "分组未设换算率",
  // A key an older control plane minted with no group: it is priced once its key takes one.
  no_group: "密钥未选分组",
  missing_tier_price: "缺少档位价",
  missing_dimension_price: "缺少计量价",
  inconsistent_usage: "用量不一致",
  unpriced_operation: "不支持计价的操作",
  undecodable_usage: "无法读取",
  unknown_api_key: "未知密钥",
  tenant_mismatch: "租户不符",
  overflow: "溢出",
  discount_above_whole: "折扣超出范围",
  missing_event_id: "缺少事件编号",
  unknown_tenant: "未知租户",
  unknown_model: "未知模型",
  unknown: "未记录原因",
};

export const reasonLabel = (code: string): string => REASONS[code] ?? code;

/** The tier as the detail names it; an upstream that reported none is said so. */
const tierName = (tier: string | null): string => (tier ? tier : "（未报告）");

/** The group as the detail names it: the name tenants see, and the routing name behind it. */
const groupName = (g: UnpricedDetail["group"]): string =>
  g ? (g.display_name && g.display_name !== g.name ? `${g.display_name}（${g.name}）` : g.name || g.id) : "该分组";

/** What the operator does about one kind of unpriced call. */
export function remedy(d: Pick<UnpricedDetail, "code" | "service_tier" | "group">): string {
  switch (d.code) {
    case "missing_tier_price":
      return `该模型的基准价没有 ${tierName(d.service_tier)} 档；补上该档或确认后用重计价更正`;
    case "missing_price":
      return "该模型在调用时没有生效的基准价；在「基准价」补录后用重计价更正";
    case "missing_rate":
      return `分组 ${groupName(d.group)} 没有该模型的换算率；在「价格 · 分组」设置后用重计价更正`;
    case "no_group":
      return "密钥没有选分组；为密钥选定分组后用重计价更正";
    case "invalid_price":
      return "生效的价格版本无法使用（见上方「不可用的价格版本」）；写入新版本后用重计价更正";
    case "missing_dimension_price":
      return "基准价缺少这次用量所需的计量项价格；补上该项或确认后用重计价更正";
    case "discount_above_whole":
      return "租户折扣超过 100%；改正折扣后用重计价更正";
    case "inconsistent_usage":
      return "上游报告的用量前后矛盾，无法计价；请核对该上游账号";
    case "unpriced_operation":
      return "这类操作没有可计价的用量；需要计费时补上对应计量项的价格";
    case "overflow":
      return "金额超出可记录的范围；请检查该模型的价格";
    case "undecodable_usage":
    case "unknown_api_key":
    case "tenant_mismatch":
    case "unknown_tenant":
      return "调用无法归属到租户，不会计费，重计价也不会为它计费";
    case "unknown_model":
      return "调用没有解析到模型，不会计费";
    case "missing_event_id":
      return "用量记录缺少事件编号，无法计价";
    default:
      return "未记录原因";
  }
}

/** The 计费明细 filter that lists one kind's calls. */
export function chargesFilterFor(d: Pick<UnpricedDetail, "code" | "model">): ChargeFilter {
  return { status: "unpriced", error_code: d.code, ...(d.model.id ? { model_id: d.model.id } : {}) };
}

/** A model as a status or charge row shows it: its name, marked when it is deleted; "—" for none. */
export function modelText(name: string | null, deleted: boolean): string {
  if (!name) return deleted ? "（已删除的模型）" : "—";
  return deleted ? `${name}（已删除）` : name;
}

const TABLES: Record<string, string> = { baseline: "基准价", rate: "分组换算率", cost: "成本换算率", discount: "折扣" };

/**
 * Per reason, the calls a detail list cut short leaves out: the reason's
 * count less the counts listed under it, where that is more than 0, in
 * `byCode`'s order. Whichever kinds the cut keeps, every call is then in a
 * row. A list that is not cut short lists every kind, so it has none (the
 * counts and the list are read one after the other, and a difference there
 * is a call that aged out or was corrected in between, not one unlisted).
 */
export function unlistedByCode(
  byCode: Record<string, number>,
  items: Pick<UnpricedDetail, "code" | "count">[],
  truncated: boolean,
): { code: string; count: number }[] {
  if (!truncated) return [];
  const listed = new Map<string, number>();
  for (const d of items) listed.set(d.code, (listed.get(d.code) ?? 0) + d.count);
  return Object.entries(byCode)
    .map(([code, count]) => ({ code, count: count - (listed.get(code) ?? 0) }))
    .filter((r) => r.count > 0);
}

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
    details: (s.unpriced_details?.items ?? []).map((d, i) => ({
      key: `${d.code}/${d.model.id ?? ""}/${d.service_tier ?? ""}/${d.group?.id ?? ""}/${i}`,
      code: d.code,
      label: reasonLabel(d.code),
      count: d.count,
      model: modelText(d.model.name, d.model.deleted),
      scope:
        d.code === "missing_tier_price"
          ? `档位 ${tierName(d.service_tier)}`
          : d.code === "missing_rate"
            ? `分组 ${groupName(d.group)}`
            : "—",
      latestAt: d.latest_at,
      remedy: remedy(d),
      filter: chargesFilterFor(d),
    })),
    unlisted: unlistedByCode(s.unpriced_last_24h.by_code, s.unpriced_details?.items ?? [], s.unpriced_details?.truncated ?? false).map(
      (r) => ({
        key: `${r.code}/unlisted`,
        code: r.code,
        label: reasonLabel(r.code),
        count: r.count,
        filter: { status: "unpriced", error_code: r.code },
      }),
    ),
    detailsTruncated: s.unpriced_details?.truncated ?? false,
    correctedTotal: s.corrected_last_24h?.total ?? 0,
  };
}

/**
 * The filter a link from 计价状态 hands 计费明细, which reads it once when it
 * opens. The sections share no state, so the link leaves it here.
 */
let pendingChargesFilter: ChargeFilter | null = null;

export function handChargesFilter(f: ChargeFilter): void {
  pendingChargesFilter = f;
}

export function takeChargesFilter(): ChargeFilter | null {
  const f = pendingChargesFilter;
  pendingChargesFilter = null;
  return f;
}
