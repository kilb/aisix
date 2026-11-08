import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { ApiError, reconciliation, tenants, type Reconciliation as Answer, type Tenant } from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { fmtMicroUsd, fmtUtc } from "../../lib/money";
import { checkLabel, dropLabel, fmtCount, isPositive } from "../../lib/reconcile";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Field } from "../../ui/Field";
import { Lamp } from "../../ui/Lamp";

/** What the operator reads when the check fails, by status; any other failure shows the Dashboard's usual text. */
function failureText(e: unknown): string {
  if (e instanceof ApiError && e.status === 404) return "找不到所选租户，可能已被删除。请重新选择租户。";
  if (e instanceof ApiError && e.status === 503) return "对账超时，控制面暂时无法完成这次检查。请稍后点「刷新」重试。";
  return errorText(e);
}

/** Micro-dollars as money; an amount past 2^53 arrives as text and is read as the nearest number. */
const money = (n: number | string) => fmtMicroUsd(Number(n));

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
  const latest = useRef(0);

  useEffect(() => {
    tenants.list().then(setTenantList, () => undefined);
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
      setError(failureText(e));
    } finally {
      if (mine === latest.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(tenantId);
  }, [load, tenantId]);

  const filtered = tenantId !== "";
  const loss = answer?.usage_loss;
  const unpriced = answer?.unpriced_last_24h;

  return (
    <div className="panel">
      <h2>对账</h2>
      <p className="hint">核对账本与扣费、调整、充值是否一致，并列出网关报告丢弃的用量。这里只读，不会改动任何数据。</p>
      <div className="r">
        <Field label="租户">
          <select value={tenantId} disabled={loading} onChange={(e) => setTenantId(e.target.value)}>
            <option value="">全部</option>
            {tenantList.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </select>
        </Field>
        <Button type="button" icon="refresh" busy={loading} busyLabel="刷新" onClick={() => void load(tenantId)}>
          刷新
        </Button>
      </div>

      {error && (
        <div className="note crit" role="alert">
          {error}
        </div>
      )}
      {answer === null && !error && <Loading />}

      {answer && loss && unpriced && (
        <>
          <p className="hint">检查时间：{fmtUtc(answer.checked_at)}</p>

          <section aria-label="账实核对" className="bl-region">
            <h3>账实核对</h3>
            {answer.mismatches.length === 0 ? (
              <Lamp state="applied">账实相符</Lamp>
            ) : (
              <>
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
                        <td>{m.tenant_name || m.tenant_id}</td>
                        <td>{checkLabel(m.check)}</td>
                        <td className="num money">{money(m.expected_micro_usd)}</td>
                        <td className="num money">{money(m.actual_micro_usd)}</td>
                        <td className="num">{m.seq ?? "—"}</td>
                      </tr>
                    ))}
                  </tbody>
                </DataTable>
                <p className="hint">「扣费与计费」不符时，已计费但未入账的调用也在其中，同时计入下方「已计费但未入账」。</p>
              </>
            )}
          </section>

          <section aria-label="已计费但未入账" className="bl-region">
            <h3>已计费但未入账</h3>
            <p className="num">
              <strong>{fmtCount(answer.charged_without_entry)}</strong> 笔
            </p>
            <p className="hint">{filtered ? "仅统计所选租户。" : "统计全部租户。"}已计费的调用在账本里没有对应流水。</p>
          </section>

          <section aria-label="近 24 小时未计价" className="bl-region">
            <h3>近 24 小时未计价</h3>
            <p className="hint">{filtered ? "不随租户筛选变化，始终统计全部租户。" : "统计全部租户。"}</p>
            <DataTable label="未计价的原因">
              <tbody>
                <tr>
                  <th scope="row">合计</th>
                  <td className="num">{fmtCount(unpriced.total)}</td>
                </tr>
                {Object.entries(unpriced.by_code).map(([code, n]) => (
                  <tr key={code}>
                    <th scope="row">{code}</th>
                    <td className="num">{fmtCount(n)}</td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
          </section>

          <section aria-label="疑似用量丢失" className="bl-region">
            <h3>疑似用量丢失</h3>
            <p className="hint">
              {filtered ? "不随租户筛选变化，始终统计全部网关。" : "统计全部网关。"}
              取最近 24 小时内有心跳的实例，自实例启动起，按其最后一次心跳报告的数量；心跳之后的丢弃不在其中，所以实际可能更多。
            </p>
            <DataTable label="丢弃合计">
              <tbody>
                <tr>
                  <th scope="row">已丢弃</th>
                  <td className="num">{fmtCount(loss.dropped_total)}</td>
                </tr>
                <tr>
                  <th scope="row">未确认送达（可能已入账）</th>
                  <td className="num">{fmtCount(loss.unconfirmed_total)}</td>
                </tr>
              </tbody>
            </DataTable>
            {loss.instances.length === 0 ? (
              <p className="hint">近 24 小时内没有网关实例上报过心跳。</p>
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
                    const reasons = Object.entries(i.drops).filter(([, n]) => isPositive(n));
                    return (
                      <tr key={i.dp_id + i.instance_id}>
                        <td>{i.credential_name}</td>
                        <td className="tabular">{i.instance_id}</td>
                        <td>{i.hostname}</td>
                        <td className="tabular">{fmtUtc(i.last_seen_at)}</td>
                        <td>
                          {reasons.length === 0 ? (
                            <span className="hint">无</span>
                          ) : (
                            reasons.map(([reason, n]) => (
                              <span key={reason} className="cell-note">
                                {dropLabel(reason)}：{fmtCount(n)}
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
          </section>
        </>
      )}
    </div>
  );
}
