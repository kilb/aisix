import { useEffect, useId, useRef, useState } from "react";
import { fetchApiMetrics, maximumMetricIndex, metricsRangeError, rankMetricModels, requestRateSummary, type ApiMetricValues, type ApiMetricsResponse, type MetricsBucket, type MetricsLatency, type MetricsScope } from "../lib/apiMetrics";
import { formatCount, formatDuration, formatRatio, normalizeExactValues } from "../lib/chartFormat";
import { cn } from "../lib/cn";
import { Button } from "../ui/Button";
import { ApiSuccessSummary } from "./ApiSuccessSummary";
import { fmtMicroCny, fmtUtc } from "../lib/money";
import { RankBars, TrendChart, type TrendPoint } from "./charts";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, span } from "./page";

export interface ApiMetricsPanelProps {
  scope: MetricsScope;
  /** 页面先完成会话、角色与组织状态判定；关闭时不请求也不保留旧范围数据。 */
  enabled: boolean;
  from: string;
  to: string;
  bucket?: MetricsBucket;
  modelId?: string;
  memberId?: string;
  apiKeyId?: string;
  tenantId?: string;
  numberStart?: number;
  refreshKey?: number;
  className?: string;
  compact?: boolean;
}

type View = "requests" | "latency" | "cache" | "cost";
type LatencyKind = "first_token_latency_ms" | "response_latency_ms" | "attempt_duration_ms";
type CacheKind = "tokens" | "response";
const VIEWS: { value: View; label: string }[] = [{ value: "requests", label: "请求" }, { value: "latency", label: "延迟" }, { value: "cache", label: "缓存" }, { value: "cost", label: "费用" }];
const LATENCY_LABELS: Record<LatencyKind, string> = { first_token_latency_ms: "首 token 延迟", response_latency_ms: "响应延迟", attempt_duration_ms: "上游尝试完整耗时" };
const LATENCY_NOTES: Record<LatencyKind, string> = {
  first_token_latency_ms: "首个生成内容交付的实测延迟。无首 token 样本包含非流式与历史记录，不以首帧或完整耗时补齐。",
  response_latency_ms: "请求至首个可用数据的实测延迟。流式请求的首个可用数据不代表完整响应结束。",
  attempt_duration_ms: "每次上游尝试的完整耗时，包含可见的已计费重试；不能拼成客户端完整请求耗时。",
};
const BUCKET_LABELS: Record<MetricsBucket, string> = { minute: "分钟", hour: "小时", day: "天" };

function money(value: bigint | null): string { return value === null ? "未知" : fmtMicroCny(value); }
function sumCounts(left: number | null | undefined, right: number | null | undefined): bigint | null {
  return left == null || right == null ? null : BigInt(left) + BigInt(right);
}
function modelName(name: string): string { return name || "未授权或未知模型"; }

function Facts({ items }: { items: { label: string; value: string }[] }) {
  return <dl className="m-0 grid min-w-0 grid-cols-2 gap-x-6 gap-y-4">{items.map(item => <div key={item.label} className="min-w-0"><dt className="font-mono text-micro text-muted">{item.label}</dt><dd className="m-0 mt-1 min-w-0 break-all text-stat font-normal tabular-nums text-ink">{item.value}</dd></div>)}</dl>;
}

function LatencyFacts({ title, value, note }: { title: string; value: MetricsLatency | undefined; note: string }) {
  return <section className="min-w-0"><h3 className="mb-4 font-mono text-meta font-normal text-ink">{title}</h3><Facts items={[
    { label: "平均", value: formatDuration(value?.avg) }, { label: "P50", value: formatDuration(value?.p50) },
    { label: "P95", value: formatDuration(value?.p95) }, { label: "实测样本", value: formatCount(value?.sample_count) },
    { label: "无实测值", value: formatCount(value?.missing_count) },
  ]} /><p className="mt-3 text-caption text-muted">{note}</p></section>;
}

