import { exactInteger } from "./chartFormat.ts";

interface CurrentMember {
  id: string;
  email: string;
  display_name: string | null;
}

interface MemberCost {
  member_id: string;
  charged_micro_cny: string;
}

interface RankedMemberCost {
  id: string;
  label: string;
  amount: bigint;
}

/** 只对当前成员的完整精确费用排序；缺失不等于零，历史成员不参与。 */
export function rankCurrentMemberCosts(
  people: readonly CurrentMember[],
  usage: readonly MemberCost[],
): RankedMemberCost[] | null {
  const byMember = new Map(usage.map((row) => [row.member_id, row.charged_micro_cny]));
  const rows: RankedMemberCost[] = [];
  for (const person of people) {
    const amount = exactInteger(byMember.get(person.id));
    if (amount === null) return null;
    rows.push({ id: person.id, label: person.display_name ?? person.email, amount });
  }
  return rows.sort((left, right) => left.amount > right.amount ? -1 : left.amount < right.amount ? 1 : left.label.localeCompare(right.label));
}

/** 活跃仅指统计窗口内有已记录调用；当前账号状态不改变已发生的活动。 */
export function rankCurrentMemberActivity(
  people: readonly CurrentMember[],
  usage: readonly { member_id: string; requests: number }[],
): { id: string; label: string; requests: bigint }[] | null {
  const byMember = new Map(usage.map((row) => [row.member_id, row.requests]));
  const rows: { id: string; label: string; requests: bigint }[] = [];
  for (const person of people) {
    const requests = exactInteger(byMember.get(person.id));
    if (requests === null || requests < 0n) return null;
    if (requests > 0n) rows.push({ id: person.id, label: person.display_name ?? person.email, requests });
  }
  return rows.sort((left, right) => left.requests > right.requests ? -1 : left.requests < right.requests ? 1 : left.label.localeCompare(right.label));
}
