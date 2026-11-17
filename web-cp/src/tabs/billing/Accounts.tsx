import { useRecordTable } from "../../lib/recordTable";
import { RecordTools, RecordPager } from "../../components/RecordTools";
import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { billing, type BillingAccount } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtMicroCny } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader } from "../../components/page";
import { exactInteger, formatCount } from "../../lib/chartFormat";
import { tenantHash } from "../../lib/navigation";
import { AccountState, MODE_LABEL } from "./AccountPanel";
import { Select } from "../../ui/Select";

// 概况使用四项紧凑指标，账户列表占满工作区。
/** 账户快照中的租户名称统一进入租户详情页。 */
export function Accounts(_props: TabProps) {
  const [items, setItems] = useState<BillingAccount[] | null>(null);
  const [error, setError] = useState<string | null>(null);

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

  const [modeFilter, setModeFilter] = useState("");
  const [balanceFilter, setBalanceFilter] = useState("");
  const table = useRecordTable({ items: (items ?? []).filter((a) => (!modeFilter || a.billing_mode === modeFilter) && (!balanceFilter || (a.exhausted ? "exhausted" : "available") === balanceFilter)), searchText: (a) => [a.tenant_id, a.tenant_name], resetKey: `${modeFilter}:${balanceFilter}` });
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
          <Button type="button" icon="refresh" className="finance-icon-button" aria-label="刷新" title="刷新账户" onClick={() => void load()} />
        }
      />
      <BentoGrid className="finance-stats" aria-label="账户概况">
        <Card className="finance-stat">
          <CardHeader title="客户租户" description={snapshotLabel} />
          {customers === null ? (error ? <CardMessage onRetry={() => void load()}>账户快照读取失败</CardMessage> : <MetricSkeleton />) : (
            <>
              <Metric size="figure">{fmtInt(customers.length)}</Metric>
              <p className="text-caption text-fg-3">额度未用尽 {fmtInt(customers.length - (stopped ?? 0))} 个，不含平台内部租户。</p>
            </>
          )}
        </Card>
        <Card className="finance-stat finance-balance-stat" tone="inverse">
          <CardHeader title="客户余额合计" description={snapshotLabel} />
          {customers === null ? (error ? <CardMessage onRetry={() => void load()}>账户快照读取失败</CardMessage> : <MetricSkeleton />) : (
            <>
              <Metric size="figure" className="break-all">{balance === null ? "金额不可精确汇总" : fmtMicroCny(balance)}</Metric>
              <p className="text-caption text-fg-3">可用额度 = 余额 + 信用额度，组织预算另行限制调用。</p>
            </>
          )}
        </Card>
        <Card className="finance-stat">
          <CardHeader title="额度已用尽" description="全部客户租户" />
          {customers === null ? (error ? <CardMessage>账户快照不可用</CardMessage> : <MetricSkeleton />) : <Metric size="figure">{fmtInt(stopped ?? 0)}</Metric>}
          <p className="text-caption text-fg-3">额度用尽后停止调用。</p>
        </Card>
        <Card className="finance-stat">
          <CardHeader title="待审批充值" description="全部客户租户" />
          {customers === null ? (error ? <CardMessage>账户快照不可用</CardMessage> : <MetricSkeleton />) : <Metric size="figure">{pending === null ? "不可精确汇总" : formatCount(pending)}</Metric>}
          <p className="text-caption text-fg-3">批准后计入对应租户余额。</p>
        </Card>
      </BentoGrid>
      <div className="finance-record-layout">
        <div className="side-main min-w-0">
          <Card className="finance-table-card">
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
              <>
                <RecordTools table={table} label="账户列表"><label className="f filter"><span>计费模式</span><Select size="compact" aria-label="计费模式" value={modeFilter} onValueChange={(value) => setModeFilter(value)}><option value="">全部</option><option value="prepaid">预付</option><option value="postpaid">后付</option></Select></label><label className="f filter"><span>余额状态</span><Select size="compact" aria-label="余额状态" value={balanceFilter} onValueChange={(value) => setBalanceFilter(value)}><option value="">全部</option><option value="exhausted">额度已用尽</option><option value="available">额度未用尽</option></Select></label></RecordTools>
                <DataTable label="账户列表">
                  <thead>
                    <tr>
                      <th>租户</th>
                      <th className="num">余额</th>
                      <th className="num">信用额度</th>
                      <th className="num">可用</th>
                      <th>状态</th>
                    </tr>
                  </thead>
                  <tbody>
                    {table.rows.map((a) => (
                      <tr data-finance-record key={a.tenant_id}>
                        <td data-label="租户">
                          <span className="flex min-w-0 items-center gap-2">
                            <a
                              className="link-button bl-name"
                              title={a.tenant_name}
                              href={tenantHash(a.tenant_id, "billing", "billing")}
                            >
                              {a.tenant_name}
                            </a>
                            {a.is_internal && <span className="tag">平台内部</span>}
                          </span>
                          <span className="cell-note">{MODE_LABEL[a.billing_mode] ?? a.billing_mode}</span>
                        </td>
                        <td data-label="余额" className="num">{fmtMicroCny(a.balance_micro_cny)}</td>
                        <td data-label="信用额度" className="num">{fmtMicroCny(a.credit_limit_micro_cny)}</td>
                        <td data-label="可用" className="num font-normal">{fmtMicroCny(a.available_micro_cny)}</td>
                        <td data-label="状态">{a.is_internal ? <span className="text-muted-foreground">—</span> : <AccountState account={a} />}
                          <span className="cell-note">待审批充值 {fmtInt(a.pending_topups)}</span>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </DataTable>
                <RecordPager table={table} label="账户列表" />
              </>
            )}
          </Card>
        </div>
      </div>
    </>
  );
}
