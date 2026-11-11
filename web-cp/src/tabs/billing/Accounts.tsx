import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { billing, type BillingAccount } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtMicroCny } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Card, CardHeader, PageHeader, Stat, StatGrid } from "../../components/page";
import { SIDE_OPEN } from "../Tenants";
import { AccountPanel, AccountState, MODE_LABEL } from "./AccountPanel";

/** 账户: every tenant's balance and credit limit, and one tenant's billing in a side panel. */
export function Accounts({ canEdit, operator, onGoto }: TabProps) {
  const [items, setItems] = useState<BillingAccount[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setItems(await billing.accounts());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);
  useEffect(() => {
    void load();
  }, [load]);

  // An action in the panel settled a new state of one account: the list shows it too.
  const take = useCallback((account: BillingAccount) => {
    setItems((list) => list && list.map((a) => (a.tenant_id === account.tenant_id ? account : a)));
  }, []);

  const open = items?.find((a) => a.tenant_id === selected) ?? null;
  // The figures count the customer tenants: the platform's own is never stopped and carries no balance to watch.
  const customers = items?.filter((a) => !a.is_internal) ?? null;
  const sum = (pick: (a: BillingAccount) => number) => (customers ?? []).reduce((n, a) => n + pick(a), 0);
  return (
    <>
      <PageHeader
        title="账户"
        description="可用额度 = 余额 + 信用额度，用尽即停止调用（预付透支到 0，后付同样透支到 0，月底结算）。平台内部租户不受限制，排在最后。"
        actions={
          <Button type="button" icon="refresh" onClick={() => void load()}>
            刷新
          </Button>
        }
      />
      <StatGrid className="grid-cols-2">
        <Stat label="客户租户" value={customers ? fmtInt(customers.length) : "…"} />
        <Stat label="余额合计" value={customers ? fmtMicroCny(sum((a) => a.balance_micro_cny)) : "…"} />
        <Stat label="已停止" value={customers ? fmtInt(customers.filter((a) => a.exhausted).length) : "…"} />
        <Stat label="待审批充值" value={customers ? fmtInt(sum((a) => a.pending_topups)) : "…"} />
      </StatGrid>
      <div className={open ? SIDE_OPEN : "with-side"}>
        <div className="side-main min-w-0">
          <Card className="mb-0">
            <CardHeader
              title={
                <>
                  账户列表
                  {items !== null && <span className="tag ml-2 align-[2px]">{fmtInt(items.length)}</span>}
                </>
              }
            />
            {error && (
              <div className="note crit" role="alert">
                {error}{" "}
                <Button type="button" size="sm" onClick={() => void load()}>
                  重试
                </Button>
              </div>
            )}
            {items === null ? (
              !error && <Loading />
            ) : items.length === 0 ? (
              <EmptyState text="还没有已开通的租户。" />
            ) : (
              <DataTable label="账户列表">
                <thead>
                  <tr>
                    <th>租户</th>
                    <th>模式</th>
                    <th className="num">余额</th>
                    <th className="num">信用额度</th>
                    <th className="num">可用</th>
                    <th>状态</th>
                    <th className="num side-opt">待审批充值</th>
                  </tr>
                </thead>
                <tbody>
                  {items.map((a) => (
                    <tr key={a.tenant_id}>
                      <td>
                        <span className="flex min-w-0 items-center gap-2">
                          <button
                            type="button"
                            className="link-button bl-name"
                            title={a.tenant_name}
                            aria-expanded={selected === a.tenant_id}
                            onClick={() => setSelected(a.tenant_id)}
                          >
                            {a.tenant_name}
                          </button>
                          {a.is_internal && <span className="tag">平台内部</span>}
                        </span>
                      </td>
                      <td className="text-text-2">{MODE_LABEL[a.billing_mode] ?? a.billing_mode}</td>
                      <td className="num">{fmtMicroCny(a.balance_micro_cny)}</td>
                      <td className="num">{fmtMicroCny(a.credit_limit_micro_cny)}</td>
                      <td className="num font-medium">{fmtMicroCny(a.available_micro_cny)}</td>
                      <td>{a.is_internal ? <span className="text-muted-foreground">—</span> : <AccountState account={a} />}</td>
                      <td className="num side-opt">{a.pending_topups}</td>
                    </tr>
                  ))}
                </tbody>
              </DataTable>
            )}
          </Card>
        </div>
        {open && (
          <AccountPanel
            account={open}
            canEdit={canEdit}
            isAdmin={operator.role === "admin"}
            onChanged={take}
            onGoto={onGoto}
            onClose={() => setSelected(null)}
          />
        )}
      </div>
    </>
  );
}
