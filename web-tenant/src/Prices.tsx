import { Fragment, useEffect, useId, useState } from "react";
import { ApiError } from "./lib/api";
import { messageFor } from "./lib/errors";
import { fmtCnyPerMillion, perThousandOfPerMillion } from "./lib/money";
import { currentPrices, dimensionLabels, discountPercent, type Dimension, type PriceQuoteList, type PriceRate, type QuotedModel, type TierQuote } from "./lib/priceQuote";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { RouteNav } from "./ui/RouteNav";
import { Button } from "./ui/Button";
import { CopyButton } from "./ui/CopyButton";
import { Card, PageHeader } from "./components/page";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

export function Prices({ initialGroup }: { initialGroup: string | null }) {
  const [quote, setQuote] = useState<PriceQuoteList | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load() {
      const started = Date.now();
      try {
        const next = await currentPrices();
        if (disposed) return;
        setQuote(next);
        setErr(null);
        if (next.valid_until !== null) {
          // 用服务端两个时刻的间隔刷新，避免本机时钟偏差；过期后先撤下旧价。
          const expires = started + Date.parse(next.valid_until) - Date.parse(next.as_of);
          timer = setTimeout(() => { setQuote(null); void load(); },
            Math.max(0, Math.min(expires - Date.now(), 2_147_000_000)));
        }
      } catch (e) {
        if (!disposed) setErr(failure(e));
      }
    }
    void load();
    return () => { disposed = true; clearTimeout(timer); };
  }, []);

  const groups = quote?.groups ?? null;
  const chosen = groups === null ? null :
    (groups.find((g) => g.id === open) ?? groups.find((g) => g.id === initialGroup) ?? groups[0] ?? null);
  const header = <PageHeader title="价格" />;
  if (err !== null) return <>{header}<div className="note crit" role="alert">{err}</div></>;
  if (quote === null) return <>{header}<Card><Loading /></Card></>;
  if (chosen === null) return <>{header}<EmptyState text="运营尚未为本组织分配分组，暂时没有可查看的价格。" /></>;

  return (
    <>
      {header}
      <Card>
        <div className="-mx-6 -mt-2 border-b border-border px-4 max-[719px]:-mx-4 max-[719px]:px-2">
          <RouteNav label="分组" look="underline" items={quote.groups.map((g) => ({ id: g.id, label: g.display_name }))}
            current={chosen.id} onSelect={setOpen} />
        </div>
        <section id={`tab-${chosen.id}`} role="tabpanel" aria-labelledby={`stop-${chosen.id}`} className="bleed">
          <div className="flex flex-wrap items-baseline justify-between gap-x-6 gap-y-1">
            {chosen.description !== "" && <p className="text-[14px] text-text-2">{chosen.description}</p>}
            <p className="hint">价格以人民币计，按实际用量计费</p>
          </div>
          <p className="text-sm">当前组织价格，已含组织优惠
            {quote.discount_bps !== null ? ` ${discountPercent(quote.discount_bps)}` : "（暂时无法读取）"}</p>
          <p className="hint">摘要为标准档基础价，单位为元/百万 token；展开可查看全部价格。当前价格不改变历史账单。</p>
          {chosen.models.length === 0 ? <EmptyState text="该分组暂时没有可计价的模型。" mark="route" /> :
            <PriceTable key={chosen.id} models={chosen.models} />}
        </section>
      </Card>
    </>
  );
}

function RateValue({ rate }: { rate: PriceRate | undefined }) {
  if (rate?.price_cny_per_million == null) return <span className="hint">未提供价格</span>;
  const amount = rate.unit === "request" ? perThousandOfPerMillion(rate.price_cny_per_million) : rate.price_cny_per_million;
  return <>{fmtCnyPerMillion(amount)}{rate.fallback_from !== undefined &&
    <span className="hint cell-note">按{dimensionLabels[rate.fallback_from]}价计费</span>}</>;
}

