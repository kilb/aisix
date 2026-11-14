import { ArrowUpRight, X } from "lucide-react";
import { Dialog } from "radix-ui";
import { useEffect, useMemo, useRef, useState } from "react";
import "./Prices.css";
import { ApiError } from "./lib/api";
import { messageFor } from "./lib/errors";
import { fmtBeijing } from "./lib/time";
import { currentPrices, discountPercent, type BaseQuotedModel, type Dimension, type PriceQuoteList } from "./lib/priceQuote";
import { modelPriceCatalog, type CatalogModel } from "./lib/priceCatalog";
import { useRecordTable } from "./lib/recordTable";
import { PriceDetails, PriceRateValue } from "./PriceDetails";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { RecordTablePager, RecordTableToolbar } from "./ui/RecordTable";
import { Button } from "./ui/Button";
import { CopyButton } from "./ui/CopyButton";
import { BentoGrid, Card, CardHeader, CardMessage, MetricSkeleton, PageHeader, span } from "./components/page";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

export function Prices({ initialGroup }: { initialGroup: string | null }) {
  const [quote, setQuote] = useState<PriceQuoteList | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [groupFilter, setGroupFilter] = useState({ linkedGroup: initialGroup, value: initialGroup ?? "" });
  // 同页新深链优先；本地选择只属于它发生时的链接参数。
  if (groupFilter.linkedGroup !== initialGroup) {
    setGroupFilter({ linkedGroup: initialGroup, value: initialGroup ?? "" });
  }

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

  const catalog = useMemo(() => modelPriceCatalog(quote?.groups ?? []), [quote]);
  const requestedGroup = groupFilter.linkedGroup === initialGroup ? groupFilter.value : initialGroup ?? "";
  const selectedGroup = quote?.groups.some((group) => group.id === requestedGroup) ? requestedGroup : "";
  const header = <PageHeader title="模型与价格" dataLead="查看美元基准价，或选择分组比较实际人民币单价与倍率。" />;
  if (err !== null) return <>{header}<Card><CardHeader title="当前价格" /><div role="alert"><CardMessage>{err}</CardMessage></div></Card></>;
  if (quote === null) return <>{header}<BentoGrid>
    <Card className={span.d8}><CardHeader title="当前组织优惠" /><MetricSkeleton className="h-24" /><p className="text-body text-muted">正在读取组织现价与报价时刻…</p></Card>
    <Card className={span.d4}><CardHeader title="可计价模型" /><MetricSkeleton /></Card>
    <Card className={span.d12}><CardHeader title="模型价格目录" /><Loading /></Card>
  </BentoGrid></>;
  if (quote.groups.length === 0) return <>{header}<Card><CardHeader title="当前价格" /><EmptyState text="运营尚未为本组织分配分组，暂时没有可查看的价格。" /></Card></>;

  return <>
    {header}
    <BentoGrid className="tenant-prices">
      <Card className={`${span.d12} price-summary price-summary-compact`}>
        <dl className="price-summary-facts">
          <div><dt>当前组织优惠</dt><dd>{quote.discount_bps === null ? "未知" : discountPercent(quote.discount_bps)}</dd><span>分组人民币报价已计入优惠</span></div>
          <div><dt>可计价模型</dt><dd>{catalog.length.toLocaleString("en-US")}</dd><span>{quote.groups.length.toLocaleString("en-US")} 个可用分组</span></div>
          <div className="price-summary-time"><dt>报价时刻</dt><dd>{fmtBeijing(quote.as_of)}</dd><span>{quote.valid_until === null ? "当前报价未设置失效时刻" : `有效至 ${fmtBeijing(quote.valid_until)}`}</span></div>
        </dl>
        <details className="price-quote-explainer"><summary>价格如何计算</summary><p>倍率 = 换算率 ×（1 − 组织优惠）。人民币单价按美元基准价乘以倍率计算。当前报价不改变历史账单；历史费用使用调用时的费率。</p></details>
      </Card>
      <Card className={span.d12}>
        <CardHeader title="模型价格目录" />
        <PriceCatalog quote={quote} groupId={selectedGroup} onGroupChange={(value) => setGroupFilter({ linkedGroup: initialGroup, value })} />
        <p className="price-estimate-note text-caption text-muted">已列出的单价按价卡确定；“未提供价格”的项目不按零计费。“含估算”表示调用的部分 token 用量由网关估算，与单价无关，具体调用会在用量页单独标注。</p>
      </Card>
    </BentoGrid>
  </>;
}

const tierName = (tier: string) => ({ default: "标准", standard: "Standard", fast: "Fast", priority: "Priority", batch: "Batch", flex: "Flex" } as Record<string, string>)[tier] ?? tier;

interface CatalogEntry extends CatalogModel {
  base?: BaseQuotedModel;
}

const searchModel = (model: CatalogEntry) => [model.name, ...model.groups.flatMap((group) => [group.display_name, group.description])];

