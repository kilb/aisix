import { cn } from "../../lib/cn";
import { cardDetails, matrixGroups } from "./cardDetails";
import "./PriceCardDetails.css";

/** 同一计量项纵向列出所有上下文区间，不以不断增加的表格列挤压金额。 */
export function PriceCardDetails({ card, label = "完整价格卡", compact = false }: { card: unknown; label?: string; compact?: boolean }) {
  const view = cardDetails(card);
  if (view.problem) {
    return <div className="price-card-invalid" role="group" aria-label={label}>
      <p>{view.problem}</p>
      <pre>{JSON.stringify(card, null, 2)}</pre>
    </div>;
  }
  if (view.tiers.length === 0) return <p className="price-card-empty">未提供价格卡</p>;
  return (
    <div className={cn("price-card-details", compact && "price-card-details-compact")} role="group" aria-label={label}>
      {view.tiers.map((tier) => (
        <section key={tier.key} className="price-card-tier" aria-label={`${tier.label}档价格`}>
          <div className="price-card-tier-heading">
            <h4>{tier.label}</h4>
            <code>{tier.key}</code>
          </div>
          <div className="price-card-band">
            {tier.bands[0]?.rates.some((rate) => ["input", "output", "web_search"].includes(rate.key)) && <p className="price-card-headline">基础价 · {tier.bands[0].rates.filter((rate) => ["input", "output", "web_search"].includes(rate.key)).map((rate) => `${rate.label} ${rate.amount}${rate.key === "web_search" ? " / 千次" : ""}`).join(" · ")}</p>}
            <div className="price-card-table-wrap">
              <table className="price-card-table" aria-label={`${tier.label}档 · 全部计量项与长上下文 · 美元价格`}>
                <colgroup><col className="price-card-dimension-col" /><col className="price-card-amount-col" /></colgroup>
                <thead><tr><th scope="col">计量项 / 单位</th><th scope="col">上下文区间与单价</th></tr></thead>
                {matrixGroups(tier).map((group) => <tbody key={group.label}>
                  <tr><th className="price-card-group-heading" scope="rowgroup" colSpan={2}>{group.label}</th></tr>
                  {group.rows.map((rate) => <tr key={rate.key}>
                    <th scope="row">{rate.label}<span className="price-card-unit">美元 / {rate.unit.replace(/^每/, "")}</span></th>
                    <td className="price-card-amount"><dl className="price-card-contexts">{tier.bands.map((band, index) => <div key={band.key}>
                      <dt>{band.label}</dt>
                      <dd>{rate.amounts[index] ?? <span className="price-card-missing">未提供价格</span>}</dd>
                    </div>)}</dl></td>
                  </tr>)}
                </tbody>)}
              </table>
            </div>
          </div>
        </section>
      ))}
    </div>
  );
}
