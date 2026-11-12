import { useRef, useState } from "react";
import { createOrgQuota, deleteOrgQuota, updateOrgQuota, billing, type Budget } from "../../lib/api";
import { fmtMicroCny, isCnyAmount, microCnyToInput } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { Lamp } from "../../ui/Lamp";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { refusal, useLoad } from "../Tenants";
import { MONEY_HINT, PERIOD_LABEL, Region, budgetPeriod, type RegionProps } from "./AccountPanel";
import { BudgetProgress } from "../../components/charts";
import { exactRatio } from "../../lib/chartFormat";

type Refusal = ReturnType<typeof refusal>;

function NewQuota({ account, onCreated }: { account: RegionProps["account"]; onCreated: () => Promise<void> }) {
  const toast = useToast();
  const [kind, setKind] = useState<"periodic" | "cumulative">("periodic");
  const [period, setPeriod] = useState<"day" | "week" | "month">("month");
  const [limit, setLimit] = useState("");
  const [busy, setBusy] = useState(false);
  const inFlight = useRef(false);
  const [local, setLocal] = useState<string | null>(null);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function create(e: React.FormEvent) {
    e.preventDefault();
    if (inFlight.current) return;
    const typed = limit.trim();
    setRefused(null);
    if (!isCnyAmount(typed, { allowZero: false })) {
      setLocal(MONEY_HINT);
      focusRefusal();
      return;
    }
    setLocal(null);
    inFlight.current = true;
    setBusy(true);
    try {
      await createOrgQuota(account.tenant_id, { kind, ...(kind === "periodic" ? { period } : {}), limit_cny: typed });
    } catch (err) {
      setRefused(refusal(err, (p) => p === "/kind" || p === "/period" || p === "/limit_cny"));
      inFlight.current = false;
      setBusy(false);
      focusRefusal();
      return;
    }
    setLimit("");
    toast("已新建");
    try {
      await onCreated();
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  }

  return (
    <form ref={form} onSubmit={(e) => void create(e)} noValidate>
      <div className="bl-controls bl-inline">
        <Field label="类型" error={refused?.at.get("/kind") ?? null}>
          {(aria) => (
            <select value={kind} disabled={busy} onChange={(e) => setKind(e.target.value as "periodic" | "cumulative")} {...aria}>
              <option value="periodic">按周期</option>
              <option value="cumulative">一次性额度</option>
            </select>
          )}
        </Field>
        {kind === "periodic" && (
          <Field label="周期" error={refused?.at.get("/period") ?? null}>
            {(aria) => (
              <select value={period} disabled={busy} onChange={(e) => setPeriod(e.target.value as "day" | "week" | "month")} {...aria}>
                {(["day", "week", "month"] as const).map((p) => (
                  <option key={p} value={p}>
                    {PERIOD_LABEL[p]}
                  </option>
                ))}
              </select>
            )}
          </Field>
        )}
        <Field label="限额（元）" error={local ?? refused?.at.get("/limit_cny") ?? null}>
          {(aria) => <input inputMode="decimal" value={limit} disabled={busy} onChange={(e) => setLimit(e.target.value)} {...aria} />}
        </Field>
        <Button kind="primary" type="submit" busy={busy} busyLabel="新建中…">
          新建
        </Button>
      </div>
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
    </form>
  );
}

/** One row's controls: 修改限额 in place, and 删除 as a two-step. */
function RowActions({ tenantId, budget, onChanged }: { tenantId: string; budget: Budget; onChanged: () => Promise<void> }) {
  const toast = useToast();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState(false);
  const [removing, setRemoving] = useState(false);
  const inFlight = useRef(false);
  const [local, setLocal] = useState<string | null>(null);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (inFlight.current) return;
    const typed = draft.trim();
    setRefused(null);
    if (!isCnyAmount(typed, { allowZero: false })) {
      setLocal(MONEY_HINT);
      focusRefusal();
      return;
    }
    setLocal(null);
    inFlight.current = true;
    setBusy(true);
    try {
      await updateOrgQuota(tenantId, budget.id, typed);
    } catch (err) {
      setRefused(refusal(err, (p) => p === "/limit_cny"));
      inFlight.current = false;
      setBusy(false);
      focusRefusal();
      return;
    }
    toast("已保存");
    try {
      await onChanged();
      setEditing(false);
    } finally {
      inFlight.current = false;
      setBusy(false);
    }
  }

  async function remove() {
    if (inFlight.current) return;
    inFlight.current = true;
    setRemoving(true);
    setRefused(null);
    try {
      await deleteOrgQuota(tenantId, budget.id);
      toast("已删除");
      await onChanged();
    } catch (err) {
      setRefused(refusal(err, () => false));
    } finally {
      inFlight.current = false;
      setRemoving(false);
    }
  }

  const problem = local ?? refused?.at.get("/limit_cny") ?? null;
  return (
    <div>
      {editing ? (
        <form ref={form} onSubmit={(e) => void save(e)} noValidate>
          <div className="bl-controls justify-end text-left">
            <Field label="新限额（元）" error={problem}>
              {(aria) => (
                <input inputMode="decimal" value={draft} disabled={busy} autoFocus onChange={(e) => setDraft(e.target.value)} {...aria} />
              )}
            </Field>
            <Button kind="primary" type="submit" size="sm" busy={busy} busyLabel="保存中…">
              保存
            </Button>
            <Button type="button" size="sm" disabled={busy} onClick={() => setEditing(false)}>
              取消
            </Button>
          </div>
        </form>
      ) : (
        <div className="bl-row-actions">
          <Button
            type="button"
            size="sm"
            onClick={() => {
              setDraft(microCnyToInput(budget.limit_micro_cny));
              setLocal(null);
              setRefused(null);
              setEditing(true);
            }}
          >
            修改限额
          </Button>
          <TwoStep label="删除" size="sm" busy={removing} warning="删除后，该额度立即不再限制这个组织" onConfirm={() => void remove()} />
        </div>
      )}
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
    </div>
  );
}

