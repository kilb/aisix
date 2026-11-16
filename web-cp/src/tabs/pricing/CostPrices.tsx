import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { costRates, type CostAccount, type CostRatePlan } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { CostAccountDetail } from "./CostAccountDetail";
import { Count } from "./kit";
import { scheduledWords } from "./GroupDetail";
import { Select } from "../../ui/Select";

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
          <Button className="finance-icon-button" aria-label="刷新" title="刷新" type="button" icon="refresh" onClick={() => void load()}></Button>
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
      <Card className="finance-facts-card">
        <dl className="compact-overview-facts">
          <div><dt>上游账号 · 全部</dt><dd>{rows === null ? "—" : fmtInt(rows.length)}</dd></div>
          <div><dt>未设现行默认成本换算率</dt><dd>{rows === null ? "—" : fmtInt(rows.length - configured)}</dd></div>
          <div><dt>默认成本换算率 · 待生效版本</dt><dd>{rows === null ? "—" : fmtInt(scheduledCount)}</dd></div>
        </dl>
        <p className="text-caption text-muted">成本只用于平台利润，对租户不可见。默认与单模型覆盖均未设置时，成本保持未知。</p>
      </Card>
      <Card className="finance-table-card">
        <CardHeader number={3} className="mb-0" title={<>上游账号{rows && <Count n={rows.length} />}</>} />
        {rows === null ? (
          !error && <Loading />
        ) : rows.length === 0 ? (
          <EmptyState text="还没有上游账号。先在「上游账号」中添加。" />
        ) : (
          <>
          <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索成本价账号", placeholder: "账号名称、ID 或供应商" }}>
            <label className="f filter"><span>默认成本换算率</span><Select size="compact" value={rateState} onValueChange={(value) => setRateState(value)}><option value="">全部</option><option value="configured">已设置</option><option value="missing">未设置</option><option value="scheduled">有待生效版本</option></Select></label>
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
                  <tr data-finance-record key={account.id}>
                    <td data-label="上游账号">
                      <button
                        type="button"
                        className="link-button"
                        title={account.display_name}
                        onClick={() => setSelected(account.id)}
                      >
                        {account.display_name || account.id}
                      </button>
                    </td>
                    <td data-label="提供方">
                      <span className="mono cost-provider">{account.provider}</span>
                    </td>
                    <td data-label="默认成本换算率" className="num">
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
