import { fmtMicroCny } from "../lib/money";
import type { ProfitSummary } from "../lib/totals";
import { DataTable } from "../ui/DataTable";
import { Card, CardHeader, CardMessage } from "./page";

export function ProfitAccounts({ accounts }: { accounts: ProfitSummary["accounts"] }) {
  return <Card className="totals-accounts" padding="table">
    <CardHeader title="上游账号汇算" description="按原调用归属汇算，按完整利润从高到低排列。" />
    {accounts === "unsupported" ? <CardMessage>请升级控制面以查看上游账号汇算。</CardMessage> :
      accounts === "catching-up" ? <CardMessage>上游账号汇算正在同步历史数据，请稍后刷新。</CardMessage> :
      accounts.length === 0 ? <CardMessage>所选时间范围没有归属上游账号的记录。</CardMessage> :
        <DataTable label="上游账号汇算" compact>
          <thead><tr>
            <th scope="col">上游账号</th>
            <th scope="col" className="num">调用收入</th>
            <th scope="col" className="num">成本</th>
            <th scope="col" className="num">利润</th>
            <th scope="col" className="num">利润率</th>
          </tr></thead>
          <tbody>{accounts.map(account => {
            const complete = account.profitMicroCny !== null;
            const costKnown = complete || account.rowsWithCost > 0n;
            const profit = account.profitMicroCny ?? account.knownProfitMicroCny;
            const margin = account.marginPct ?? account.knownMarginPct;
            return <tr key={account.id}>
              <td>
                <span>{account.displayName || "历史账号"}</span>
                <code className="cell-note break-all">{account.id}</code>
              </td>
              <td className="num">{fmtMicroCny(account.revenueMicroCny)}</td>
              <td className="num">
                {costKnown ? fmtMicroCny(account.knownCostMicroCny) : "—"}
                {!complete && <span className="cell-note">{costKnown ? "已知成本小计" : "成本未知"}</span>}
              </td>
              <td className="num">
                {profit === null ? "—" : fmtMicroCny(profit)}
                {!complete && profit !== null && <span className="cell-note">已知成本部分</span>}
              </td>
              <td className="num">
                {margin === null ? "—" : `${margin}%`}
                {!complete && margin !== null && <span className="cell-note">已知成本部分</span>}
              </td>
            </tr>;
          })}</tbody>
        </DataTable>}
  </Card>;
}
