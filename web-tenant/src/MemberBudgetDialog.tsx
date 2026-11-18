import { useRef, useState } from "react";
import { ApiError, budgets, type Budget } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { keyBudgetAmountInput } from "./lib/keyBudget";
import { isCnyAmount, microCnyToInput } from "./lib/money";
import { Button } from "./ui/Button";
import { Field, useRefusalFocus } from "./ui/Field";
import { FormDialog } from "./ui/FormDialog";
import { Select } from "./ui/Select";

type BudgetDraft = Pick<Budget, "id" | "kind" | "period" | "limit_micro_cny">;
interface DialogProps {
  budget: BudgetDraft | null;
  onClose: () => void;
  onSaved: () => void;
  onInactive: () => void;
}

export function MemberBudgetDialog({ memberId, ...props }: DialogProps & { memberId: string }) {
  return <BudgetDialog {...props} targetId={memberId} scope="member" />;
}

/** 已有额度只调整金额；新增沿用目标、类型及周期的服务端约束。 */
export function BudgetDialog({ scope, targetId, budget, onClose, onSaved, onInactive }: DialogProps & {
  scope: "member" | "api_key"; targetId: string;
}) {
  const label = scope === "member" ? "成员额度" : "密钥额度";
  const title = (budget ? "调整" : "设置") + label;
  const [kind, setKind] = useState<Budget["kind"]>(budget?.kind ?? "periodic");
  const [period, setPeriod] = useState<NonNullable<Budget["period"]>>(budget?.period ?? "month");
  const [amount, setAmount] = useState(budget ? (scope === "api_key" ? keyBudgetAmountInput(budget.limit_micro_cny) : microCnyToInput(budget.limit_micro_cny)) : "");
  const [amountError, setAmountError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (pending.current) return;
    setError(null);
    setAmountError(null);
    const value = amount.trim();
    if (!isCnyAmount(value, { allowZero: false })) {
      setAmountError("请输入大于 0 的金额，最多两位小数");
      focusRefusal();
      return;
    }
    pending.current = true;
    setBusy(true);
    try {
      if (budget) await budgets.setLimit(budget.id, value);
      else await budgets.create({ scope, scope_ref: targetId, kind, ...(kind === "periodic" ? { period } : {}), limit_cny: value });
      onSaved();
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onInactive();
      if (e instanceof ApiError && e.code === "validation_failed" && e.details.some((detail) => detail.path === "/limit_cny")) {
        setAmountError(e.details.filter((detail) => detail.path === "/limit_cny").map((detail) => detail.message).join("；"));
        focusRefusal();
      } else setError(e instanceof ApiError ? formMessageFor(e) : String(e));
    } finally {
      pending.current = false;
      setBusy(false);
    }
  }

  return <FormDialog open onOpenChange={(open) => { if (!open) onClose(); }} busy={busy} title={title} description="组织、成员与密钥额度分别生效，以先用尽者为准。">
    <form ref={form} className="grid gap-4" aria-label={title} onSubmit={submit}>
      {!budget && <>
        <Field label="额度类型"><Select value={kind} onValueChange={(value) => setKind(value as Budget["kind"])} disabled={busy} options={[{ value: "periodic", label: "按周期" }, { value: "cumulative", label: "累计" }]} /></Field>
        {kind === "periodic" && <Field label="额度周期"><Select value={period} onValueChange={(value) => setPeriod(value as NonNullable<Budget["period"]>)} disabled={busy} options={[{ value: "day", label: "每天" }, { value: "week", label: "每周" }, { value: "month", label: "每月" }]} /></Field>}
      </>}
      <Field label="额度（元）" error={amountError}>{(aria) => <input value={amount} onChange={(event) => setAmount(event.target.value)} inputMode="decimal" autoComplete="off" disabled={busy} required {...aria} />}</Field>
      {error && <div className="note crit" role="alert">{error}</div>}
      <div className="flex flex-wrap justify-end gap-2"><Button disabled={busy} onClick={onClose}>取消</Button><Button kind="primary" type="submit" busy={busy} busyLabel="正在保存额度…">保存额度</Button></div>
    </form>
  </FormDialog>;
}
