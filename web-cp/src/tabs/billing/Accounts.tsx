import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { billing, type BillingAccount } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtMicroCny } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { RankBars } from "../../components/charts";
import { exactInteger, formatCount } from "../../lib/chartFormat";
import { SIDE_OPEN } from "../Tenants";
import { AccountPanel, AccountState, MODE_LABEL } from "./AccountPanel";

// 桌面/平板：6 + 6；账户列表与详情：12。
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
  // 全量账户快照用整数求和；无法精确表示的上游数值不冒充已知金额。
  const sum = (pick: (a: BillingAccount) => number): bigint | null => {
    if (customers === null) return null;
    let total = 0n;
    for (const account of customers) {
      const value = exactInteger(pick(account));
      if (value === null) return null;
      total += value;
    }
    return total;
  };
  const balance = sum((a) => a.balance_micro_cny);
  const pending = sum((a) => a.pending_topups);
  const stopped = customers?.filter((a) => a.exhausted).length ?? null;
  const snapshotLabel = error ? "最近成功读取" : "当前快照";
  return (
    <>
      <PageHeader
        title="账户"
        eyebrow="计费 / 账户快照"
        dataLead={customers !== null ? <>{error ? "刷新失败，以下为最近成功读取值。" : ""}全部客户租户 <strong>{fmtInt(customers.length)}</strong> 个，其中 <strong>{fmtInt(stopped ?? 0)}</strong> 个额度已用尽。</> : error ? "账户快照读取失败，请重试。" : "正在读取全部账户快照。"}
        description="可用额度 = 余额 + 信用额度，用尽即停止调用（预付透支到 0，后付同样透支到 0，月底结算）。平台内部租户不受限制，排在最后。"
        actions={
          <Button type="button" icon="refresh" onClick={() => void load()}>
            刷新
          </Button>
        }
      />
      <BentoGrid className="mb-3" aria-label="账户概况">
        <Card className={span.d6}>
          <CardHeader number={1} title={`客户租户 · ${snapshotLabel}`} description="统计全部客户租户，不含平台内部租户。" />
          {customers === null ? (error ? <CardMessage onRetry={() => void load()}>账户快照读取失败</CardMessage> : <MetricSkeleton />) : (
            <>
              <Metric size="hero">{fmtInt(customers.length)}</Metric>
              <CardFooter>
                <RankBars
                  accessibleLabel={`全部 ${fmtInt(customers.length)} 个客户租户中，${fmtInt(stopped ?? 0)} 个额度已用尽`}
                  items={[
                    { label: "额度未用尽", value: customers.length - (stopped ?? 0), displayValue: fmtInt(customers.length - (stopped ?? 0)) },
                    { label: "额度已用尽", value: stopped ?? 0, displayValue: fmtInt(stopped ?? 0) },
                  ]}
                />
              </CardFooter>
            </>
          )}
        </Card>
        <Card className={span.d6} tone="inverse">
          <CardHeader number={2} title={`客户余额合计 · ${snapshotLabel}`} description="余额与信用额度分开记录；组织预算另行限制调用。" />
          {customers === null ? (error ? <CardMessage onRetry={() => void load()}>账户快照读取失败</CardMessage> : <MetricSkeleton />) : (
            <>
              <Metric size="value" className="break-all">{balance === null ? "金额不可精确汇总" : fmtMicroCny(balance)}</Metric>
              <CardFooter>
                <dl className="grid gap-4 border-t border-surface/12 pt-5 tablet:grid-cols-2">
                  <div>
                    <dt className="font-mono text-meta text-on-inverse-muted">待审批充值</dt>
                    <dd className="mt-2 text-value font-normal tabular-nums">{pending === null ? "不可精确汇总" : formatCount(pending)}</dd>
                  </div>
                  <div>
                    <dt className="font-mono text-meta text-on-inverse-muted">可用额度计算</dt>
                    <dd className="mt-2 text-body">余额 + 信用额度</dd>
                  </div>
                </dl>
              </CardFooter>
            </>
          )}
        </Card>
      </BentoGrid>
      <div className={open ? SIDE_OPEN : "with-side"}>
        <div className="side-main min-w-0">
          <Card className="mb-0">
            <CardHeader
              number={3}
              description="可用额度 = 余额 + 信用额度，用尽即停止调用（预付透支到 0，后付同样透支到 0，月底结算）。平台内部租户不受限制，排在最后。"
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
                      <td className="num font-normal">{fmtMicroCny(a.available_micro_cny)}</td>
                      <td>{a.is_internal ? <span className="text-muted-foreground">—</span> : <AccountState account={a} />}</td>
                      <td className="num side-opt">{fmtInt(a.pending_topups)}</td>
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
