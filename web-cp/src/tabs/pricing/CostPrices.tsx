import { useCallback, useEffect, useState } from "react";
import type { TabProps } from "../../App";
import { costRates, type CostAccount, type CostRatePlan } from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Card, CardHeader, PageHeader } from "../../components/page";
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

  const open = rows?.find((r) => r.account.id === selected)?.account ?? null;
  if (open) {
    return <CostAccountDetail account={open} canEdit={canEdit} onBack={() => setSelected(null)} onChanged={load} />;
  }

  return (
    <>
      <PageHeader
        title="成本价"
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
      <Card className="mb-0">
        <CardHeader className="mb-0" title={<>上游账号{rows && <Count n={rows.length} />}</>} />
        {rows === null ? (
          !error && <Loading />
        ) : rows.length === 0 ? (
          <EmptyState text="还没有上游账号。先在「上游账号」中添加。" />
        ) : (
          <DataTable label="上游账号成本价">
            <thead>
              <tr>
                <th>上游账号</th>
                <th>提供方</th>
                <th className="num">默认成本换算率</th>
              </tr>
            </thead>
            <tbody>
              {rows.map(({ account, default: plan }) => {
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
        )}
      </Card>
    </>
  );
}
