import type { Balance } from "./lib/api";
import type { FormulaBalance } from "./lib/formulaApi";
import { Money } from "./ui/BalanceReadout";
import { Metric } from "./components/page";

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
    <dl className="grid gap-6">
      <div className="min-w-0">
        <dt className="text-meta font-mono text-muted">账户可用额度</dt>
        <dd className="mt-3"><Metric size="hero" className="break-all">{available === null ? "未知" : <Money micro={available} />}</Metric></dd>
      </div>
      <div className="min-w-0">
        <dt className="text-meta font-mono text-muted">信用额度</dt>
        <dd className="mt-2"><Metric size="value" className="break-all">{credit === null ? "未知" : <Money micro={credit} />}</Metric></dd>
      </div>
    </dl>
  );
}