/** 横轴来自服务器实际时间桶；位置归一化与精确金额显示互不回流。 */
function trendPoints(data: ApiMetricsResponse, view: View, latencyKind: LatencyKind, cacheKind: CacheKind): TrendPoint[] {
  const costs = view === "cost" ? normalizeExactValues(data.series.map(row => row.charged_micro_cny)) : [];
  return data.series.map((row, index) => {
    const iso = new Date(row.period_start).toISOString();
    const label = data.bucket === "day" ? iso.slice(0, 10) : `${iso.slice(5, 10)} ${iso.slice(11, 16)}`;
    if (view === "cost") return { label, value: costs[index] ?? null, displayValue: money(row.charged_micro_cny) };
    if (view === "latency") return { label, value: row[latencyKind].avg, displayValue: formatDuration(row[latencyKind].avg) };
    if (view === "cache") {
      const value = cacheKind === "tokens" ? row.tokens.cache_hit_rate : row.response_cache?.hit_rate;
      return { label, value, displayValue: formatRatio(value) };
    }
    return { label, value: row.requests, displayValue: formatCount(row.requests) };
  });
}

function currentPeriod(data: ApiMetricsResponse): number | undefined {
  const duration = data.bucket === "minute" ? 60_000 : data.bucket === "hour" ? 3_600_000 : 86_400_000;
  const now = Math.floor(Date.now() / duration);
  const index = data.series.findIndex(row => Math.floor(Date.parse(row.period_start) / duration) === now);
  return index < 0 ? undefined : index;
}

function ModelRanks({ data, field }: { data: ApiMetricsResponse | null; field: "requests" | "charged_micro_cny" }) {
  if (data === null) return <><div aria-hidden="true" className="grid gap-4">{Array.from({ length: 5 }, (_, index) => <div key={index}><div className="mb-2 flex justify-between gap-4"><span className="h-5 w-1/3 rounded-chip bg-ink/7" /><span className="h-5 w-1/4 rounded-chip bg-ink/7" /></div><div className="h-2.5 rounded-full bg-ink/7" /></div>)}</div><p className="mt-5 text-caption text-muted">展示所选范围的全部模型分组。未授权或已无法识别的模型合并显示，未暴露其内部名称。</p></>;
  const ranked = rankMetricModels(data.models, field);
  if (ranked === null) return <CardMessage>部分模型的{field === "requests" ? "请求数" : "费用"}未知，无法完整比较模型分布。</CardMessage>;
  const shown = ranked.slice(0, 5);
  const normalized = normalizeExactValues(shown.map(row => row[field]));
  const leader = shown[0];
  const leaderValue = leader === undefined ? "" : field === "requests" ? `${formatCount(leader.requests)} 次` : money(leader.charged_micro_cny);
  return <><RankBars items={shown.map((row, index) => ({ label: modelName(row.model.name), value: normalized[index] ?? 0, displayValue: field === "requests" ? `${formatCount(row.requests)} 次` : money(row.charged_micro_cny) }))} accessibleLabel={leader ? `当前范围${field === "requests" ? "请求数" : "费用"}最多的模型分组为${modelName(leader.model.name)}，${leaderValue}；展示前 ${formatCount(shown.length)} 个，共 ${formatCount(ranked.length)} 个分组` : "当前范围没有模型用量"} emptyLabel="所选范围内没有模型用量" /><p className="mt-5 text-caption text-muted">{ranked.length > shown.length ? `按全部 ${formatCount(ranked.length)} 个模型分组比较，展示前 ${formatCount(shown.length)} 个。` : "展示所选范围的全部模型分组。"}未授权或已无法识别的模型合并显示，未暴露其内部名称。</p></>;
}

