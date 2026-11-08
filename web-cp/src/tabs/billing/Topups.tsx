import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { ApiError, tenants, topups, type Tenant, type Topup } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtMicroUsd, fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { Lamp } from "../../ui/Lamp";
import { TwoStep } from "../../ui/TwoStep";

type StatusFilter = Topup["status"] | "";

const STATUS_LABEL: Record<Topup["status"], string> = {
  pending: "待审批",
  approved: "已批准",
  rejected: "已驳回",
};

const ORIGIN_LABEL: Record<Topup["origin"], string> = {
  tenant: "租户申请",
  operator: "平台入账",
};

/** The request whose answer is awaited: at most one at a time, so no row can be sent twice. */
interface Flight {
  id: string;
  kind: "approve" | "reject";
}

/**
 * "充值审批": tenants' top-up requests, newest first. An operator approves
 * one (the amount is posted to the tenant's balance at once) or rejects it
 * with a reason the tenant reads. The control plane decides every request
 * once, so each answer is shown as it comes back, including
 * `409 topup_state` when another operator got there first.
 */
export function Topups({ canEdit, refreshShell }: TabProps) {
  const [status, setStatus] = useState<StatusFilter>("pending");
  const [origin, setOrigin] = useState<Topup["origin"] | "">("");
  const [tenantId, setTenantId] = useState("");
  const [tenantList, setTenantList] = useState<Tenant[]>([]);
  const [items, setItems] = useState<Topup[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<{ ok: boolean; text: string } | null>(null);
  const [flight, setFlight] = useState<Flight | null>(null);
  // The row whose rejection reason is being written.
  const [rejecting, setRejecting] = useState<{ id: string; reason: string; problem: string | null } | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  // A newer read makes an older one's answer stale.
  const latest = useRef(0);

  useEffect(() => {
    tenants.list().then(setTenantList, () => undefined);
  }, []);

  const load = useCallback(
    async (before?: number) => {
      const mine = ++latest.current;
      try {
        const page = await topups.list({
          status: status || undefined,
          origin: origin || undefined,
          tenant_id: tenantId || undefined,
          before,
        });
        if (mine !== latest.current) return;
        setItems((prev) => (before === undefined ? page.items : [...(prev ?? []), ...page.items]));
        setNext(page.next_before);
        setLoadError(null);
      } catch (e) {
        if (mine === latest.current) setLoadError(errorText(e));
      }
    },
    [status, origin, tenantId],
  );

  useEffect(() => {
    setItems(null);
    setRejecting(null);
    void load();
  }, [load]);

  async function decide(t: Topup, kind: Flight["kind"], reason?: string) {
    setFlight({ id: t.id, kind });
    setOutcome(null);
    try {
      if (kind === "approve") await topups.approve(t.id);
      else await topups.reject(t.id, reason ?? "");
      const amount = fmtMicroUsd(t.amount_micro_usd);
      setOutcome({
        ok: true,
        text: kind === "approve" ? `已批准 ${t.tenant_name} 的 ${amount} 充值` : `已驳回 ${t.tenant_name} 的 ${amount} 充值`,
      });
      setRejecting(null);
      void load();
      refreshShell();
    } catch (e) {
      const refusedReason = e instanceof ApiError ? e.details.find((d) => d.path === "/reason") : undefined;
      if (kind === "reject" && refusedReason) {
        setRejecting((r) => (r ? { ...r, problem: refusedReason.message } : r));
        focusRefusal();
      } else {
        setOutcome({ ok: false, text: `${t.tenant_name}：${e instanceof ApiError ? messageFor(e) : errorText(e)}` });
      }
      // Another operator may have decided it: show what is stored now.
      if (e instanceof ApiError && e.code === "topup_state") {
        setRejecting(null);
        void load();
        refreshShell();
      }
    } finally {
      setFlight(null);
    }
  }

  function confirmReject(t: Topup) {
    if (!rejecting) return;
    const reason = rejecting.reason.trim();
    if (reason === "") {
      setRejecting({ ...rejecting, problem: "请填写驳回原因（1–500 个字符）" });
      focusRefusal();
      return;
    }
    void decide(t, "reject", reason);
  }

  const inFlight = flight !== null;

  return (
    <div className="panel">
      <h2>充值审批</h2>
      <p className="hint">租户提交的充值申请。批准后金额立即计入该租户余额；驳回须写明原因，租户可以看到。</p>
      <div className="r">
        <Field label="状态">
          <select value={status} onChange={(e) => setStatus(e.target.value as StatusFilter)}>
            <option value="pending">待审批</option>
            <option value="approved">已批准</option>
            <option value="rejected">已驳回</option>
            <option value="">全部</option>
          </select>
        </Field>
        <Field label="来源">
          <select value={origin} onChange={(e) => setOrigin(e.target.value as Topup["origin"] | "")}>
            <option value="">全部</option>
            <option value="tenant">{ORIGIN_LABEL.tenant}</option>
            <option value="operator">{ORIGIN_LABEL.operator}</option>
          </select>
        </Field>
        <Field label="租户">
          <select value={tenantId} onChange={(e) => setTenantId(e.target.value)}>
            <option value="">全部</option>
            {tenantList.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </select>
        </Field>
        <Button type="button" icon="refresh" onClick={() => void load()}>
          刷新
        </Button>
      </div>
      {outcome && (
        <div className={outcome.ok ? "note ok" : "note crit"} role={outcome.ok ? "status" : "alert"}>
          {outcome.text}
        </div>
      )}
      {loadError && (
        <div className="note crit" role="alert">
          {loadError}
        </div>
      )}
      {items === null && !loadError && <Loading />}
      {items !== null && (
        <DataTable label="充值申请">
          <thead>
            <tr>
              <th>租户</th>
              <th className="num">金额</th>
              <th>备注</th>
              <th>凭证号</th>
              <th>来源</th>
              <th>提交人</th>
              <th>提交时间（UTC）</th>
              <th className="right">操作</th>
            </tr>
          </thead>
          <tbody>
            {items.length === 0 && (
              <tr>
                <td colSpan={8} className="hint">
                  没有符合条件的充值申请
                </td>
              </tr>
            )}
            {items.map((t) => {
              const mine = flight?.id === t.id ? flight : null;
              const writing = rejecting?.id === t.id ? rejecting : null;
              return [
                <tr key={t.id}>
                  <td>{t.tenant_name}</td>
                  <td className="num money">{fmtMicroUsd(t.amount_micro_usd)}</td>
                  <td>
                    {t.origin === "operator" ? (t.credit_reason ?? t.note) : t.note}
                    {t.status === "rejected" && t.reject_reason && (
                      <span className="cell-note">驳回原因：{t.reject_reason}</span>
                    )}
                  </td>
                  <td>{t.reference || "—"}</td>
                  <td>{ORIGIN_LABEL[t.origin]}</td>
                  <td>{t.requested_by?.display_name ?? "—"}</td>
                  <td className="tabular">{fmtUtc(t.requested_at)}</td>
                  <td className="right">
                    {t.status === "pending" && t.origin === "tenant" && canEdit && (
                      <>
                        <TwoStep
                          label="批准"
                          size="sm"
                          kind="primary"
                          warning="批准后金额立即计入该租户余额"
                          disabled={inFlight}
                          busy={mine?.kind === "approve"}
                          onConfirm={() => void decide(t, "approve")}
                        />{" "}
                        <Button
                          size="sm"
                          type="button"
                          disabled={inFlight || writing !== null}
                          onClick={() => {
                            setOutcome(null);
                            setRejecting({ id: t.id, reason: "", problem: null });
                          }}
                        >
                          驳回
                        </Button>
                      </>
                    )}
                    {t.status !== "pending" && (
                      <Lamp state={t.status === "approved" ? "applied" : "rejected"}>{STATUS_LABEL[t.status]}</Lamp>
                    )}
                  </td>
                </tr>,
                writing && (
                  <tr key={`${t.id}-reject`}>
                    <td colSpan={8}>
                      <form
                        ref={form}
                        aria-label={`驳回 ${t.tenant_name} 的充值申请`}
                        onSubmit={(e) => {
                          e.preventDefault();
                          confirmReject(t);
                        }}
                      >
                        <Field label="驳回原因" hint="租户会看到这段文字，1–500 个字符。" error={writing.problem}>
                          {(aria) => (
                            <textarea
                              {...aria}
                              rows={2}
                              value={writing.reason}
                              disabled={mine?.kind === "reject"}
                              onChange={(e) => setRejecting({ ...writing, reason: e.target.value, problem: null })}
                            />
                          )}
                        </Field>
                        <div className="r">
                          <Button
                            kind="danger"
                            type="submit"
                            size="sm"
                            disabled={inFlight && mine?.kind !== "reject"}
                            busy={mine?.kind === "reject"}
                            busyLabel="确认驳回"
                          >
                            确认驳回
                          </Button>
                          <Button size="sm" type="button" disabled={inFlight} onClick={() => setRejecting(null)}>
                            取消
                          </Button>
                        </div>
                      </form>
                    </td>
                  </tr>
                ),
              ];
            })}
          </tbody>
        </DataTable>
      )}
      {items !== null && next !== null && (
        <div className="r">
          <Button type="button" disabled={inFlight} onClick={() => void load(next)}>
            加载更多
          </Button>
        </div>
      )}
    </div>
  );
}
