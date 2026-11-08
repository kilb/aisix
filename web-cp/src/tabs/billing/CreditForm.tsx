import { useRef, useState } from "react";
import { billing } from "../../lib/api";
import { sendCredit } from "../../lib/creditOutcome";
import { isUsdAmount } from "../../lib/money";
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
  // 请求可能已落库而应答丢了：表单保持锁定，直到操作员确认过并明确放弃。
  const [unknown, setUnknown] = useState(false);
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
    if (inFlight.current || Object.keys(check(amount, reason)).length > 0) return;
    inFlight.current = true;
    setBusy(true);
    const outcome = await sendCredit(() =>
      billing.credit(account.tenant_id, {
        amount_usd: amount.trim(),
        reason: reason.trim(),
        ...(reference.trim() !== "" ? { reference: reference.trim() } : {}),
      }),
    );
    if (outcome.kind === "unknown") {
      setUnknown(true);
      inFlight.current = false;
      setBusy(false);
      return;
    }
    if (outcome.kind === "refused") {
      setRefused(refusal(outcome.error, (p) => p === "/amount_usd" || p === "/reason" || p === "/reference"));
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

  /** 重新读取余额：入账若已落库，余额变化会让流水区重新加载。 */
  async function recheck() {
    setChecking(true);
    try {
      onChanged(await billing.account(account.tenant_id));
    } catch {
      // 读取失败时保持原样，操作员可以再点一次。
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
    setUnknown(false);
  }

  const amountProblem = local.amount ?? refused?.at.get("/amount_usd") ?? null;
  const reasonProblem = local.reason ?? refused?.at.get("/reason") ?? null;
  return (
    <Region label="直接入账">
      <p className="hint">直接给这个租户入账，用于线下收款或补偿。入账后租户在自己的充值记录里看到这笔入账和原因。</p>
      <form ref={form} onSubmit={(e) => e.preventDefault()} noValidate>
        <Field label="金额（美元）" error={amountProblem}>
          {(aria) => (
            <input inputMode="decimal" value={amount} disabled={busy || unknown} onChange={(e) => setAmount(e.target.value)} {...aria} />
          )}
        </Field>
        <Field label="原因" error={reasonProblem}>
          {(aria) => <input value={reason} maxLength={500} disabled={busy || unknown} onChange={(e) => setReason(e.target.value)} {...aria} />}
        </Field>
        <Field label="凭证号" error={refused?.at.get("/reference") ?? null}>
          {(aria) => <input value={reference} maxLength={128} disabled={busy || unknown} onChange={(e) => setReference(e.target.value)} {...aria} />}
        </Field>
        {!unknown && (
        <TwoStep
          label="入账"
          kind="primary"
          busy={busy}
          ask={ask}
          warning="将立即计入该租户余额"
          onConfirm={() => void send()}
        />
        )}
      </form>
      {unknown && (
        <div className="note crit" role="alert">
          入账结果未知：请先在该租户的充值记录中确认是否已入账，再决定是否重试。本面板的「余额流水」里类型为「充值」的条目就是入账记录。
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