/**
 * 组织额度: the tenant's `team` quotas, which only operators set. A quota the
 * tenant set at another level (member, key) is in the read-only 额度 region.
 */
export function OrgQuota({ account, canEdit, onQuotasChanged }: RegionProps) {
  const budgets = useLoad(() => billing.budgets(account.tenant_id), [account.tenant_id]);
  const rows = budgets.data?.filter((b) => b.scope === "team") ?? null;
  const reload = async () => {
    await budgets.reload();
    onQuotasChanged?.();
  };
  return (
    <Region label="组织额度">
      <p className="hint">限制这个租户整个组织的花费，由运维分配。按周期的额度每个周期重新计算，一次性额度用完为止。</p>
      {canEdit && !account.is_internal && <NewQuota key={account.tenant_id} account={account} onCreated={reload} />}
      {account.is_internal ? (
        <p className="hint">平台内部租户不设额度。</p>
      ) : budgets.error ? (
        <div className="note crit" role="alert">
          {budgets.error}{" "}
          <Button type="button" size="sm" onClick={() => void budgets.reload()}>
            重试
          </Button>
        </div>
      ) : rows === null ? (
        <Loading />
      ) : rows.length === 0 ? (
        <EmptyState text="这个租户没有组织额度。" />
      ) : (
        <DataTable label="组织额度" compact>
          <thead>
            <tr>
              <th>类型</th>
              <th>周期（北京时间）</th>
              <th className="num">限额</th>
              <th className="num">已用</th>
              <th className="num">剩余</th>
              <th>状态</th>
              {canEdit && <th className="right">操作</th>}
            </tr>
          </thead>
          <tbody>
            {rows.map((b) => (
              <tr key={b.id}>
                <td>{b.kind === "cumulative" ? "一次性额度" : "按周期"}</td>
                <td>{budgetPeriod(b)}</td>
                <td className="num">{fmtMicroCny(b.limit_micro_cny)}</td>
                <td className="num">
                  {fmtMicroCny(b.spent_micro_cny)}
                  <BudgetProgress
                    value={exactRatio(b.spent_micro_cny, b.limit_micro_cny)}
                    accessibleLabel={`组织${budgetPeriod(b)}额度：已用 ${fmtMicroCny(b.spent_micro_cny)}，限额 ${fmtMicroCny(b.limit_micro_cny)}`}
                    className="mt-2 min-w-24"
                  />
                </td>
                <td className="num">{fmtMicroCny(b.remaining_micro_cny)}</td>
                <td>
                  <span data-state={b.exhausted ? "exhausted" : "ok"}>
                    <Lamp state={b.exhausted ? "rejected" : "applied"}>{b.exhausted ? "已用尽" : "正常"}</Lamp>
                  </span>
                </td>
                {canEdit && (
                  <td className="right">{b.set_by === "operator" ? <RowActions tenantId={account.tenant_id} budget={b} onChanged={reload} /> : null}</td>
                )}
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </Region>
  );
}
