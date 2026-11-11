import { call } from "./api";

/** pending 包含 unpriced；两个计数不能相加。 */
export interface MemberUsage {
  member_id: string;
  requests: number;
  charged_micro_cny: string;
  estimated_requests: number;
  quantities: Record<string, number>;
  pending_requests: number;
  unpriced_requests: number;
  /** 最新有效明细缺失的请求数；quantities 仅包含已知部分。 */
  unknown_quantity_requests: number;
}

export interface MemberUsageSummary {
  from: string;
  to: string;
  items: MemberUsage[];
}

export async function memberUsage(): Promise<MemberUsageSummary> {
  return (await call<MemberUsageSummary>("GET", "/members/usage")).data;
}
