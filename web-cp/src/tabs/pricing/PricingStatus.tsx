import { useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { pricingStatus, type PricingStatus as Status } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Lamp } from "../../ui/Lamp";
import { fmtUtc } from "../../lib/money";
import { handChargesFilter, statusView } from "../../lib/pricingStatus";
import { Card, CardHeader, PageHeader, Stat, StatGrid } from "../../components/page";

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
  return (
    <>
      <PageHeader
        title="计价状态"
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
      {!status && !failure && <Loading />}
      {status && view && (
        <>
          <StatGrid className="xl:grid-cols-3">
            <Stat
              label="价格版本"
              value={<code className="font-mono">{status.pricing_version ?? "未知"}</code>}
            />
            <Stat label="不可用的价格版本" value={fmtInt(view.invalid.length)} />
            <Stat label="最近 24 小时未计价" value={fmtInt(view.unpricedTotal)} />
          </StatGrid>

          <Card>
            <CardHeader className={view.unpricedTotal === 0 ? undefined : "mb-0"} title="最近 24 小时未计价" />
            {view.unpricedTotal === 0 ? (
              <p className="status-line">
                <Lamp state="applied">最近 24 小时没有未计价的调用。</Lamp>
              </p>
            ) : (
              <DataTable label="未计价的调用" scroll>
                <thead>
                  <tr>
                    <th>原因</th>
                    <th className="num">次数</th>
                    <th>模型</th>
                    <th>档位 / 分组</th>
                    <th>最近一次（UTC）</th>
                    <th>处理</th>
                    <th className="right">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {view.details.map((d) => (
                    <tr key={d.key}>
                      <td className="font-medium">{d.label}</td>
                      <td className="num">{fmtInt(d.count)}</td>
                      <td>{d.model}</td>
                      <td>{d.scope}</td>
                      <td className="tabular">{fmtUtc(d.latestAt)}</td>
                      <td className="max-w-[28rem] whitespace-normal text-[13px] text-text-2">{d.remedy}</td>
                      <td className="right">
                        <Button
                          type="button"
                          size="sm"
                          onClick={() => {
                            handChargesFilter(d.filter);
                            onGoto("pricing", "charges");
                          }}
                        >
                          查看调用
                        </Button>
                      </td>
                    </tr>
                  ))}
                  {view.unlisted.map((u) => (
                    <tr key={u.key}>
                      <td className="font-medium">{u.label}</td>
                      <td className="num">{fmtInt(u.count)}</td>
                      <td colSpan={4}>其余 {fmtInt(u.count)} 次，未逐一列出</td>
                      <td className="right">
                        <Button
                          type="button"
                          size="sm"
                          onClick={() => {
                            handChargesFilter(u.filter);
                            onGoto("pricing", "charges");
                          }}
                        >
                          查看调用
                        </Button>
                      </td>
                    </tr>
                  ))}
                  <tr className="bg-muted/50 font-medium">
                    <th scope="row" className="!static !h-12 !border-b-0 !bg-transparent !text-sm !font-medium !text-foreground">
                      合计
                    </th>
                    <td className="num">{fmtInt(view.unpricedTotal)}</td>
                    <td colSpan={5} />
                  </tr>
                </tbody>
              </DataTable>
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
            <CardHeader className={view.invalid.length === 0 ? undefined : "mb-0"} title="不可用的价格版本" />
            {view.invalid.length === 0 ? (
              <p className="status-line">
                <Lamp state="applied">没有不可用的价格版本。</Lamp>
              </p>
            ) : (
              <DataTable label="不可用的价格版本">
                <thead>
                  <tr>
                    <th>表</th>
                    <th>版本</th>
                    <th>原因</th>
                  </tr>
                </thead>
                <tbody>
                  {view.invalid.map((r) => (
                    <tr key={r.key}>
                      <td>{r.table}</td>
                      <td className="tabular">{r.revision}</td>
                      <td className="whitespace-normal">{r.reason}</td>
                    </tr>
                  ))}
                </tbody>
              </DataTable>
            )}
          </Card>
        </>
      )}
    </>
  );
}
