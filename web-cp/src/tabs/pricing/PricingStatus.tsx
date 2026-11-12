import { useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { pricingStatus, type PricingStatus as Status } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Lamp } from "../../ui/Lamp";
import { fmtBeijing } from "../../lib/time";
import { useRecordTable } from "../../lib/recordTable";
import { handChargesFilter, statusView } from "../../lib/pricingStatus";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { RankBars } from "../../components/charts";
import { normalizeExactValues } from "../../lib/chartFormat";

/**
 * 计价状态: the pricing version, price revisions that cannot be used, and the
 * last day's unpriced calls, each kind by reason and model with what to do
 * about it and a link to its calls in 计费明细. When the list of kinds is cut
 * short, each reason's calls it leaves out are one more row of that reason.
 * Calls a re-pricing run has corrected since are counted apart.
 */
export function PricingStatus({ onGoto }: TabProps) {
  const [status, setStatus] = useState<Status | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [reading, setReading] = useState(false);
  const [reasonFilter, setReasonFilter] = useState("");
  const [tableFilter, setTableFilter] = useState("");

  async function read() {
    setReading(true);
    try {
      setStatus(await pricingStatus.get());
      setFailure(null);
    } catch (e) {
      setFailure(errorText(e));
    } finally {
      setReading(false);
    }
  }

  useEffect(() => {
    void read();
  }, []);

  const view = status ? statusView(status) : null;
  const reasonValues = normalizeExactValues(view?.unpriced.map((r) => r.count) ?? []);
  const unpricedTable = useRecordTable({
    items: [
      ...(view?.details ?? []).map((row) => ({ kind: "detail" as const, ...row })),
      ...(view?.unlisted ?? []).map((row) => ({ kind: "unlisted" as const, ...row })),
    ].filter((row) => !reasonFilter || row.code === reasonFilter),
    searchText: (row) => [row.code, row.label, ...(row.kind === "detail" ? [row.model, row.scope, row.remedy] : [])],
    resetKey: reasonFilter,
  });
  const invalidTable = useRecordTable({
    items: (view?.invalid ?? []).filter((row) => !tableFilter || row.table === tableFilter),
    searchText: (row) => [row.table, row.revision, row.reason],
    resetKey: tableFilter,
  });
  return (
    <>
      <PageHeader
        title="计价状态"
        eyebrow="价格 / 运行状态"
        dataLead={view ? `最近 24 小时仍有 ${fmtInt(view.unpricedTotal)} 次调用未计价；另有 ${fmtInt(view.correctedTotal)} 次已通过重计价更正。` : "读取计价引擎与最近 24 小时状态。"}
        description="计价引擎的版本、不可用的价格版本，以及最近 24 小时没能计价的调用。"
        actions={
          <Button type="button" icon="refresh" busy={reading} busyLabel="读取中…" onClick={() => void read()}>
            刷新
          </Button>
        }
      />
      {failure && (
        <div className="note crit mt-0" role="alert">
          {failure}
        </div>
      )}
      <BentoGrid className="mb-3">
        <Card className={span.d7}>
          <CardHeader number={1} title="未计价调用 · 最近 24 小时" />
          {view ? <Metric size="hero">{fmtInt(view.unpricedTotal)}</Metric> : failure ? <CardMessage onRetry={() => void read()}>计价状态读取失败</CardMessage> : <MetricSkeleton />}
          {view && <div className="mt-6"><RankBars items={view.unpriced.flatMap((r, i) => reasonValues[i] === null ? [] : [{ label: r.label, value: reasonValues[i] ?? 0, displayValue: `${fmtInt(r.count)} 次` }])} accessibleLabel="最近24小时全部未计价调用按原因的数量" emptyLabel="最近 24 小时没有未计价调用" /></div>}
          <p className="mt-4 text-caption text-muted">已经重计价更正的调用单独列出，不计入未计价总数。</p>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="不可用的价格版本 · 当前" />
          {view ? <Metric size="display">{fmtInt(view.invalid.length)}</Metric> : failure ? <CardMessage>当前数据不可用</CardMessage> : <MetricSkeleton />}
          <dl className="mt-6 border-t border-surface/12 pt-4">
            <dt className="font-mono text-micro text-on-inverse-muted">价格版本</dt>
            <dd className="mt-1 break-all font-mono text-stat">{status?.pricing_version ?? "未知"}</dd>
          </dl>
        </Card>
      </BentoGrid>
      {status && view && (
        <>
          <Card className="mb-3">
            <CardHeader number={3} className={view.unpricedTotal === 0 ? undefined : "mb-0"} title="最近 24 小时未计价" />
            {view.unpricedTotal > 0 && <RecordTableToolbar search={{ value: unpricedTable.query, onChange: unpricedTable.setQuery, label: "搜索未计价分组", placeholder: "按模型、原因或分组搜索" }}>
              <label className="field mb-0"><span className="field-label">未计价原因</span><select value={reasonFilter} onChange={(e) => setReasonFilter(e.target.value)}><option value="">全部原因</option>{view.unpriced.map((row) => <option key={row.code} value={row.code}>{row.label}</option>)}</select></label>
              <p className="hint mb-0">未逐一列出的调用按原因汇总；合计包含全部未计价调用。</p>
            </RecordTableToolbar>}
            {view.unpricedTotal === 0 ? (
              <p className="status-line">
                <Lamp state="applied">最近 24 小时没有未计价的调用。</Lamp>
              </p>
            ) : (
              <><DataTable label="未计价的调用" scroll>
                <thead>
                  <tr>
                    <th>原因</th>
                    <th className="num">次数</th>
                    <th>模型</th>
                    <th>档位 / 分组</th>
                    <th>最近一次（北京时间）</th>
                    <th>处理</th>
                    <th className="right">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {unpricedTable.rows.length === 0 && <tr><td colSpan={7}><EmptyState text="没有符合筛选条件的未计价分组" /></td></tr>}
                  {unpricedTable.rows.map((d) => (
                    <tr key={d.key}>
                      <td className="font-medium">{d.label}</td>
                      <td className="num">{fmtInt(d.count)}</td>
                      {d.kind === "detail" ? <><td>{d.model}</td><td>{d.scope}</td><td className="tabular">{fmtBeijing(d.latestAt)}</td><td className="max-w-[28rem] whitespace-normal text-caption text-ink-soft">{d.remedy}</td></> : <td colSpan={4}>其余 {fmtInt(d.count)} 次，未逐一列出</td>}
                      <td className="right">
                        <Button
                          type="button"
                          size="sm"
                          onClick={() => {
                            handChargesFilter(d.filter);
                            onGoto("billing", "charges");
                          }}
                        >
                          查看调用
                        </Button>
                      </td>
                    </tr>
                  ))}
                  <tr className="bg-well font-medium">
                    <th scope="row" className="!static !h-12 !border-b-0 !bg-transparent !text-body !font-medium !text-ink">
                      合计
                    </th>
                    <td className="num">{fmtInt(view.unpricedTotal)}</td>
                    <td colSpan={5} />
                  </tr>
                </tbody>
              </DataTable><RecordTablePager page={unpricedTable.page} pageSize={unpricedTable.pageSize} onPageChange={unpricedTable.setPage} onPageSizeChange={unpricedTable.setPageSize} total={unpricedTable.filteredCount} label="未计价分组分页" /></>
            )}
            {view.detailsTruncated && (
              <p className="hint mt-4">种类太多，没有逐一列出全部；每种原因没有列出的次数合在它的「其余 … 次」一行里，合计仍是全部未计价的调用。</p>
            )}
          </Card>
          {view.correctedTotal > 0 && (
            <p className="hint mb-4" role="status">
              另有 {fmtInt(view.correctedTotal)} 次调用已通过重计价更正，不再计入未计价。
            </p>
          )}

          <Card className="mb-0">
            <CardHeader number={4} className={view.invalid.length === 0 ? undefined : "mb-0"} title="不可用的价格版本" />
            {view.invalid.length > 0 && <RecordTableToolbar search={{ value: invalidTable.query, onChange: invalidTable.setQuery, label: "搜索价格版本", placeholder: "按版本、表或原因搜索" }}>
              <label className="field mb-0"><span className="field-label">价格表</span><select value={tableFilter} onChange={(e) => setTableFilter(e.target.value)}><option value="">全部价格表</option>{[...new Set(view.invalid.map((row) => row.table))].map((table) => <option key={table} value={table}>{table}</option>)}</select></label>
            </RecordTableToolbar>}
            {view.invalid.length === 0 ? (
              <p className="status-line">
                <Lamp state="applied">没有不可用的价格版本。</Lamp>
              </p>
            ) : (
              <><DataTable label="不可用的价格版本">
                <thead>
                  <tr>
                    <th>表</th>
                    <th>版本</th>
                    <th>原因</th>
                  </tr>
                </thead>
                <tbody>
                  {invalidTable.rows.length === 0 && <tr><td colSpan={3}><EmptyState text="没有符合筛选条件的价格版本" /></td></tr>}
                  {invalidTable.rows.map((r) => (
                    <tr key={r.key}>
                      <td>{r.table}</td>
                      <td className="tabular">{r.revision}</td>
                      <td className="whitespace-normal">{r.reason}</td>
                    </tr>
                  ))}
                </tbody>
              </DataTable><RecordTablePager page={invalidTable.page} pageSize={invalidTable.pageSize} onPageChange={invalidTable.setPage} onPageSizeChange={invalidTable.setPageSize} total={invalidTable.filteredCount} label="不可用价格版本分页" /></>
            )}
          </Card>
        </>
      )}
    </>
  );
}
