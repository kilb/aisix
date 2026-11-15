import { useRef, useState } from "react";
import { ApiError, budgets, type Budget } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { isCnyAmount, microCnyToInput } from "./lib/money";
import { Button } from "./ui/Button";
import { Field, useRefusalFocus } from "./ui/Field";
import { FormDialog } from "./ui/FormDialog";
import { Select } from "./ui/Select";

/** 成员页沿用额度 API；已有额度只改限额，不暗改类型或周期。 */
export function MemberBudgetDialog({ memberId, budget, onClose, onSaved, onInactive }: {
  memberId: string;
  budget: Budget | null;
  onClose: () => void;
  onSaved: () => void;
  onInactive: () => void;
}) {
  const [kind, setKind] = useState<Budget["kind"]>(budget?.kind ?? "periodic");
  const [period, setPeriod] = useState<NonNullable<Budget["period"]>>(budget?.period ?? "month");
  const [amount, setAmount] = useState(budget ? microCnyToInput(budget.limit_micro_cny) : "");
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
      else await budgets.create({ scope: "member", scope_ref: memberId, kind, ...(kind === "periodic" ? { period } : {}), limit_cny: value });
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

  return <FormDialog open onOpenChange={(open) => { if (!open) onClose(); }} busy={busy} title={budget ? "调整成员额度" : "设置成员额度"} description="成员额度与组织、密钥额度独立生效，以先用尽者为准。">
    <form ref={form} className="grid gap-5" aria-label={budget ? "调整成员额度" : "设置成员额度"} onSubmit={submit}>
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
