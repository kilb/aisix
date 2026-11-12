import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { costRates, type CostAccount, type CostRatePlan } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { CostAccountDetail } from "./CostAccountDetail";
import { Count } from "./kit";
import { scheduledWords } from "./GroupDetail";

/**
 * 价格 · 成本价: every upstream account and its default cost rate, in CNY per
 * USD of a model's official price. Selecting one opens its per-model rates.
 *
 * A cost is the same official price a tenant's charge is built on, times the
 * account's rate: the platform's cost for a call, never shown to a tenant.
 */
export function CostPrices({ canEdit }: TabProps) {
  const [rows, setRows] = useState<{ account: CostAccount; default: CostRatePlan }[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setRows(await costRates.accounts());
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const [rateState, setRateState] = useState("");
  const records = useRecordTable({
    items: (rows ?? []).filter((row) => rateState === "" || (rateState === "configured" ? row.default.current?.cny_per_usd != null : rateState === "missing" ? row.default.current?.cny_per_usd == null : row.default.upcoming.length > 0)),
    searchText: (row) => [row.account.display_name, row.account.id, row.account.provider],
    resetKey: rateState,
  });
  const open = rows?.find((r) => r.account.id === selected)?.account ?? null;
  if (open) {
    return <CostAccountDetail account={open} canEdit={canEdit} onBack={() => setSelected(null)} onChanged={load} />;
  }
  const configured = rows?.filter((r) => r.default.current?.cny_per_usd != null).length ?? 0;
  const scheduledCount = rows?.reduce((n, r) => n + r.default.upcoming.length, 0) ?? 0;

  return (
    <>
      <PageHeader
        title="成本价"
        eyebrow="价格 / 成本"
        dataLead={rows === null ? "读取账号成本换算率。" : `全部 ${fmtInt(rows.length)} 个账号中，${fmtInt(configured)} 个已设置现行默认成本换算率。`}
        description="成本价按上游账号设置换算率：模型的官方美元价 × 这个账号在模型上的成本换算率（没有时为账号默认），就是平台为一次调用付给上游的人民币成本。只用于利润报表，租户看不到。换算率只能从现在或将来开始生效；账号没有成本换算率时，它的调用成本记为未知。"
        actions={
          <Button type="button" icon="refresh" onClick={() => void load()}>
            刷新
          </Button>
        }
      />
      {error && (
        <div className="note crit mt-0" role="alert">
          {error}{" "}
          <Button type="button" onClick={() => void load()}>
            重试
          </Button>
        </div>
      )}
      <BentoGrid className="mb-3">
        <Card className={span.d7}>
          <CardHeader number={1} title="上游账号 · 全部" />
          {rows === null ? error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton /> : <Metric size="hero">{fmtInt(rows.length)}</Metric>}
          <p className="mt-4 text-caption text-muted">成本只用于平台利润报表，对租户不可见。</p>
          <p className="mt-6 border-t border-ink/7 pt-4 text-body">{rows === null ? "—" : fmtInt(scheduledCount)} 个默认成本换算率版本待生效。</p>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="未设现行默认成本换算率" />
          {rows === null ? error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton /> : <Metric size="display">{fmtInt(rows.length - configured)}</Metric>}
          <p className="mt-4 text-caption text-on-inverse-muted">单模型覆盖仍可能提供成本；默认与模型覆盖都未设置的调用，成本记为未知。</p>
        </Card>
      </BentoGrid>
      <Card className="mb-0">
        <CardHeader number={3} className="mb-0" title={<>上游账号{rows && <Count n={rows.length} />}</>} />
        {rows === null ? (
          !error && <Loading />
        ) : rows.length === 0 ? (
          <EmptyState text="还没有上游账号。先在「上游账号」中添加。" />
        ) : (
          <>
          <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索成本价账号", placeholder: "账号名称、ID 或供应商" }}>
            <label className="f filter"><span>默认成本换算率</span><select value={rateState} onChange={(e) => setRateState(e.target.value)}><option value="">全部</option><option value="configured">已设置</option><option value="missing">未设置</option><option value="scheduled">有待生效版本</option></select></label>
          </RecordTableToolbar>
          <DataTable label="上游账号成本价">
            <thead>
              <tr>
                <th>上游账号</th>
                <th>提供方</th>
                <th className="num">默认成本换算率</th>
              </tr>
            </thead>
            <tbody>
              {records.rows.length === 0 && <tr><td colSpan={3}><EmptyState text="没有符合筛选条件的账号" /></td></tr>}
              {records.rows.map(({ account, default: plan }) => {
                const later = scheduledWords(plan, "");
                return (
                  <tr key={account.id}>
                    <td>
                      <button
                        type="button"
                        className="link-button"
                        title={account.display_name}
                        onClick={() => setSelected(account.id)}
                      >
                        {account.display_name || account.id}
                      </button>
                    </td>
                    <td>
                      <span className="mono cost-provider">{account.provider}</span>
                    </td>
                    <td className="num">
                      <span className="tabular">{plan.current?.cny_per_usd ?? "未设置"}</span>
                      {later && <span className="cell-note">待生效 {later}</span>}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </DataTable>
          <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="上游账号成本价分页" />
          </>
        )}
      </Card>
    </>
  );
}