function RateTable({ rates }: { rates: PriceRate[] }) {
  return (
    <DataTable label="完整模型单价" compact>
      <thead><tr><th>计价维度</th><th className="num">组织单价</th><th>单位</th></tr></thead>
      <tbody>{rates.map((rate) => <tr key={rate.dimension}>
        <td>{dimensionLabels[rate.dimension]}</td><td className="num"><RateValue rate={rate} /></td>
        <td>{rate.unit === "request" ? "元/千次" : "元/百万 token"}</td>
      </tr>)}</tbody>
    </DataTable>
  );
}

function TierPrices({ name, tier }: { name: string; tier: TierQuote }) {
  const first = tier.long_context[0];
  return <section className="space-y-3">
    <h4 className="font-medium">{name === "default" ? "标准" : name}</h4>
    <p className="hint">{first ? `基础价：输入 ≤ ${first.above_input_tokens.toLocaleString("en-US")} token` : "基础价"}</p>
    <RateTable rates={tier.base} />
    {tier.long_context.map((band, index) => {
      const next = tier.long_context[index + 1];
      return <div className="space-y-2" key={band.above_input_tokens}>
        <p className="hint">输入 &gt; {band.above_input_tokens.toLocaleString("en-US")} token{next && `，且 ≤ ${next.above_input_tokens.toLocaleString("en-US")} token`}，整次请求使用本段价格</p>
        <RateTable rates={band.rates} />
      </div>;
    })}
  </section>;
}

function ModelRow({ model }: { model: QuotedModel }) {
  const [expanded, setExpanded] = useState(false);
  const [copyError, setCopyError] = useState<string | null>(null);
  const detailsId = useId();
  const base = model.prices_by_tier.default?.base ?? [];
  const rate = (dimension: Dimension) => base.find((r) => r.dimension === dimension);
  const oneHour = rate("cache_write_1h");
  return <Fragment>
    <tr>
      <td>
        <span className="block max-w-md break-all select-text" data-model-name>{model.name}</span>
        <div className="mt-2 flex flex-wrap gap-2">
          <CopyButton text={async () => { setCopyError(null); return model.name; }} label="复制模型名" size="sm"
            onFailed={(e) => setCopyError(`复制失败：${failure(e)}。请手动选择上方模型名。`)} />
          <Button size="sm" kind="quiet" aria-expanded={expanded} aria-controls={detailsId} onClick={() => setExpanded(!expanded)}>
            {expanded ? "收起价格" : "查看全部价格"}
          </Button>
        </div>
        {copyError !== null && <p className="field-problem" role="alert">{copyError}</p>}
      </td>
      <td className="num"><RateValue rate={rate("input")} /></td>
      <td className="num"><RateValue rate={rate("output")} /></td>
      <td className="num"><RateValue rate={rate("cache_read")} /></td>
      <td className="num"><RateValue rate={rate("cache_write")} />
        {oneHour?.fallback_from === undefined && oneHour?.price_cny_per_million != null &&
          <span className="hint cell-note">1 小时缓存写 <RateValue rate={oneHour} /></span>}
      </td>
    </tr>
    {expanded && <tr><td colSpan={5} id={detailsId}>
      <div className="space-y-6 py-4">
        <p className="hint">回退价格仅说明计费规则，不代表模型支持该模态。长上下文按包含缓存、音频和图像的整个输入 token 数选择价格段。</p>
        {Object.entries(model.prices_by_tier).sort(([a], [b]) => a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b))
          .map(([name, tier]) => <TierPrices key={name} name={name} tier={tier} />)}
      </div>
    </td></tr>}
  </Fragment>;
}

function PriceTable({ models }: { models: QuotedModel[] }) {
  return <DataTable label="模型单价">
    <thead><tr><th>模型</th><th className="num">输入</th><th className="num">输出</th><th className="num">缓存读</th><th className="num">缓存写</th></tr></thead>
    <tbody>{models.map((model) => <ModelRow key={model.model_id} model={model} />)}</tbody>
  </DataTable>;
}
