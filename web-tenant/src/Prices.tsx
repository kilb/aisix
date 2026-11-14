import { ChevronDown } from "lucide-react";
import { Fragment, useEffect, useId, useMemo, useState } from "react";
import "./Prices.css";
import { ApiError } from "./lib/api";
import { messageFor } from "./lib/errors";
import { fmtBeijing } from "./lib/time";
import { currentPrices, discountPercent, type Dimension, type PriceQuoteList } from "./lib/priceQuote";
import { modelPriceCatalog, type CatalogModel, type ModelGroupQuote } from "./lib/priceCatalog";
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
  const header = <PageHeader title="模型与价格" dataLead="按模型比较各分组的当前倍率与单价，选择适合调用的分组。" />;
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
          <div><dt>当前组织优惠</dt><dd>{quote.discount_bps === null ? "未知" : discountPercent(quote.discount_bps)}</dd><span>下方价格已计入优惠</span></div>
          <div><dt>可计价模型</dt><dd>{catalog.length.toLocaleString("en-US")}</dd><span>{quote.groups.length.toLocaleString("en-US")} 个可用分组</span></div>
          <div className="price-summary-time"><dt>报价时刻</dt><dd>{fmtBeijing(quote.as_of)}</dd><span>{quote.valid_until === null ? "当前报价未设置失效时刻" : `有效至 ${fmtBeijing(quote.valid_until)}`}</span></div>
        </dl>
        <details className="price-quote-explainer"><summary>价格如何计算</summary><p>倍率 = 换算率 ×（1 − 组织优惠）。人民币单价按美元基准价乘以倍率计算。当前报价不改变历史账单；历史费用使用调用时的费率。</p></details>
      </Card>
      <Card className={span.d12}>
        <CardHeader title="模型价格目录" />
        <p className="text-caption text-muted">价格以人民币计，按实际用量计费。摘要跟随所选服务档位，单位为元/百万 Token；展开可对比全部档位、长上下文与其他计量项目。</p>
        <PriceCatalog quote={quote} groupId={selectedGroup} onGroupChange={(value) => setGroupFilter({ linkedGroup: initialGroup, value })} />
        <p className="price-estimate-note text-caption text-muted">已列出的单价按价卡确定；“未提供价格”的项目不按零计费。“含估算”表示调用的部分 token 用量由网关估算，与单价无关，具体调用会在用量页单独标注。</p>
      </Card>
    </BentoGrid>
  </>;
}

const tierName = (tier: string) => ({ default: "标准", standard: "Standard", fast: "Fast", priority: "Priority", batch: "Batch", flex: "Flex" } as Record<string, string>)[tier] ?? tier;

const searchModel = (model: CatalogModel) => [model.name, ...model.groups.flatMap((group) => [group.display_name, group.description])];

function PriceCatalog({ quote, groupId, onGroupChange }: { quote: PriceQuoteList; groupId: string; onGroupChange: (group: string) => void }) {
  const models = useMemo(() => modelPriceCatalog(quote.groups, groupId), [quote.groups, groupId]);
  const table = useRecordTable({ items: models, searchText: searchModel, resetKey: groupId });
  const [tier, setTier] = useState("default");
  const tiers = [...new Set(models.flatMap((model) => model.groups.flatMap((group) => Object.keys(group.quote.prices_by_tier))))].sort((a, b) => (a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b)));
  return <>
    <RecordTableToolbar search={{ value: table.query, onChange: table.setQuery, label: "搜索模型价格", placeholder: "模型名、分组或说明" }}>
      <label className="price-group-filter">分组筛选
        <select value={groupId} onChange={(event) => onGroupChange(event.target.value)}>
          <option value="">全部分组</option>
          {quote.groups.map((group) => <option key={group.id} value={group.id}>{group.display_name}</option>)}
        </select>
      </label>
      <label className="price-group-filter">服务档位<select aria-label="服务档位" value={tier} onChange={(event) => setTier(event.target.value)}><option value="default">标准</option>{tiers.filter((value) => value !== "default").map((value) => <option key={value} value={value}>{tierName(value)}</option>)}</select></label>
    </RecordTableToolbar>
    {table.rows.length === 0 ? <EmptyState text={table.query || groupId ? "没有符合筛选条件的模型价格。" : "当前分组暂时没有可计价的模型。"} mark="route" /> : <>
      <div className="price-catalog" role="region" aria-label="模型价格"><DataTable label="模型单价">
        <colgroup><col className="price-summary-group" /><col className="price-summary-multiplier" />
          {Array.from({ length: 4 }, (_, index) => <col key={index} className="price-summary-amount" />)}
        </colgroup>
        <thead><tr><th>模型 / 分组</th><th className="num">倍率</th><th className="num">输入</th><th className="num">输出</th><th className="num">缓存读</th><th className="num">缓存写</th></tr></thead>
        {table.rows.map((model) => <ModelRows key={model.model_id} model={model} tier={tier} discountBps={quote.discount_bps} />)}
      </DataTable></div>
    </>}
    <RecordTablePager page={table.page} pageSize={table.pageSize} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} total={table.filteredCount} label="模型价格分页" />
  </>;
}

