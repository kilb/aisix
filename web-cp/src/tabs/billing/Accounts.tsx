import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { billing, type BillingAccount } from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { fmtMicroUsd } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { AccountPanel, AccountState, MODE_LABEL } from "./AccountPanel";
import "./billing.css";

/** 账户: every tenant's balance and credit limit, and one tenant's billing in a side panel. */
export function Accounts({ canEdit, operator }: TabProps) {
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
  return (
    <div className={open ? "with-side is-open" : "with-side"}>
      <div className="side-main">
        <div className="panel">
          <h2>
            账户
            <Button size="sm" type="button" icon="refresh" onClick={() => void load()}>
              刷新
            </Button>
          </h2>
          <p className="hint">
            预付租户余额为零即停止调用；后付租户用尽信用额度即停止。平台内部租户不受限制，排在最后。
          </p>
          {error && (
            <div className="note crit" role="alert">
              {error}{" "}
              <Button type="button" onClick={() => void load()}>
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
                  <th className="num">待审批充值</th>
                </tr>
              </thead>
              <tbody>
                {items.map((a) => (
                  <tr key={a.tenant_id}>
                    <td>
                      <button
                        type="button"
                        className="link-button bl-name"
                        title={a.tenant_name}
                        aria-expanded={selected === a.tenant_id}
                        onClick={() => setSelected(a.tenant_id)}
                      >
                        {a.tenant_name}
                      </button>
                      {a.is_internal && <span className="tag bl-mark">平台内部</span>}
                    </td>
                    <td>{MODE_LABEL[a.billing_mode] ?? a.billing_mode}</td>
                    <td className="num">{fmtMicroUsd(a.balance_micro_usd)}</td>
                    <td className="num">{a.billing_mode === "postpaid" ? fmtMicroUsd(a.credit_limit_micro_usd) : "—"}</td>
                    <td className="num">{fmtMicroUsd(a.available_micro_usd)}</td>
                    <td>{a.is_internal ? "—" : <AccountState account={a} />}</td>
                    <td className="num">{a.pending_topups}</td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
          )}
        </div>
      </div>
      {open && <AccountPanel account={open} canEdit={canEdit} isAdmin={operator.role === "admin"} onChanged={take} onClose={() => setSelected(null)} />}
    </div>
  );
}
