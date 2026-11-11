import type { Balance } from "./lib/api";
import type { FormulaBalance } from "./lib/formulaApi";
import { Money } from "./ui/BalanceReadout";

type AmountField = "available_micro_cny" | "credit_limit_micro_cny";
export type BudgetAccountBalance = Balance & {
  calculation?: Partial<Pick<FormulaBalance["calculation"], AmountField>>;
};

/** 同一响应优先使用精确金额；旧控制面只有安全整数才能可靠回退。 */
function amount(balance: BudgetAccountBalance, field: AmountField): string | null {
  if (balance.calculation !== undefined) {
    const exact = balance.calculation?.[field];
    return typeof exact === "string" && /^-?\d+$/.test(exact) ? exact : null;
  }
  return Number.isSafeInteger(balance[field]) ? String(balance[field]) : null;
}

/** 预算页的账户金额，与额外预算上限独立展示。 */
export function BudgetAccountAmounts({ balance }: { balance: BudgetAccountBalance }) {
  const available = amount(balance, "available_micro_cny");
  const credit = amount(balance, "credit_limit_micro_cny");
  return (
    <dl className="grid gap-3 sm:grid-cols-2">
      <div className="entry rounded-xl border border-border bg-muted/50 px-4 py-3">
        <dt className="lab">账户可用额度</dt>
        <dd className="val mt-1 text-2xl">
          {available === null ? "未知" : <Money micro={available} />}
        </dd>
      </div>
      <div className="entry rounded-xl border border-border bg-muted/50 px-4 py-3">
        <dt className="lab">信用额度</dt>
        <dd className="val mt-1 text-2xl">
          {credit === null ? "未知" : <Money micro={credit} />}
        </dd>
      </div>
    </dl>
  );
}