function PriceScopes({ items, value, onChange, label }: {
  items: { id: string; display_name: string; multiplier?: string }[];
  value: string;
  onChange: (value: string) => void;
  label: string;
}) {
  return <div role="group" aria-label={label} className="price-scope-buttons">
    <button type="button" data-price-scope="" aria-pressed={value === ""} onClick={() => onChange("")}>Base USD</button>
    {items.map((item) => <button type="button" key={item.id} data-price-scope={item.id} aria-pressed={value === item.id} onClick={() => onChange(item.id)}>
      <span>{item.display_name}</span>{item.multiplier !== undefined && <strong>{item.multiplier}×</strong>}
    </button>)}
  </div>;
}

function PriceCatalog({ quote, groupId, onGroupChange }: { quote: PriceQuoteList; groupId: string; onGroupChange: (group: string) => void }) {
  const models = useMemo(() => {
    const catalog = modelPriceCatalog(quote.groups);
    const bases = new Map(quote.base_models?.map((model) => [model.model_id, model]));
    if (groupId !== "") return catalog.filter((model) => model.groups.some((group) => group.id === groupId))
      .map((model) => ({ ...model, base: bases.get(model.model_id) }));
    const grouped = new Map(catalog.map((model) => [model.model_id, model.groups]));
    return (quote.base_models ?? []).map((base) => ({ model_id: base.model_id, name: base.name, base, groups: grouped.get(base.model_id) ?? [] }))
      .sort((a, b) => a.name.localeCompare(b.name) || a.model_id.localeCompare(b.model_id));
  }, [quote.groups, quote.base_models, groupId]);
  const table = useRecordTable({ items: models, searchText: searchModel, resetKey: groupId });
  const [tier, setTier] = useState("default");
  const tiers = [...new Set(models.flatMap((model) => Object.keys(groupId === ""
    ? model.base?.prices_by_tier ?? {} : model.groups.find((group) => group.id === groupId)?.quote.prices_by_tier ?? {})))].sort((a, b) => (a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b)));
  // 保留用户偏好；分组不支持时，选择框、行标题和报价一起回到标准档。
  const effectiveTier = tiers.includes(tier) ? tier : "default";
  const group = quote.groups.find((item) => item.id === groupId);
  const currency = groupId === "" ? "USD" : "CNY";
  const unavailable = groupId === "" && quote.base_models === undefined;
  return <>
    <PriceScopes items={quote.groups} value={groupId} onChange={onGroupChange} label="价格分组" />
    <RecordTableToolbar search={{ value: table.query, onChange: table.setQuery, label: "搜索模型价格", placeholder: "模型名、分组或说明" }}>
      <label className="price-group-filter">服务档位<select aria-label="服务档位" value={effectiveTier} onChange={(event) => setTier(event.target.value)}><option value="default">标准</option>{tiers.filter((value) => value !== "default").map((value) => <option key={value} value={value}>{tierName(value)}</option>)}</select></label>
    </RecordTableToolbar>
    <div className="price-scope-note"><p>{group?.description || (currency === "USD" ? "美元基准价，不含分组换算与组织优惠。" : "分组人民币单价，已含组织优惠。")}</p><p>{currency === "USD" ? "美元" : "元"}/百万 token · 点击模型查看完整价格</p></div>
    {unavailable ? <CardMessage>暂无法读取美元基准价，请稍后刷新或选择分组查看人民币报价。</CardMessage>
      : table.rows.length === 0 ? <EmptyState text={table.query ? "没有符合筛选条件的模型价格。" : "当前范围暂时没有可计价的模型。"} mark="route" /> : <div className="price-catalog" role="region" aria-label="模型价格"><DataTable label="模型单价">
        <colgroup><col className="price-summary-model" /><col className="price-summary-multiplier" />{Array.from({ length: 4 }, (_, index) => <col key={index} className="price-summary-amount" />)}</colgroup>
        <thead><tr><th>模型</th><th className="num">{currency === "USD" ? "价格基准" : "倍率"}</th><th className="num">输入</th><th className="num">输出</th><th className="num">缓存读</th><th className="num">缓存写</th></tr></thead>
        {table.rows.map((model) => <ModelRow key={model.model_id} model={model} tier={effectiveTier} groupId={groupId} discountBps={quote.discount_bps} />)}
      </DataTable></div>}
    {!unavailable && <RecordTablePager page={table.page} pageSize={table.pageSize} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} total={table.filteredCount} label="模型价格分页" />}
  </>;
}