function CacheFacts({ values }: { values: ApiMetricValues | undefined }) {
  const cache = values?.response_cache;
  return <>
    <div><div className="font-mono text-micro text-muted">响应缓存命中率</div><Metric size="value" className="mt-3">{formatRatio(cache?.hit_rate)}</Metric><p className="mt-3 text-caption text-muted">命中 ÷（命中 + 未命中）；绕过、未启用与未知不进入分母。</p></div>
    <Facts items={[
      { label: "命中请求", value: formatCount(cache?.hit_requests) }, { label: "未命中请求", value: formatCount(cache?.miss_requests) },
      { label: "绕过缓存", value: formatCount(cache?.bypass_requests) }, { label: "未启用缓存", value: formatCount(cache?.disabled_requests) },
      { label: "缓存状态未知", value: formatCount(cache?.unknown_requests) },
    ]} />
    <div className="border-t border-ink/7 pt-5"><h3 className="mb-4 font-mono text-meta font-normal text-ink">已记录 token</h3><Facts items={[
      { label: "输入合计", value: formatCount(values?.tokens.input_tokens) }, { label: "输出", value: formatCount(values?.tokens.completion_tokens) },
      { label: "推理 token", value: formatCount(values?.tokens.reasoning_tokens) }, { label: "原始输入", value: formatCount(values?.tokens.prompt_tokens) },
      { label: "输入内缓存子集", value: formatCount(values?.tokens.cached_prompt_tokens) }, { label: "额外缓存读取", value: formatCount(values?.tokens.cache_read_tokens) },
      { label: "额外缓存创建", value: formatCount(values?.tokens.cache_creation_tokens) }, { label: "已记录缓存读取合计", value: formatCount(values?.tokens.cache_hit_tokens) },
    ]} /></div>
    <p className="text-caption text-muted">输入合计 = 原始输入 + 额外缓存读取 + 额外缓存创建。输入内缓存子集已在原始输入内，不能再相加。缓存计数未区分明确报告零与未报告。</p>
  </>;
}

/** 首页诊断只投影同次响应，延迟与两种缓存仍有直接可达的完整明细。 */
function QuickDiagnostics({ totals, onView }: { totals?: ApiMetricValues; onView: (view: View) => void }) {
  return <>
    <dl className="m-0 grid gap-5">
      <div className="flex min-w-0 items-start justify-between gap-5"><dt className="min-w-0 text-caption text-muted">首 token 延迟（平均）<p className="mt-2 font-mono text-micro">实测 {formatCount(totals?.first_token_latency_ms.sample_count)} · 无值 {formatCount(totals?.first_token_latency_ms.missing_count)}</p></dt><dd className="m-0 min-w-0"><Metric size="value">{formatDuration(totals?.first_token_latency_ms.avg)}</Metric></dd></div>
      <div className="flex min-w-0 items-start justify-between gap-5 border-t border-ink/7 pt-4"><dt className="min-w-0 text-caption text-muted">已记录缓存 token 占输入比例</dt><dd className="m-0 min-w-0"><Metric size="value">{formatRatio(totals?.tokens.cache_hit_rate)}</Metric></dd></div>
      <div className="flex min-w-0 items-start justify-between gap-5 border-t border-ink/7 pt-4"><dt className="min-w-0 text-caption text-muted">响应缓存命中率<p className="mt-2 font-mono text-micro">命中 + 未命中样本 {formatCount(sumCounts(totals?.response_cache.hit_requests, totals?.response_cache.miss_requests))}</p></dt><dd className="m-0 min-w-0"><Metric size="value">{formatRatio(totals?.response_cache.hit_rate)}</Metric></dd></div>
    </dl>
    <p className="text-caption text-muted">首 token 无值包含非流式与历史记录；缓存 token 比例不代表上游报告覆盖率。</p>
    <div className="mt-auto flex flex-wrap gap-2 pt-2"><Button kind="quiet" onClick={() => onView("latency")}>查看延迟</Button><Button kind="quiet" onClick={() => onView("cache")}>查看缓存</Button></div>
  </>;
}