function GroupPriceRow({ group, discountBps, tier }: { group: ModelGroupQuote; discountBps: number | null; tier: string }) {
  const base = group.quote.prices_by_tier[tier]?.base ?? [];
  const rate = (dimension: Dimension) => base.find((item) => item.dimension === dimension);
  const oneHour = rate("cache_write_1h");
  return <tr data-price-group={group.id} className="price-group-row">
    <th scope="row" className="price-group-name"><span>{group.display_name}</span>
      {group.description !== "" && <span className="cell-note hint">{group.description}</span>}
    </th>
    <td className="num price-multiplier" data-label="倍率"><span className="price-multiplier-value">{group.quote.effective_multiplier}×</span>
      <span className="cell-note hint" title={`${group.quote.cny_per_usd} ×（1 − ${discountBps === null ? "未知" : discountPercent(discountBps)}）`}>已含组织优惠</span>
    </td>
    <td className="num" data-label="输入"><PriceRateValue rate={rate("input")} /></td>
    <td className="num" data-label="输出"><PriceRateValue rate={rate("output")} /></td>
    <td className="num" data-label="缓存读"><PriceRateValue rate={rate("cache_read")} /></td>
    <td className="num" data-label="缓存写"><PriceRateValue rate={rate("cache_write")} />
      {oneHour?.fallback_from === undefined && oneHour?.price_cny_per_million != null &&
        <span className="hint cell-note">1 小时缓存写 <PriceRateValue rate={oneHour} /></span>}
    </td>
  </tr>;
}

function ModelRows({ model, discountBps, tier }: { model: CatalogModel; discountBps: number | null; tier: string }) {
  const [expanded, setExpanded] = useState(false);
  const [copyError, setCopyError] = useState<string | null>(null);
  const detailsId = useId();
  return <tbody data-price-model={model.model_id}>
    <tr className="price-model-heading"><td colSpan={6}>
      <div className="price-model-bar"><span className="break-all select-text" data-model-name>{model.name}<span className="price-model-tier">{tierName(tier)}{model.groups.some((group) => (group.quote.prices_by_tier[tier]?.long_context.length ?? 0) > 0) ? " · 含长上下文价格" : ""}</span></span>
        <div className="price-model-actions">
          <CopyButton text={async () => { setCopyError(null); return model.name; }} label="复制模型名" size="sm"
            onFailed={(error) => setCopyError(`复制失败：${failure(error)}。请手动选择上方模型名。`)} />
          <Button size="sm" kind="quiet" className="tenant-icon-action" aria-label={expanded ? "收起价格" : "查看全部价格"} title={expanded ? "收起价格" : "查看全部价格"} aria-expanded={expanded} aria-controls={detailsId} onClick={() => setExpanded(!expanded)}>
            <ChevronDown className="price-expand-icon" aria-hidden />
          </Button>
        </div>
      </div>
      {copyError !== null && <p className="field-problem" role="alert">{copyError}</p>}
    </td></tr>
    {model.groups.map((group) => <GroupPriceRow key={group.id} group={group} tier={tier} discountBps={discountBps} />)}
    {expanded && <tr className="price-expanded-row"><td colSpan={6} id={detailsId} className="price-detail-cell">
      {model.groups.map((group) => <Fragment key={group.id}>
        <h3 className="price-detail-group text-body font-medium">{group.display_name} · 完整价格</h3>
        <PriceDetails tiers={group.quote.prices_by_tier} />
      </Fragment>)}
    </td></tr>}
  </tbody>;
}
