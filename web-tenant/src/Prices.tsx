import { Fragment, useEffect, useId, useState } from "react";
import "./Prices.css";
import { ApiError } from "./lib/api";
import { messageFor } from "./lib/errors";
import { fmtUtc } from "./lib/money";
import { currentPrices, discountPercent, type Dimension, type PriceQuoteList, type QuotedModel } from "./lib/priceQuote";
import { PriceDetails, PriceRateValue } from "./PriceDetails";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { RouteNav } from "./ui/RouteNav";
import { Button } from "./ui/Button";
import { CopyButton } from "./ui/CopyButton";
import { cn } from "./lib/cn";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "./components/page";

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
  const header = <PageHeader title="价格" eyebrow="PRICING · 组织现价" dataLead="查看当前组织优惠与分组价格；所有单价按实际用量计费，历史账单保留调用当时的费率。" />;
  if (err !== null) return <>{header}<Card><CardHeader number={1} title="当前价格" /><div role="alert"><CardMessage>{err}</CardMessage></div></Card></>;
  if (quote === null) return <>{header}<BentoGrid>
    <Card className={span.d8}><CardHeader number={1} title="当前组织优惠" /><MetricSkeleton className="h-24" /><p className="text-body text-muted">正在读取组织现价与报价时刻…</p></Card>
    <Card tone="inverse" className={cn(span.d4, "tablet:col-span-12")}><CardHeader number={2} title="当前分组" /><MetricSkeleton /></Card>
    <Card className={span.d12}><CardHeader number={3} title="模型价格目录" /><Loading /></Card>
  </BentoGrid></>;
  if (chosen === null) return <>{header}<Card><CardHeader number={1} title="当前价格" /><EmptyState text="运营尚未为本组织分配分组，暂时没有可查看的价格。" /></Card></>;

  return (
    <>
      {header}
      <BentoGrid>
        <Card className={span.d8}>
          <CardHeader number={1} title="当前组织优惠" />
          <Metric size="hero">{quote.discount_bps === null ? "未知" : discountPercent(quote.discount_bps)}</Metric>
          <p className="text-stat text-ink-soft">当前组织价格，已含组织优惠{quote.discount_bps === null ? "（暂时无法读取）" : ""}</p>
          <p className="text-body text-ink-soft">摘要为标准档基础价，单位为元/百万 token；展开可查看全部价格。当前价格不改变历史账单。</p>
          <CardFooter className="grid gap-2 border-t border-ink/7 pt-4 text-meta font-mono text-muted">
            <p>报价时刻 {fmtUtc(quote.as_of)}</p>
            <p>{quote.valid_until === null ? "当前报价未设置失效时刻" : `有效至 ${fmtUtc(quote.valid_until)}`}</p>
          </CardFooter>
        </Card>
        <Card tone="inverse" className={cn(span.d4, "tablet:col-span-12")}>
          <CardHeader number={2} title="当前分组" />
          <p className="break-words text-stat">{chosen.display_name}</p>
          <Metric size="display">{chosen.models.length.toLocaleString("en-US")}</Metric>
          <p className="text-caption text-on-inverse-muted">个可计价模型</p>
          <CardFooter className="border-t border-ink/12 pt-4 text-caption text-on-inverse-muted">
            组织已分配 {quote.groups.length.toLocaleString("en-US")} 个分组；下方可切换查看。
          </CardFooter>
        </Card>
        <Card className={span.d12}>
          <CardHeader number={3} title="模型价格目录" />
          <RouteNav label="分组" look="underline" items={quote.groups.map((g) => ({ id: g.id, label: g.display_name }))}
            current={chosen.id} onSelect={setOpen} />
          <section id={`tab-${chosen.id}`} role="tabpanel" aria-labelledby={`stop-${chosen.id}`} className="min-w-0 space-y-4">
            {chosen.description !== "" && <p className="text-body text-ink-soft">{chosen.description}</p>}
            <p className="text-caption text-muted">价格以人民币计，按实际用量计费</p>
            {chosen.models.length === 0 ? <EmptyState text="该分组暂时没有可计价的模型。" mark="route" /> :
              <PriceTable key={chosen.id} models={chosen.models} />}
          </section>
        </Card>
      </BentoGrid>
    </>
  );
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
      <td className="num"><PriceRateValue rate={rate("input")} /></td>
      <td className="num"><PriceRateValue rate={rate("output")} /></td>
      <td className="num"><PriceRateValue rate={rate("cache_read")} /></td>
      <td className="num"><PriceRateValue rate={rate("cache_write")} />
        {oneHour?.fallback_from === undefined && oneHour?.price_cny_per_million != null &&
          <span className="hint cell-note">1 小时缓存写 <PriceRateValue rate={oneHour} /></span>}
      </td>
    </tr>
    {expanded && <tr><td colSpan={5} id={detailsId} className="price-detail-cell">
      <PriceDetails tiers={model.prices_by_tier} />
    </td></tr>}
  </Fragment>;
}

function PriceTable({ models }: { models: QuotedModel[] }) {
  return <div className="price-catalog"><DataTable label="模型单价">
    <thead><tr><th>模型</th><th className="num">输入</th><th className="num">输出</th><th className="num">缓存读</th><th className="num">缓存写</th></tr></thead>
    <tbody>{models.map((model) => <ModelRow key={model.model_id} model={model} />)}</tbody>
  </DataTable></div>;
}
