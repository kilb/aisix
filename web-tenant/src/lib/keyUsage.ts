import { exactInteger } from "./chartFormat.ts";
import { todayBeijing } from "./time.ts";

export interface KeyCost {
  chargedMicroCny: bigint | null;
  unpricedRequests: number | null;
  pendingRequests: number | null;
}
export interface KeyCosts {
  from: string;
  to: string;
  items: Map<string, KeyCost>;
}

/** 与成员费用相同：北京时间月初至当前时刻，左闭右开。 */
export function keyMonthRange(now = new Date()): { from: string; to: string } {
  return { from: new Date(`${todayBeijing(now).slice(0, 7)}-01T00:00:00+08:00`).toISOString(), to: now.toISOString() };
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function count(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

/** 金额只接受接口的整数字符串；缺失行和缺失字段不补零。 */
export function decodeKeyCosts(input: unknown): KeyCosts {
  const body = record(input);
  if (!body || typeof body.from !== "string" || !Number.isFinite(Date.parse(body.from)) || typeof body.to !== "string" || !Number.isFinite(Date.parse(body.to)) || !Array.isArray(body.items)) {
    throw new Error("密钥费用响应不完整，请重试。");
  }
  const items = new Map<string, KeyCost>();
  for (const input of body.items) {
    const row = record(input);
    if (!row || typeof row.api_key_id !== "string" || !row.api_key_id || items.has(row.api_key_id)) throw new Error("密钥费用响应的密钥信息无效，请重试。");
    items.set(row.api_key_id, {
      chargedMicroCny: typeof row.charged_micro_cny === "string" ? exactInteger(row.charged_micro_cny) : null,
      unpricedRequests: count(row.unpriced_requests), pendingRequests: count(row.pending_requests),
    });
  }
  return { from: body.from, to: body.to, items };
}

export function keyCostNotes(cost: KeyCost): string[] {
  const complete = cost.unpricedRequests !== null && cost.pendingRequests !== null && cost.pendingRequests >= cost.unpricedRequests;
  return [
    ...(!complete ? ["计价状态未知"] : []),
    ...(cost.unpricedRequests !== null && cost.unpricedRequests > 0 ? [`待计价 ${cost.unpricedRequests} 次`] : []),
    ...(complete && cost.pendingRequests! > cost.unpricedRequests! ? [`另有 ${cost.pendingRequests! - cost.unpricedRequests!} 次处理中`] : []),
  ];
}

export async function fetchKeyCosts(range: { from: string; to: string }): Promise<KeyCosts> {
  const { call } = await import("./api");
  return decodeKeyCosts((await call<unknown>("GET", `/usage/keys?${new URLSearchParams(range)}`)).data);
}
