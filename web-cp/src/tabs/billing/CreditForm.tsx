import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { billing } from "../../lib/api";
import { creditLocks, sendCredit, type CreditRequest } from "../../lib/creditOutcome";
import { errorText } from "../../lib/fmt";
import { assessmentRequestId as newOperationId } from "../../lib/modelAssessment";
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
  // 回包丢失或标签页刷新后沿用原操作；同一笔未核实的入账不能再次创建。
  const lock = useSyncExternalStore(creditLocks.subscribe, () => creditLocks.get(account.tenant_id));
  const unknown = lock === "unknown";
  const saved = lock ? creditLocks.request(account.tenant_id) : null;
  // A credit this form did not send is still on its way.
  const elsewhere = lock === "pending" && !busy;
  // A credit refused after the form that sent it closed: nothing was credited.
  const earlier = useSyncExternalStore(creditLocks.subscribe, () => creditLocks.refusal(account.tenant_id));
  // Whether this form is still on screen to show its own refusal.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
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

  async function send(retry = false) {
    let body: CreditRequest | null;
    if (retry) {
      body = creditLocks.retry(account.tenant_id);
      if (!body) return;
    } else {
      if (Object.keys(check(amount, reason)).length > 0) return;
      body = {
        operation_id: newOperationId(),
        amount_cny: amount.trim(),
        reason: reason.trim(),
        ...(reference.trim() !== "" ? { reference: reference.trim() } : {}),
      };
      try {
        if (!creditLocks.acquire(account.tenant_id, body)) return;
      } catch (error) {
        if (!(error instanceof DOMException)) throw error;
        setStale("浏览器无法保存待确认的入账，本次尚未提交。请检查标签页的存储权限后重试。");
        return;
      }
    }
    setBusy(true);
    setStale(null);
    const outcome = await sendCredit(() => billing.credit(account.tenant_id, body));
    if (outcome.kind === "unknown" || (retry && outcome.kind === "refused")) {
      // 本次重试被拒绝，也不能据此判定上一次的入账结果。
      creditLocks.settle(account.tenant_id, "unknown");
      if (outcome.kind === "refused") setStale(`重试未完成：${errorText(outcome.error)}`);
      setBusy(false);
      return;
    }
    if (outcome.kind === "refused") {
      const refused = refusal(outcome.error, (p) => p === "/amount_cny" || p === "/reason" || p === "/reference");
      // Closed since, the form cannot show it: the tenant's next form says it instead.
      const why = [...refused.at.values(), refused.general].filter(Boolean).join("；") || errorText(outcome.error);
      creditLocks.settle(account.tenant_id, "refused", mounted.current ? undefined : `上一笔入账被拒绝，未入账：${why}`);
      setRefused(refused);
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
    setStale(null);
    creditLocks.release(account.tenant_id);
  }

  const amountProblem = local.amount ?? refused?.at.get("/amount_cny") ?? null;
  const reasonProblem = local.reason ?? refused?.at.get("/reason") ?? null;
  return (
    <Region label="直接入账">
      <p className="hint text-caption">直接给这个租户入账，用于线下收款或补偿。入账后租户在自己的充值记录里看到这笔入账和原因。</p>
      <form ref={form} onSubmit={(e) => e.preventDefault()} noValidate>
        <div className="grid gap-x-4 sm:grid-cols-2">
          <Field label="金额（元）" error={amountProblem}>
            {(aria) => (
              <input inputMode="decimal" value={saved?.amount_cny ?? amount} disabled={busy || lock !== null} onChange={(e) => setAmount(e.target.value)} {...aria} />
            )}
          </Field>
          <Field label="凭证号" error={refused?.at.get("/reference") ?? null}>
            {(aria) => <input value={saved ? (saved.reference ?? "") : reference} maxLength={128} disabled={busy || lock !== null} onChange={(e) => setReference(e.target.value)} {...aria} />}
          </Field>
        </div>
        <Field label="原因" error={reasonProblem}>
          {(aria) => <input value={saved?.reason ?? reason} maxLength={500} disabled={busy || lock !== null} onChange={(e) => setReason(e.target.value)} {...aria} />}
        </Field>
        {!unknown && (
          <div className="right flex justify-end">
            <TwoStep
              label="入账"
              kind="primary"
              disabled={elsewhere}
              busy={busy}
              ask={ask}
              warning="将立即计入该租户余额"
              onConfirm={() => void send()}
            />
          </div>
        )}
      </form>
      {elsewhere && (
        <div className="note warn" role="status">
          这个租户的上一笔入账仍在等待结果，结果出来之前不能再次入账。
        </div>
      )}
      {earlier && lock === null && (
        <div className="note crit" role="alert">
          {earlier}
        </div>
      )}
      {unknown && (
        <div className="note crit" role="alert">
          入账结果未知。{saved && "重试会使用原操作编号，已入账的款项只计一次。"}也可以在「计费 · 充值审批」中按「平台入账」、租户、原因与凭证号核对记录。
          <div className="bl-row-actions mt-3">
            {saved && <Button type="button" size="sm" onClick={() => void send(true)}>重试这笔入账</Button>}
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
