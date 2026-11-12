import { useRecordTable } from "../../lib/recordTable";
import { RecordTools, RecordPager } from "../../components/RecordTools";
import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { ApiError, reconciliation, tenants, type Reconciliation as Answer, type Tenant } from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { fmtMicroCny } from "../../lib/money";
import { fmtBeijing } from "../../lib/time";
import { reasonLabel } from "../../lib/pricingStatus";
import { checkLabel, dropLabel, failureText } from "../../lib/reconcile";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { BentoGrid, Card, CardFooter, CardHeader, Metric, PageHeader, span } from "../../components/page";
import { RankBars } from "../../components/charts";
import { exactInteger, formatCount } from "../../lib/chartFormat";
import { Lamp } from "../../ui/Lamp";

// 桌面/平板：12；6 + 6；12。
/**
 * "对账": whether each tenant's ledger matches what moved it, the calls
 * charged with no ledger entry, the calls stored unpriced in the last 24
 * hours, and the usage the gateways report they dropped. It only reads.
 * A tenant filter narrows the first two; the unpriced count and the usage
 * loss always cover everything, and the page says so.
 */
export function Reconciliation(_props: TabProps) {
  const [tenantList, setTenantList] = useState<Tenant[]>([]);
  const [tenantId, setTenantId] = useState("");
  const [answer, setAnswer] = useState<Answer | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Said once when the chosen tenant turned out not to exist; cleared by the next choice or refresh.
  const [reset, setReset] = useState<string | null>(null);
  const [tenantsFailed, setTenantsFailed] = useState(false);
  const latest = useRef(0);

  useEffect(() => {
    tenants.list().then(setTenantList, () => setTenantsFailed(true));
  }, []);

  const load = useCallback(async (tenant: string) => {
    const mine = ++latest.current;
    setLoading(true);
    try {
      const next = await reconciliation.get(tenant || undefined);
      if (mine !== latest.current) return;
      setAnswer(next);
      setError(null);
    } catch (e) {
      if (mine !== latest.current) return;
      setAnswer(null);
      if (tenant && e instanceof ApiError && e.status === 404) {
        // The tenant is gone: look at everyone instead of retrying the same 404, and say so.
        setReset(failureText(e, errorText));
        setError(null);
        setTenantId("");
        tenants.list().then(setTenantList, () => undefined);
        return;
      }
      setError(failureText(e, errorText));
    } finally {
      if (mine === latest.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(tenantId);
  }, [load, tenantId]);

  // What the answer on screen covers, not what the select says while a new answer is on its way.
  const filtered = answer?.tenant_id != null;
  const loss = answer?.usage_loss;
  const unpriced = answer?.unpriced_last_24h;
  const reasonEntries = Object.entries(unpriced?.by_code ?? {});
  const canChartReasons = reasonEntries.every(([, value]) => exactInteger(value) !== null);
  const [checkFilter, setCheckFilter] = useState("");
  const mismatchTable = useRecordTable({ items: (answer?.mismatches ?? []).filter((m) => !checkFilter || m.check === checkFilter), searchText: (m) => [m.tenant_id, m.tenant_name ?? "", m.check], resetKey: `${tenantId}:${checkFilter}` });
  const instanceTable = useRecordTable({ items: loss?.instances ?? [], searchText: (i) => [i.instance_id, i.hostname, i.credential_name], resetKey: tenantId });

  return (
    <>
      <PageHeader
        title="对账"
        eyebrow="计费 / 只读核对"
        dataLead={answer ? <>检查于 {fmtBeijing(answer.checked_at)}；账实不符 <strong>{formatCount(answer.mismatches.length)}</strong> 项，已计费未入账 <strong>{formatCount(answer.charged_without_entry)}</strong> 笔。</> : error ? "对账结果读取失败，请重试。" : "正在读取账本、计费和网关用量核对结果。"}
        description="核对账本与扣费、调整、充值是否一致，并列出网关报告丢弃的用量。这里只读，不会改动任何数据。"
        actions={
          <>
            <label className="f filter">
              <span>租户</span>
              <select
                value={tenantId}
                disabled={loading}
                onChange={(e) => {
                  setReset(null);
                  setTenantId(e.target.value);
                }}
              >
                <option value="">全部</option>
                {tenantList.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                  </option>
                ))}
              </select>
            </label>
            <Button
              type="button"
              icon="refresh"
              busy={loading}
              busyLabel="刷新"
              onClick={() => {
                setReset(null);
                void load(tenantId);
              }}
            >
              刷新
            </Button>
          </>
        }
      />

      {tenantsFailed && <p className="hint mb-4">租户列表载入失败，目前只能查看全部租户。刷新页面后重试。</p>}
      {reset && (
        <div className="note crit" role="alert">
          {reset}
        </div>
      )}
      {error && (
        <div className="note crit" role="alert">
          {error}
        </div>
      )}
      {answer === null && !error && (
        <Card>
          <CardHeader number={1} title="账务核对结果" />
          <Loading />
        </Card>
      )}

      {answer && loss && unpriced && (
        <>
          <BentoGrid aria-label="账务核对结果">
            <Card
              aria-label="账实核对"
              className={span.d12}
            >
              <CardHeader number={1} title="账实核对" description={`${filtered ? "核对所选租户。" : "核对全部租户。"}这里只读，不会改动任何数据。`} />
              {answer.mismatches.length === 0 ? (
                <p className="text-stat font-normal">
                  <Lamp state="applied">账实相符</Lamp>
                </p>
              ) : (
                <>
                  <p className="hint text-caption">「扣费与计费」不符时，已计费但未入账的调用也在其中，同时计入下方「已计费但未入账」。</p>
                  <>
                    <RecordTools table={mismatchTable} label="账实不符"><label className="f filter"><span>核对项目</span><select aria-label="核对项目" value={checkFilter} onChange={(e) => setCheckFilter(e.target.value)}><option value="">全部</option>{[...new Set((answer?.mismatches ?? []).map((m) => m.check))].map((c) => <option key={c} value={c}>{checkLabel(c)}</option>)}</select></label></RecordTools>
                    <DataTable label="账实不符">
                      <thead>
                        <tr>
                          <th>租户</th>
                          <th>项目</th>
                          <th className="num">应为</th>
                          <th className="num">实为</th>
                          <th className="num">流水号</th>
                        </tr>
                      </thead>
                      <tbody>
                        {mismatchTable.rows.map((m) => (
                          <tr key={`${m.tenant_id}-${m.check}`}>
                            <td className="font-medium">{m.tenant_name || m.tenant_id}</td>
                            <td>{checkLabel(m.check)}</td>
                            <td className="num money">{fmtMicroCny(m.expected_micro_cny)}</td>
                            <td className="num money text-ink">{fmtMicroCny(m.actual_micro_cny)}</td>
                            <td className="num">{m.seq ?? "—"}</td>
                          </tr>
                        ))}
                      </tbody>
                    </DataTable>
                    <RecordPager table={mismatchTable} label="账实不符" />
                  </>
                </>
              )}
            </Card>

            <Card aria-label="已计费但未入账" className={span.d6} tone="inverse">
              <CardHeader number={2} title="已计费但未入账" />
              <Metric size="hero">{formatCount(answer.charged_without_entry)}</Metric>
              <p className="mt-3 font-mono text-meta text-on-inverse-muted">笔调用</p>
              <p className="mt-5 text-caption text-on-inverse-muted">
                {filtered ? "仅统计所选租户。" : "统计全部租户。"}已计费的调用在账本里没有对应流水。
              </p>
            </Card>

            <Card aria-label="近 24 小时未计价" className={span.d6}>
              <CardHeader
                number={3}
                title="近 24 小时未计价"
                description={filtered ? "不随租户筛选变化，始终统计全部租户。" : "统计全部租户。"}
              />
              <Metric size="display">{formatCount(unpriced.total)}</Metric>
              <CardFooter>
                {canChartReasons ? <RankBars
                  accessibleLabel={`全部租户近 24 小时未计价 ${formatCount(unpriced.total)} 笔，按原因列出`}
                  emptyLabel="近 24 小时没有未计价调用"
                  items={reasonEntries.map(([code, value]) => ({ label: reasonLabel(code), value, displayValue: formatCount(value) }))}
                /> : <p className="text-caption text-muted">原因计数无法精确表示，暂不绘制分布。</p>}
              </CardFooter>
              <div className="bl-box mt-5">
                <DataTable label="未计价的原因">
                  <tbody>
                    <tr>
                      <th scope="row" className="font-medium text-foreground">
                        合计
                      </th>
                      <td className="num font-normal">{formatCount(unpriced.total)}</td>
                    </tr>
                    {Object.entries(unpriced.by_code).map(([code, n]) => (
                      <tr key={code}>
                        <th scope="row">{reasonLabel(code)}</th>
                        <td className="num">{formatCount(n)}</td>
                      </tr>
                    ))}
                  </tbody>
                </DataTable>
              </div>
            </Card>
          <Card aria-label="疑似用量丢失" className={span.d12}>
            <CardHeader
              number={4}
              title="疑似用量丢失"
              description={
                <>
                  {filtered ? "不随租户筛选变化，始终统计全部网关。" : "统计全部网关。"}
                  取最近 24 小时内有心跳的实例，自实例启动起，按其最后一次心跳报告的数量；心跳之后的丢弃不在其中，所以实际可能更多。
                </>
              }
            />
            <div className="bl-box max-w-md">
              <DataTable label="丢弃合计">
                <tbody>
                  <tr>
                    <th scope="row">已丢弃</th>
                    <td className="num font-normal">{formatCount(loss.dropped_total)}</td>
                  </tr>
                  <tr>
                    <th scope="row">未确认送达（可能已入账）</th>
                    <td className="num font-normal">{formatCount(loss.unconfirmed_total)}</td>
                  </tr>
                </tbody>
              </DataTable>
            </div>
            {loss.instances.length === 0 ? (
              <p className="hint mt-4 text-caption">近 24 小时内没有网关实例上报过心跳。</p>
            ) : (
              <>
                <RecordTools table={instanceTable} label="各实例的丢弃明细"></RecordTools>
                <DataTable label="各实例的丢弃明细">
                  <thead>
                    <tr>
                      <th>凭据</th>
                      <th>实例</th>
                      <th>主机</th>
                      <th>最近心跳</th>
                      <th>丢弃明细</th>
                    </tr>
                  </thead>
                  <tbody>
                    {instanceTable.rows.map((i) => {
                      const reasons = Object.entries(i.drops).filter(([, n]) => n > 0);
                      return (
                        <tr key={i.dp_id + i.instance_id}>
                          <td className="font-medium">{i.credential_name}</td>
                          <td className="tabular bl-wrap font-mono text-caption">{i.instance_id}</td>
                          <td className="bl-wrap">{i.hostname}</td>
                          <td className="tabular text-text-2">{fmtBeijing(i.last_seen_at)}</td>
                          <td>
                            {reasons.length === 0 ? (
                              <span className="hint">无</span>
                            ) : (
                              reasons.map(([reason, n]) => (
                                <span key={reason} className="cell-note">
                                  {dropLabel(reason)}：{formatCount(n)}
                                </span>
                              ))
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </DataTable>
                <RecordPager table={instanceTable} label="各实例的丢弃明细" />
              </>
            )}
          </Card>
          </BentoGrid>
        </>
      )}
    </>
  );
}
