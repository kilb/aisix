import { useState, useSyncExternalStore } from "react";
import { billing } from "../../lib/api";
import { creditLocks, sendCredit } from "../../lib/creditOutcome";
import { isCnyAmount } from "../../lib/money";
import { Button } from "../../ui/Button";
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
  if (!isCnyAmount(amount.trim(), { allowZero: false })) problems.amount = MONEY_HINT;
  if (reason.trim() === "") problems.reason = "请填写入账原因";
  return problems;
}

function Form({ account, onChanged }: RegionProps) {
  const toast = useToast();
  const [amount, setAmount] = useState("");
  const [reason, setReason] = useState("");
  const [reference, setReference] = useState("");
  const [busy, setBusy] = useState(false);
  const [local, setLocal] = useState<Problems>({});
  const [refused, setRefused] = useState<ReturnType<typeof refusal> | null>(null);
  const [stale, setStale] = useState<string | null>(null);
  // The tenant's credit lock, which outlives this form: a credit sent from a form since closed
  // is still followed here. An unknown outcome may have been stored while its answer was lost:
  // the form stays locked until the operator has checked and starts over.
  const lock = useSyncExternalStore(creditLocks.subscribe, () => creditLocks.get(account.tenant_id));
  const unknown = lock === "unknown";
  // A credit this form did not send is still on its way.
  const elsewhere = lock === "pending" && !busy;
  const [checking, setChecking] = useState(false);
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
    // The lock is taken synchronously before anything is sent: a second confirm, or another
    // form of the same tenant, cannot get in.
    if (Object.keys(check(amount, reason)).length > 0 || !creditLocks.acquire(account.tenant_id)) return;
    setBusy(true);
    const outcome = await sendCredit(() =>
      billing.credit(account.tenant_id, {
        amount_cny: amount.trim(),
        reason: reason.trim(),
        ...(reference.trim() !== "" ? { reference: reference.trim() } : {}),
      }),
    );
    if (outcome.kind === "unknown") {
      creditLocks.settle(account.tenant_id, "unknown");
      setBusy(false);
      return;
    }
    if (outcome.kind === "refused") {
      creditLocks.settle(account.tenant_id, "refused");
      setRefused(refusal(outcome.error, (p) => p === "/amount_cny" || p === "/reason" || p === "/reference"));
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
      creditLocks.settle(account.tenant_id, "done");
      setBusy(false);
    }
  }

  /** Reads the balance again: if the credit was stored, the balance change reloads the ledger region. */
  async function recheck() {
    setChecking(true);
    try {
      onChanged(await billing.account(account.tenant_id));
    } catch {
      // A failed read leaves everything as it was; the operator can click again.
    } finally {
      setChecking(false);
    }
  }

  function startOver() {
    setAmount("");
    setReason("");
    setReference("");
    setLocal({});
    setRefused(null);
    creditLocks.release(account.tenant_id);
  }

  const amountProblem = local.amount ?? refused?.at.get("/amount_cny") ?? null;
  const reasonProblem = local.reason ?? refused?.at.get("/reason") ?? null;
  return (
    <Region label="直接入账">
      <p className="hint">直接给这个租户入账，用于线下收款或补偿。入账后租户在自己的充值记录里看到这笔入账和原因。</p>
      <form ref={form} onSubmit={(e) => e.preventDefault()} noValidate>
        <Field label="金额（元）" error={amountProblem}>
          {(aria) => (
            <input inputMode="decimal" value={amount} disabled={busy || lock !== null} onChange={(e) => setAmount(e.target.value)} {...aria} />
          )}
        </Field>
        <Field label="原因" error={reasonProblem}>
          {(aria) => <input value={reason} maxLength={500} disabled={busy || lock !== null} onChange={(e) => setReason(e.target.value)} {...aria} />}
        </Field>
        <Field label="凭证号" error={refused?.at.get("/reference") ?? null}>
          {(aria) => <input value={reference} maxLength={128} disabled={busy || lock !== null} onChange={(e) => setReference(e.target.value)} {...aria} />}
        </Field>
        {!unknown && (
        <TwoStep
          label="入账"
          kind="primary"
          disabled={elsewhere}
          busy={busy}
          ask={ask}
          warning="将立即计入该租户余额"
          onConfirm={() => void send()}
        />
        )}
      </form>
      {elsewhere && (
        <div className="note warn" role="status">
          这个租户的上一笔入账仍在等待结果，结果出来之前不能再次入账。
        </div>
      )}
      {unknown && (
        <div className="note crit" role="alert">
          入账结果未知：请先在「计费 · 充值审批」中把状态选为「已批准」、来源选为「平台入账」，按租户、原因与凭证号确认这笔入账是否已记录，再决定是否重试。本面板「余额流水」里的「充值」条目不分租户申请的充值与平台入账，不能据此判断。
          <div className="bl-row-actions">
            <Button type="button" size="sm" busy={checking} busyLabel="刷新中…" onClick={() => void recheck()}>
              刷新余额与流水
            </Button>
            <Button type="button" size="sm" onClick={startOver}>
              我已确认，重新填写
            </Button>
          </div>
        </div>
      )}
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
