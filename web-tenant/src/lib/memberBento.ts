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
