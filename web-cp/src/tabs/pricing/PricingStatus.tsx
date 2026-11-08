import { useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { pricingStatus, type PricingStatus as Status } from "../../lib/api";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Lamp } from "../../ui/Lamp";
import { statusView } from "../../lib/pricingStatus";

/** 计价状态: the pricing version, price revisions that cannot be used, and the last day's unpriced calls by reason. */
export function PricingStatus(_props: TabProps) {
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
    <div className="panel">
      <h2>计价状态</h2>
      <p className="hint">计价引擎的版本、不可用的价格版本，以及最近 24 小时没能计价的调用。</p>
      <div className="r">
        <Button type="button" size="sm" icon="refresh" busy={reading} busyLabel="读取中…" onClick={() => void read()}>
          刷新
        </Button>
      </div>
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      {!status && !failure && <Loading />}
      {status && view && (
        <>
          <p className="hint">
            价格版本 <code>{status.pricing_version ?? "未知"}</code>
          </p>

          <h3>不可用的价格版本</h3>
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
                    <td>{r.reason}</td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
          )}

          <h3>最近 24 小时未计价</h3>
          {view.unpricedTotal === 0 ? (
            <p className="status-line">
              <Lamp state="applied">最近 24 小时没有未计价的调用。</Lamp>
            </p>
          ) : (
            <DataTable label="未计价的调用">
              <thead>
                <tr>
                  <th>原因</th>
                  <th className="num">次数</th>
                </tr>
              </thead>
              <tbody>
                {view.unpriced.map((u) => (
                  <tr key={u.code}>
                    <td>{u.label}</td>
                    <td className="num">{fmtInt(u.count)}</td>
                  </tr>
                ))}
                <tr>
                  <th scope="row">合计</th>
                  <td className="num">{fmtInt(view.unpricedTotal)}</td>
                </tr>
              </tbody>
            </DataTable>
          )}
        </>
      )}
    </div>
  );
}