function ModelRow({ model, discountBps, tier, groupId }: { model: CatalogEntry; discountBps: number | null; tier: string; groupId: string }) {
  const [open, setOpen] = useState(false);
  const [detailGroupId, setDetailGroupId] = useState(groupId);
  const [copyError, setCopyError] = useState<string | null>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const group = model.groups.find((item) => item.id === groupId);
  const currency = groupId === "" ? "USD" : "CNY";
  const selectedTier = groupId === "" ? model.base?.prices_by_tier[tier] : group?.quote.prices_by_tier[tier];
  const rate = (dimension: Dimension) => selectedTier?.base.find((item) => item.dimension === dimension);
  const oneHour = rate("cache_write_1h");
  const oneHourValue = oneHour === undefined ? null : "price_usd_per_million" in oneHour ? oneHour.price_usd_per_million : oneHour.price_cny_per_million;
  const detailGroup = model.groups.find((item) => item.id === detailGroupId);
  const detailCurrency = detailGroupId === "" ? "USD" : "CNY";
  const detailTiers = detailGroupId === "" ? model.base?.prices_by_tier : detailGroup?.quote.prices_by_tier;
  return <tbody data-price-model={model.model_id}>
    <tr className="price-model-row" data-price-group={group?.id ?? "base"} data-group-name={group?.display_name ?? "Base USD"}>
      <th scope="row"><div className="price-model-bar">
        <button ref={trigger} type="button" className="price-model-link" aria-label={`查看 ${model.name} 的价格详情`} onClick={() => { setDetailGroupId(groupId); setOpen(true); }}>
          <span data-model-name>{model.name}</span><ArrowUpRight aria-hidden /><span className="price-model-tier">{tierName(tier)}{(selectedTier?.long_context.length ?? 0) > 0 ? " · 含长上下文价格" : ""}</span>
        </button>
        <CopyButton text={async () => { setCopyError(null); return model.name; }} label="复制模型名" size="sm" onFailed={(error) => setCopyError(`复制失败：${failure(error)}。请手动选择模型名。`)} />
      </div>{copyError !== null && <p className="field-problem" role="alert">{copyError}</p>}</th>
      <td className="num price-multiplier" data-label={currency === "USD" ? "价格基准" : "倍率"} title={group === undefined ? "美元基准价，不含分组换算与组织优惠" : `${group.quote.cny_per_usd} ×（1 − ${discountBps === null ? "未知" : discountPercent(discountBps)}）`}>
        <span className="price-multiplier-value">{group === undefined ? "USD" : `${group.quote.effective_multiplier}×`}</span>{group !== undefined && <span className="cell-note">已含组织优惠</span>}
      </td>
      <td className="num" data-label="输入"><PriceRateValue rate={rate("input")} currency={currency} /></td>
      <td className="num" data-label="输出"><PriceRateValue rate={rate("output")} currency={currency} /></td>
      <td className="num" data-label="缓存读"><PriceRateValue rate={rate("cache_read")} currency={currency} /></td>
      <td className="num" data-label="缓存写"><PriceRateValue rate={rate("cache_write")} currency={currency} />
        {oneHour?.fallback_from === undefined && oneHourValue != null && <span className="hint cell-note">1 小时缓存写 <PriceRateValue rate={oneHour} currency={currency} /></span>}
      </td>
    </tr>
    <Dialog.Root open={open} onOpenChange={setOpen}>
      <Dialog.Portal>
        <Dialog.Overlay className="price-sheet-overlay" />
        <Dialog.Content className="price-sheet" onCloseAutoFocus={(event) => { event.preventDefault(); trigger.current?.focus(); }}>
          <header className="price-sheet-header">
            <div><Dialog.Title className="price-sheet-title">{model.name}</Dialog.Title><Dialog.Description className="price-sheet-description">完整价格与分组倍率 · {detailCurrency === "USD" ? "美元基准价" : "人民币实际单价"}</Dialog.Description></div>
            <Dialog.Close asChild><Button kind="quiet" iconOnly aria-label="关闭价格详情" title="关闭价格详情"><X aria-hidden /></Button></Dialog.Close>
          </header>
          <div className="price-sheet-body">
            <div className="price-sheet-scopes">
              <PriceScopes items={model.groups.map((item) => ({ id: item.id, display_name: item.display_name, multiplier: item.quote.effective_multiplier }))} value={detailGroupId} onChange={setDetailGroupId} label="详情价格分组" />
              <p>分组倍率 = 换算率 ×（1 − 组织优惠）。当前组织优惠 {discountBps === null ? "未知" : discountPercent(discountBps)}，人民币报价已含优惠。</p>
            </div>
            <section className="price-sheet-group" aria-label={`${detailGroup?.display_name ?? "Base USD"}价格`}>
              <header className="price-sheet-group-header"><h3>{detailGroup?.display_name ?? "Base USD"}</h3>{detailGroup !== undefined && <span>{detailGroup.quote.effective_multiplier}×</span>}</header>
              {detailGroup !== undefined && <p className="text-caption text-muted">{detailGroup.description} 换算率 {detailGroup.quote.cny_per_usd}，已含组织优惠。</p>}
              {detailTiers === undefined ? <CardMessage>暂无法读取美元基准价，请选择分组查看人民币报价。</CardMessage> : <PriceDetails tiers={detailTiers} currency={detailCurrency} />}
            </section>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  </tbody>;
}
