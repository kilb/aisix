import { useRef, useState } from "react";
import { billing } from "../../lib/api";
import { isUsdAmount } from "../../lib/money";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { refusal } from "../Tenants";
import { MONEY_HINT, Region, type RegionProps } from "./AccountPanel";

/** What the form refuses before anything is sent, by field. */
interface Problems {
  amount?: string;
  reason?: string;
}

function check(amount: string, reason: string): Problems {
  const problems: Problems = {};
  if (!isUsdAmount(amount.trim(), { allowZero: false })) problems.amount = MONEY_HINT;
  if (reason.trim() === "") problems.reason = "请填写入账原因";
  return problems;
}

function Form({ account, onChanged }: RegionProps) {
  const toast = useToast();
  const [amount, setAmount] = useState("");
  const [reason, setReason] = useState("");
  const [reference, setReference] = useState("");
  const [busy, setBusy] = useState(false);
  // 同步的重入保护：状态更新前的第二次确认也进不来
  const inFlight = useRef(false);
  const [local, setLocal] = useState<Problems>({});
  const [refused, setRefused] = useState<ReturnType<typeof refusal> | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  /** The first click: refused fields are shown and the two-step does not open. */
  function ask(): boolean {
    const problems = check(amount, reason);
    setLocal(problems);
    setRefused(null);
    setStale(null);
    if (problems.amount || problems.reason) {
      focusRefusal();
      return false;
    }
    return true;
  }

  async function send() {
    if (inFlight.current || Object.keys(check(amount, reason)).length > 0) return;
    inFlight.current = true;
    setBusy(true);
    try {
      await billing.credit(account.tenant_id, {
        amount_usd: amount.trim(),
        reason,
        ...(reference.trim() !== "" ? { reference: reference.trim() } : {}),
      });
    } catch (err) {
      setRefused(refusal(err, (p) => p === "/amount_usd" || p === "/reason" || p === "/reference"));
      inFlight.current = false;
      setBusy(false);
      focusRefusal();
      return;
    }
    // Credited: the form starts over, and the balance shown is the backend's own, read again.
    setAmount("");
    setReason("");
    setReference("");
    toast("已入账");
    try {
      onChanged(await billing.account(account.tenant_id));
    } catch {
      setStale("已入账，但读取最新余额失败，请刷新账户列表。");
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  }

  const amountProblem = local.amount ?? refused?.at.get("/amount_usd") ?? null;
  const reasonProblem = local.reason ?? refused?.at.get("/reason") ?? null;
  return (
    <Region label="直接入账">
      <p className="hint">直接给这个租户入账，用于线下收款或补偿。入账后租户在自己的充值记录里看到这笔入账和原因。</p>
      <form ref={form} onSubmit={(e) => e.preventDefault()} noValidate>
        <Field label="金额（美元）" error={amountProblem}>
          {(aria) => (
            <input inputMode="decimal" value={amount} disabled={busy} onChange={(e) => setAmount(e.target.value)} {...aria} />
          )}
        </Field>
        <Field label="原因" error={reasonProblem}>
          {(aria) => <input value={reason} maxLength={500} disabled={busy} onChange={(e) => setReason(e.target.value)} {...aria} />}
        </Field>
        <Field label="凭证号" error={refused?.at.get("/reference") ?? null}>
          {(aria) => <input value={reference} maxLength={128} disabled={busy} onChange={(e) => setReference(e.target.value)} {...aria} />}
        </Field>
        <TwoStep
          label="入账"
          kind="primary"
          busy={busy}
          ask={ask}
          warning="将立即计入该租户余额"
          onConfirm={() => void send()}
        />
      </form>
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
      {stale && (
        <div className="note warn" role="status">
          {stale}
        </div>
      )}
    </Region>
  );
}

/** 直接入账: admin operators only. One form per tenant opened, so a draft never carries to another tenant. */
export function CreditForm(props: RegionProps) {
  if (!props.isAdmin) return null;
  return <Form key={props.account.tenant_id} {...props} />;
}
