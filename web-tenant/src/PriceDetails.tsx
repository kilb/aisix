import { useId } from "react";
import { fmtCnyPerMillion, perThousandOfPerMillion } from "./lib/money";
import { dimensionLabels, type Dimension, type PriceRate, type TierQuote } from "./lib/priceQuote";

const RATE_GROUPS: { label: string; dimensions: readonly Dimension[] }[] = [
  { label: "文本与推理", dimensions: ["input", "output", "reasoning"] },
  { label: "缓存", dimensions: ["cache_read", "cache_write", "cache_write_1h"] },
  { label: "音频", dimensions: ["audio_input", "audio_output"] },
  { label: "图像", dimensions: ["image_input", "image_output"] },
  { label: "联网", dimensions: ["web_search"] },
];

export function PriceRateValue({ rate }: { rate: PriceRate | undefined }) {
  if (rate?.price_cny_per_million == null) return <span className="hint price-rate-unknown">未提供价格</span>;
  const amount = rate.unit === "request" ? perThousandOfPerMillion(rate.price_cny_per_million) : rate.price_cny_per_million;
  return <>
    <span className="price-rate-amount">{fmtCnyPerMillion(amount)}</span>
    {rate.fallback_from !== undefined && <span className="hint cell-note price-rate-rule">按{dimensionLabels[rate.fallback_from]}价计费</span>}
  </>;
}

interface PriceColumn {
  label: string;
  above?: number;
  through?: number;
  rates: PriceRate[];
}

function PriceBandLabel({ column }: { column: PriceColumn }) {
  return <>
    <span className="price-band-name">{column.label}</span>
    <span className="price-band-range">
      {column.above === undefined
        ? column.through === undefined ? "全部输入长度" : `输入 ≤ ${column.through.toLocaleString("en-US")} token`
        : `输入 > ${column.above.toLocaleString("en-US")} token`}
    </span>
    {column.above !== undefined && column.through !== undefined && <span className="price-band-range">且 ≤ {column.through.toLocaleString("en-US")} token</span>}
  </>;
}

function TierPrices({ name, tier }: { name: string; tier: TierQuote }) {
  const heading = useId();
  const columns: PriceColumn[] = [
    { label: "基础价", through: tier.long_context[0]?.above_input_tokens, rates: tier.base },
    ...tier.long_context.map((band, index) => ({
      label: `长上下文 ${index + 1}`,
      above: band.above_input_tokens,
      through: tier.long_context[index + 1]?.above_input_tokens,
      rates: band.rates,
    })),
  ];
  const tierName = name === "default" ? "标准" : name;
  return <section className="price-tier" aria-labelledby={heading}>
    <header className="price-tier-header">
      <h4 id={heading} className="text-stat font-medium">{tierName}</h4>
      <p className="text-caption text-muted">组织实际单价（已含优惠）</p>
    </header>
    <div className="price-matrix-scroll" role="region" aria-label={`${tierName}档完整模型单价，可横向滚动`} tabIndex={0}>
      <table className="price-matrix" aria-label="完整模型单价" style={{ minWidth: `${20 + columns.length * 12}rem` }}>
        <colgroup>
          <col className="price-item-column" />
          <col className="price-unit-column" />
          {columns.map((_, index) => <col key={index} className="price-amount-column" />)}
        </colgroup>
        <thead><tr>
          <th scope="col">计费项目</th>
          <th scope="col">单位</th>
          {columns.map((column, index) => <th key={index} scope="col" className="price-band-heading"><PriceBandLabel column={column} /></th>)}
        </tr></thead>
        {RATE_GROUPS.map((group) => <tbody key={group.label}>
          <tr><th className="price-group-heading" scope="rowgroup" colSpan={columns.length + 2}>{group.label}</th></tr>
          {group.dimensions.map((dimension) => {
            const rates = columns.map((column) => column.rates.find((rate) => rate.dimension === dimension));
            // 报价协议中只有联网搜索按次，其余维度按 token；缺价不补零。
            const unit = rates.find((rate) => rate !== undefined)?.unit ?? (dimension === "web_search" ? "request" : "token");
            return <tr key={dimension}>
              <th scope="row" className="price-item">{dimensionLabels[dimension]}</th>
              <td className="price-unit">{unit === "request" ? "元/千次" : "元/百万 token"}</td>
              {rates.map((rate, index) => <td key={index} className="price-amount"><PriceRateValue rate={rate} /></td>)}
            </tr>;
          })}
        </tbody>)}
      </table>
    </div>
  </section>;
}

export function PriceDetails({ tiers }: { tiers: Record<string, TierQuote> }) {
  return <div className="price-details">
    <p className="text-caption text-muted">长上下文按包含缓存、音频和图像的整个输入 token 数选择价格列，整次请求使用本段价格。回退价格仅说明计费规则，不代表模型支持该模态。窄屏可在表内横向滚动查看价格。</p>
    {Object.entries(tiers).sort(([a], [b]) => a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b))
      .map(([name, tier]) => <TierPrices key={name} name={name} tier={tier} />)}
  </div>;
}