/** 固定两张卡和连续编号；切换指标只改变真实度量，不改变查询范围。 */
export function ApiMetricsPanel({ scope, enabled, from, to, bucket, modelId, memberId, apiKeyId, tenantId, numberStart = 1, refreshKey = 0, className, compact = false }: ApiMetricsPanelProps) {
  const [view, setView] = useState<View>("requests");
  const [detailsOpen, setDetailsOpen] = useState(false);
  const viewButtons = useRef<Partial<Record<View, HTMLButtonElement>>>({});
  const [latencyKind, setLatencyKind] = useState<LatencyKind>("first_token_latency_ms");
  const [cacheKind, setCacheKind] = useState<CacheKind>("tokens");
  const [revision, setRevision] = useState(0);
  const [answer, setAnswer] = useState<{ key: string; data: ApiMetricsResponse | null; error: string | null }>({ key: "", data: null, error: null });
  const choiceId = useId();
  const rangeError = metricsRangeError(from, to, bucket);
  const key = JSON.stringify([scope, from, to, bucket, modelId, memberId, apiKeyId, tenantId, refreshKey, revision]);
  useEffect(() => {
    if (!enabled || rangeError) {
      setAnswer({ key: "", data: null, error: null });
      return;
    }
    let current = true;
    setAnswer({ key, data: null, error: null });
    void fetchApiMetrics(scope, { from, to, bucket, model_id: modelId, member_id: memberId, api_key_id: apiKeyId, tenant_id: scope === "admin" ? tenantId : undefined }).then(data => {
      if (current) setAnswer({ key, data, error: null });
    }).catch((error: unknown) => {
      if (current) setAnswer({ key, data: null, error: error instanceof Error ? error.message : "API 指标加载失败，请重试。" });
    });
    return () => { current = false; };
  }, [enabled, key, rangeError, scope, from, to, bucket, modelId, memberId, apiKeyId, tenantId]);
  if (!enabled) return null;
  const data = answer.key === key ? answer.data : null;
  const error = rangeError ?? (answer.key === key ? answer.error : null);
  const totals = data?.totals;
  const requestRates = data ? requestRateSummary(data.totals?.requests, data.totals?.success_rate, data.from, data.to) : null;
  const points = data ? trendPoints(data, view, latencyKind, cacheKind) : [];
  const metricTitle = view === "requests" ? "最终请求" : view === "latency" ? LATENCY_LABELS[latencyKind] : view === "cost" ? "已计价费用" : cacheKind === "tokens" ? "已记录缓存 token 占输入比例" : "响应缓存命中率";
  const primary = !totals ? "—" : view === "requests" ? formatCount(totals?.requests) : view === "latency" ? formatDuration(totals?.[latencyKind].avg) : view === "cost" ? money(totals?.charged_micro_cny ?? null) : formatRatio(cacheKind === "tokens" ? totals?.tokens.cache_hit_rate : totals?.response_cache?.hit_rate);
  const maximum = points[maximumMetricIndex(view === "cost" && data ? data.series.map(row => row.charged_micro_cny) : points.map(point => point.value))];
  const trendLabel = maximum ? `${metricTitle}按${data ? BUCKET_LABELS[data.bucket] : "期间"}统计，最高期间为 ${maximum.label}，${maximum.displayValue}` : `${metricTitle}在所选范围没有可用实测值`;
  const sideTitle = view === "requests" ? "模型请求分布" : view === "cost" ? "模型费用分布" : view === "cache" ? "响应缓存与 token 明细" : "耗时样本与口径";
  const windowSeconds = (Date.parse(to) - Date.parse(from)) / 1000;
  const displayBucket = data?.bucket ?? bucket ?? (windowSeconds <= 6 * 3600 ? "minute" : windowSeconds <= 3 * 86400 ? "hour" : "day");
  function selectView(value: View) {
    setView(value);
    if (compact) setDetailsOpen(true);
  }
  return (
    <BentoGrid className={cn(span.d12, "min-w-0", className)} role={compact && !detailsOpen ? "group" : "region"} aria-label={compact && !detailsOpen ? "API 指标概览" : "完整 API 指标"} aria-busy={!error && !data}>
      <Card className={span.d7}>
        <CardHeader number={numberStart} title={compact && !detailsOpen ? "API 请求健康" : `API 指标 · ${VIEWS.find(item => item.value === view)?.label}`} description={`${fmtUtc(data?.from ?? from)} — ${fmtUtc(data?.to ?? to)}（结束时间不含）`} />
        {compact && !detailsOpen && <ApiSuccessSummary totals={totals} />}
        <div role="group" aria-label="指标视图" className="mb-6 flex w-fit max-w-full flex-wrap gap-1 rounded-full bg-ink/7 p-1">
          {VIEWS.map(item => <button key={item.value} type="button" ref={element => { viewButtons.current[item.value] = element ?? undefined; }} aria-expanded={compact ? detailsOpen && view === item.value : undefined} aria-pressed={view === item.value && (!compact || detailsOpen)} onClick={() => selectView(item.value)} className={cn("min-h-11 min-w-11 rounded-full border-0 px-4 font-mono text-caption font-normal", view === item.value && (!compact || detailsOpen) ? "bg-surface text-ink" : "bg-transparent text-muted hover:text-ink")}>{item.label}</button>)}
        </div>
        {compact && <div className="flex min-h-11 flex-wrap items-center justify-between gap-3 text-caption text-muted">
          {error ? <><span role="alert">{error}</span>{!rangeError && <Button onClick={() => setRevision(value => value + 1)}>重试</Button>}</> : !data ? <span role="status">正在加载当前范围的 API 指标</span> : <span>统计周期平均请求速率 {requestRates?.averageRps == null ? "未知" : `${new Intl.NumberFormat("en-US", { maximumSignificantDigits: 3 }).format(requestRates.averageRps)} 次/秒`}</span>}
          {detailsOpen && <Button kind="quiet" onClick={() => { setDetailsOpen(false); viewButtons.current[view]?.focus(); }}>收起指标明细</Button>}
        </div>}
        {(!compact || detailsOpen) && view === "latency" && <label htmlFor={choiceId} className="mb-5 grid gap-2 text-caption text-muted">耗时口径<select id={choiceId} value={latencyKind} onChange={event => setLatencyKind(event.target.value as LatencyKind)}>{Object.entries(LATENCY_LABELS).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>}
        {(!compact || detailsOpen) && view === "cache" && <label htmlFor={choiceId} className="mb-5 grid gap-2 text-caption text-muted">缓存口径<select id={choiceId} value={cacheKind} onChange={event => setCacheKind(event.target.value as CacheKind)}><option value="tokens">已记录缓存 token 占输入比例</option><option value="response">响应缓存命中率</option></select></label>}
        {(!compact || detailsOpen) && <>
          <div className="mb-6 min-w-0"><div className="mb-3 font-mono text-micro text-muted">{metricTitle}{view === "latency" ? " · 平均" : ""}</div><Metric size={primary.length > 18 ? "value" : "figure"} className="break-all">{primary}</Metric></div>
          {view === "requests" && <Facts items={[
            { label: "成功率", value: formatRatio(totals?.success_rate) },
            { label: "失败率（已知结果）", value: formatRatio(requestRates?.failureRate) },
            { label: "统计周期平均请求速率", value: requestRates?.averageRps == null ? "未知" : `${new Intl.NumberFormat("en-US", { maximumSignificantDigits: 3 }).format(requestRates.averageRps)} 次/秒` }, { label: "结果已知的请求", value: formatCount(totals?.known_outcome_requests) }, { label: "成功请求", value: formatCount(totals?.succeeded_requests) },
            { label: "失败请求", value: formatCount(totals?.failed_requests) }, { label: "结果未知的请求", value: formatCount(totals?.unknown_outcome_requests) },
            { label: "计费尝试", value: formatCount(totals?.billing_attempts) }, { label: "最终状态未知的尝试", value: formatCount(totals?.unknown_final_attempts) },
          ]} />}
          {view === "latency" && <Facts items={[
            { label: "P50", value: formatDuration(totals?.[latencyKind].p50) }, { label: "P95", value: formatDuration(totals?.[latencyKind].p95) },
            { label: "实测样本", value: formatCount(totals?.[latencyKind].sample_count) }, { label: "无实测值", value: formatCount(totals?.[latencyKind].missing_count) },
          ]} />}
          {view === "cache" && <Facts items={cacheKind === "tokens" ? [
            { label: "已记录缓存读取", value: formatCount(totals?.tokens.cache_hit_tokens) }, { label: "输入 token", value: formatCount(totals?.tokens.input_tokens) },
            { label: "有可用用量记录的尝试", value: formatCount(totals?.tokens.known_usage_attempts) }, { label: "无可用用量记录的尝试", value: formatCount(totals?.tokens.unknown_usage_attempts) },
          ] : [
            { label: "命中请求", value: formatCount(totals?.response_cache?.hit_requests) }, { label: "查询缓存的请求", value: formatCount(sumCounts(totals?.response_cache?.hit_requests, totals?.response_cache?.miss_requests)) },
          ]} />}
          {view === "cost" && <Facts items={[{ label: "最终请求", value: formatCount(totals?.requests) }, { label: "计费尝试", value: formatCount(totals?.billing_attempts) }, { label: "待计价尝试", value: formatCount(totals?.unpriced_attempts) }, { label: "费用调整", value: money(totals?.adjusted_micro_cny ?? null) }]} />}
          <CardFooter className="pt-7"><div className="min-h-64">{data ? <TrendChart points={points} currentIndex={currentPeriod(data)} accessibleLabel={trendLabel} emptyLabel="所选范围没有可用期间数据" /> : <div><div className="relative h-56">{error ? <div className="absolute inset-0 flex items-center"><CardMessage onRetry={rangeError ? undefined : () => setRevision(value => value + 1)}>{error}</CardMessage></div> : <><div aria-hidden="true" className="h-full w-full rounded-cell bg-ink/7" /><span role="status" className="sr-only">正在加载完整 API 指标</span></>}</div><div aria-hidden="true" className="mt-2 flex justify-between font-mono text-micro text-muted"><span>—</span><span>—</span><span>—</span></div></div>}</div><p className="mt-4 text-caption text-muted">按 UTC {BUCKET_LABELS[displayBucket]}汇总，图形使用相对刻度，精确值可用键盘逐期查看。</p></CardFooter>
          <p className="mt-4 text-caption text-muted">{view === "latency" ? LATENCY_NOTES[latencyKind] : view === "cache" ? cacheKind === "tokens" ? "（输入内缓存子集 + 额外缓存读取）÷（原始输入 + 额外缓存读取 + 额外缓存创建）。这是已记录 token 的比例，不代表上游报告覆盖率。" : "响应缓存命中 ÷（命中 + 未命中）；绕过、未启用与未知不进入分母。" : view === "requests" ? "成功率 = 成功请求 ÷（成功 + 失败请求），失败率使用同一已知结果分母。结果未知的请求单列；最终状态未知的尝试不作为最终请求。请求速率是整个查询周期的平均值，不是瞬时 RPS。" : "已计价费用汇总最终记录及可见的已计费重试，保留当笔历史费率与费用调整。待计价尝试不等于免费，完成计价后费用可能变化。"}</p>
          <p className="mt-3 text-caption text-muted">请求数按最终记录统计，费用与 token 还包含已计费重试。计费尝试包含免费与未定价的最终记录，不等于请求数。</p>
        </>}
      </Card>
      <Card className={span.d5Wide}>
        <CardHeader number={numberStart + 1} title={compact && !detailsOpen ? "延迟与缓存" : sideTitle} description="与左侧使用相同时间、身份和筛选范围" />
        {compact && !detailsOpen ? <QuickDiagnostics totals={totals} onView={selectView} /> : view === "requests" || view === "cost" ? <ModelRanks data={data} field={view === "requests" ? "requests" : "charged_micro_cny"} /> : view === "cache" ? <CacheFacts values={totals} /> : <div className="grid gap-7"><LatencyFacts title="首 token 延迟" value={totals?.first_token_latency_ms} note="最终请求的首个生成内容；无样本包含非流式与历史记录。" /><div className="border-t border-ink/7 pt-6"><LatencyFacts title="响应延迟" value={totals?.response_latency_ms} note="最终请求至首个可用数据，不是完整响应时长。" /></div><div className="border-t border-ink/7 pt-6"><LatencyFacts title="上游尝试完整耗时" value={totals?.attempt_duration_ms} note="所有可见计费尝试各自的完整耗时，不能相加为客户端请求时长。" /></div></div>}
      </Card>
    </BentoGrid>
  );
}
