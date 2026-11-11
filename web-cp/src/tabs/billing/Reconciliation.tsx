import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { ApiError, reconciliation, tenants, type Reconciliation as Answer, type Tenant } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtMicroCny, fmtUtc } from "../../lib/money";
import { reasonLabel } from "../../lib/pricingStatus";
import { checkLabel, dropLabel, failureText } from "../../lib/reconcile";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { cn } from "../../lib/cn";
import { Lamp } from "../../ui/Lamp";

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

  return (
    <>
      <PageHeader
        title="对账"
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
          <Loading />
        </Card>
      )}

      {answer && loss && unpriced && (
        <>
          <p className="mb-4 text-[13px] text-muted-foreground">检查时间：{fmtUtc(answer.checked_at)}</p>

          <div className="mb-4 grid items-stretch gap-4 lg:grid-cols-3">
            <Card
              aria-label="账实核对"
              className={cn("mb-0 flex flex-col", answer.mismatches.length > 0 && "lg:col-span-3")}
            >
              <CardHeader title="账实核对" />
              {answer.mismatches.length === 0 ? (
                <p className="mt-auto text-xl font-semibold tracking-tight">
                  <Lamp state="applied">账实相符</Lamp>
                </p>
              ) : (
                <>
                  <p className="hint text-[13px]">「扣费与计费」不符时，已计费但未入账的调用也在其中，同时计入下方「已计费但未入账」。</p>
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
                      {answer.mismatches.map((m) => (
                        <tr key={`${m.tenant_id}-${m.check}`}>
                          <td className="font-medium">{m.tenant_name || m.tenant_id}</td>
                          <td>{checkLabel(m.check)}</td>
                          <td className="num money">{fmtMicroCny(m.expected_micro_cny)}</td>
                          <td className="num money text-danger">{fmtMicroCny(m.actual_micro_cny)}</td>
                          <td className="num">{m.seq ?? "—"}</td>
                        </tr>
                      ))}
                    </tbody>
                  </DataTable>
                </>
              )}
            </Card>

            <Card aria-label="已计费但未入账" className="mb-0 flex flex-col">
              <CardHeader title="已计费但未入账" />
              <p className="num text-muted-foreground">
                <strong className="mr-1 text-3xl font-semibold tracking-tight text-foreground tabular-nums">
                  {fmtInt(answer.charged_without_entry)}
                </strong>{" "}
                笔
              </p>
              <p className="hint mt-auto pt-3 text-[13px]">
                {filtered ? "仅统计所选租户。" : "统计全部租户。"}已计费的调用在账本里没有对应流水。
              </p>
            </Card>

            <Card aria-label="近 24 小时未计价" className="mb-0 flex flex-col">
              <CardHeader
                title="近 24 小时未计价"
                description={filtered ? "不随租户筛选变化，始终统计全部租户。" : "统计全部租户。"}
              />
              <div className="bl-box">
                <DataTable label="未计价的原因">
                  <tbody>
                    <tr>
                      <th scope="row" className="!font-medium !text-foreground">
                        合计
                      </th>
                      <td className="num font-semibold">{fmtInt(unpriced.total)}</td>
                    </tr>
                    {Object.entries(unpriced.by_code).map(([code, n]) => (
                      <tr key={code}>
                        <th scope="row">{reasonLabel(code)}</th>
                        <td className="num">{fmtInt(n)}</td>
                      </tr>
                    ))}
                  </tbody>
                </DataTable>
              </div>
            </Card>
          </div>

          <Card aria-label="疑似用量丢失" className="mb-0">
            <CardHeader
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
                    <td className="num font-semibold">{fmtInt(loss.dropped_total)}</td>
                  </tr>
                  <tr>
                    <th scope="row">未确认送达（可能已入账）</th>
                    <td className="num font-semibold">{fmtInt(loss.unconfirmed_total)}</td>
                  </tr>
                </tbody>
              </DataTable>
            </div>
            {loss.instances.length === 0 ? (
              <p className="hint mt-4 text-[13px]">近 24 小时内没有网关实例上报过心跳。</p>
            ) : (
              <DataTable label="各实例的丢弃明细">
                <thead>
                  <tr>
                    <th>凭据</th>
                    <th>实例</th>
                    <th>主机</th>
                    <th>最近心跳（UTC）</th>
                    <th>丢弃明细</th>
                  </tr>
                </thead>
                <tbody>
                  {loss.instances.map((i) => {
                    const reasons = Object.entries(i.drops).filter(([, n]) => n > 0);
                    return (
                      <tr key={i.dp_id + i.instance_id}>
                        <td className="font-medium">{i.credential_name}</td>
                        <td className="tabular bl-wrap font-mono text-[13px]">{i.instance_id}</td>
                        <td className="bl-wrap">{i.hostname}</td>
                        <td className="tabular text-text-2">{fmtUtc(i.last_seen_at)}</td>
                        <td>
                          {reasons.length === 0 ? (
                            <span className="hint">无</span>
                          ) : (
                            reasons.map(([reason, n]) => (
                              <span key={reason} className="cell-note">
                                {dropLabel(reason)}：{fmtInt(n)}
                              </span>
                            ))
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </DataTable>
            )}
          </Card>
        </>
      )}
    </>
  );
}
