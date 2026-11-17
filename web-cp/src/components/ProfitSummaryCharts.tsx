import { exactRatio, formatCount, formatRatio, normalizeExactValues } from "../lib/chartFormat";
import { fmtMicroCny } from "../lib/money";
import type { ProfitSummary } from "../lib/totals";

/** 图形只比较接口金额，不以收入减去已知成本推算完整利润。 */
export function ProfitComparison({ data }: { data: ProfitSummary }) {
  const complete = data.rowsWithoutCost === 0n && data.unpricedRows === 0n;
  const costKnown = data.rowsWithCost > 0n || complete;
  const amounts = [
    { label: data.unpricedRows > 0n ? "已计价收入小计" : "调用收入", value: data.revenueMicroCny, tone: "revenue" },
    { label: complete ? "总成本" : "已知成本小计", value: costKnown ? data.knownCostMicroCny : null, tone: "cost" },
    { label: "完整利润", value: data.profitMicroCny, tone: "profit" },
  ];
  const values = normalizeExactValues(amounts.map(item => item.value));
  const minimum = Math.min(0, ...values.filter((value): value is number => value !== null));
  const maximum = Math.max(0, ...values.filter((value): value is number => value !== null));
  const range = maximum - minimum || 1;
  const zero = -minimum / range * 100;
  return <figure className="totals-comparison" aria-label="收入、成本与完整利润金额对照">
    <dl>
      {amounts.map((item, index) => {
        const value = values[index] ?? null;
        const endpoint = value === null ? zero : (value - minimum) / range * 100;
        return <div key={item.tone} className="totals-comparison-row">
          <dt>{item.label}</dt>
          <dd>
            <div className="totals-comparison-track" data-unknown={value === null || undefined} aria-hidden="true">
              <span className="totals-comparison-zero" style={{ left: `${zero}%` }} />
              {value !== null ? <span className={`totals-comparison-bar totals-comparison-${item.tone}`} data-negative={value < 0 || undefined} data-zero={value === 0 || undefined} style={{ left: `${Math.min(zero, endpoint)}%`, width: `${Math.abs(endpoint - zero)}%` }} /> : <span className="totals-comparison-unknown">未知</span>}
            </div>
            <span className="totals-comparison-amount">{item.value === null ? "—" : fmtMicroCny(item.value)}</span>
          </dd>
        </div>;
      })}
    </dl>
    <figcaption>三项使用相同金额刻度，负值位于零轴左侧。未知金额不按零绘制。</figcaption>
  </figure>;
}

/** 覆盖率只统计已计费记录的成本完整度，不表示利润率或待计价记录的成本。 */
export function CostCoverage({ data }: { data: ProfitSummary }) {
  const total = data.rowsWithCost + data.rowsWithoutCost;
  const ratio = exactRatio(data.rowsWithCost, total);
  return <figure className="totals-coverage" aria-label="已计费记录的成本覆盖">
    <div className="totals-coverage-ring" aria-hidden="true">
      <svg viewBox="0 0 160 160">
        <circle cx="80" cy="80" r="64" fill="none" strokeWidth="14" className="totals-coverage-track" />
        {ratio !== null && <circle cx="80" cy="80" r="64" fill="none" strokeWidth="14" pathLength="100" strokeDasharray={`${ratio * 100} 100`} transform="rotate(-90 80 80)" className="totals-coverage-known" />}
      </svg>
      <div><strong>{formatRatio(ratio)}</strong><span>成本已知</span></div>
    </div>
    <figcaption>
      <span className="sr-only">成本已知覆盖率：{formatRatio(ratio)}。</span>
      <dl className="totals-coverage-legend">
        <div><dt><span className="totals-coverage-dot" />成本已知</dt><dd>{formatCount(data.rowsWithCost)} 笔</dd></div>
        <div><dt><span className="totals-coverage-dot" data-unknown />成本缺失</dt><dd>{formatCount(data.rowsWithoutCost)} 笔</dd></div>
      </dl>
      <p>{total === 0n ? "暂无已计费记录，成本覆盖率无定义。" : `共 ${formatCount(total)} 笔已计费记录；覆盖率按笔数统计，不按金额。`}</p>
      {data.unpricedRows > 0n && <p>另有 {formatCount(data.unpricedRows)} 笔待计价记录，未计入此覆盖率。</p>}
    </figcaption>
  </figure>;
}
