import { Select } from "../ui/Select";
import { useEffect, useId, useMemo, useState } from "react";
import { ArrowDownRight, ArrowUpRight, Check, ChevronDown, RefreshCw, Search } from "lucide-react";
import { fetchDirectModelMetrics, type DirectModelMetricRow, type DirectModelMetricsQuery, type DirectModelMetricsResponse } from "../lib/directModelMetrics";
import { metricsRangeError, type MetricsBucket, type MetricsLatency } from "../lib/apiMetrics";
import { formatCount, formatDuration, formatRatio } from "../lib/chartFormat";
import { errorText } from "../lib/fmt";
import { useHashQuery } from "../lib/useHashQuery";
import { useRecordTable } from "../lib/recordTable";
import { beijingInputValue, fmtBeijing, metricPeriodLabel, parseBeijingInput } from "../lib/time";
import { Button } from "../ui/Button";
import { CopyButton } from "../ui/CopyButton";
import { EmptyState } from "../ui/EmptyState";
import { RecordTablePager } from "../ui/RecordTable";
import { Card, CardHeader, PageHeader } from "./page";
import { DataTable } from "../ui/DataTable";
import { TrendChart } from "./charts";
import { OUTPUT_THROUGHPUT_NOTE } from "./UsagePerformance";
import "./DirectModelMonitoring.css";

function formatTps(value: number | null | undefined): string {
  return value === null || value === undefined || !Number.isFinite(value) ? "—" : `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(value)} token/s`;
}

function Sample({ value }: { value: MetricsLatency }) {
  return <span className="direct-monitor-samples">实测 {formatCount(value.sample_count)} · 缺失 {formatCount(value.missing_count)}</span>;
}

function Outcome({ model }: { model: DirectModelMetricRow }) {
  const failed = model.failed_attempts;
  return <div className="direct-monitor-outcome">
    <span className={failed !== null && failed > 0 ? "direct-monitor-failure" : undefined}>{formatRatio(model.failure_rate)}</span>
    <span className="direct-monitor-samples">失败 {formatCount(failed)} / 已知结果 {model.succeeded_attempts === null || failed === null ? "—" : formatCount(model.succeeded_attempts + failed)}</span>
  </div>;
}

function ModelRow({ model }: { model: DirectModelMetricRow }) {
  const [copyError, setCopyError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState(false);
  const detailId = useId();
  return <>
    <tr className="direct-monitor-model" aria-label={model.model.name}>
      <td><div className="direct-monitor-model-identity">
        <h3>{model.model.name || "未命名直连模型"}</h3>
        <div className="direct-monitor-model-id"><span title={model.model.id}>{model.model.id}</span><CopyButton text={model.model.id} label="复制 ID" size="sm" onFailed={cause => setCopyError(errorText(cause))} /></div>
        <p>{model.provider_key.name || "上游账号名称未知"}</p>
      </div></td>
      <td className="num"><strong>{formatCount(model.attempts)}</strong><span className="direct-monitor-samples">结果未知 {formatCount(model.unknown_outcome_attempts)}</span></td>
      <td className="num"><Outcome model={model} /></td>
      <td className="num"><strong>{formatDuration(model.first_token_latency_ms.p50)}</strong><span className="direct-monitor-samples">上游首帧 {formatDuration(model.upstream_ttft_ms.p50)}</span></td>
      <td className="num"><strong>{formatRatio(model.tokens.cache_hit_rate)}</strong><span className="direct-monitor-samples">命中 {formatCount(model.tokens.cache_measured_hit_tokens)} / 已采样输入 {formatCount(model.tokens.cache_measured_input_tokens)}</span></td>
      <td className="num"><strong>{formatTps(model.throughput_tps.avg)}</strong><Sample value={model.throughput_tps} /></td>
      <td className="right"><Button type="button" kind="quiet" size="sm" aria-expanded={expanded} aria-controls={detailId} onClick={() => setExpanded(value => !value)}>样本与延迟明细<ChevronDown size={16} aria-hidden /></Button></td>
    </tr>
    <tr className="dt-detail" hidden={!expanded && !copyError}><td colSpan={7}>
      {copyError && <p className="note crit" role="alert">复制失败：{copyError}；可选中模型 ID 复制。</p>}
      <div id={detailId} className="direct-monitor-model-details" role="region" aria-label={`${model.model.name}样本与延迟明细`} hidden={!expanded}>
      <dl>
        <div><dt>上游首帧延迟</dt><dd>P50 {formatDuration(model.upstream_ttft_ms.p50)} · P95 {formatDuration(model.upstream_ttft_ms.p95)} · P99 {formatDuration(model.upstream_ttft_ms.p99)}<Sample value={model.upstream_ttft_ms} /></dd></div>
        <div><dt>客户端首 token 延迟</dt><dd>P50 {formatDuration(model.first_token_latency_ms.p50)} · P95 {formatDuration(model.first_token_latency_ms.p95)}<Sample value={model.first_token_latency_ms} /></dd></div>
        <div><dt>上游尝试耗时</dt><dd>P50 {formatDuration(model.attempt_duration_ms.p50)} · P95 {formatDuration(model.attempt_duration_ms.p95)}<Sample value={model.attempt_duration_ms} /></dd></div>
        <div><dt title={OUTPUT_THROUGHPUT_NOTE}>输出 TPS 分布</dt><dd>P50 {formatTps(model.throughput_tps.p50)} · P95 {formatTps(model.throughput_tps.p95)}<Sample value={model.throughput_tps} /></dd></div>
        <div><dt>成功与未知结果</dt><dd>成功 {formatCount(model.succeeded_attempts)} · 失败 {formatCount(model.failed_attempts)} · 未知 {formatCount(model.unknown_outcome_attempts)}</dd></div>
        <div><dt>归属与排除记录</dt><dd>归属 {formatCount(model.attributed_attempts)} · 排除 {formatCount(model.excluded_attempts)} · 无法确认派发 {formatCount(model.unmeasured_attempts)}</dd></div>
        <div><dt>Token 用量样本</dt><dd>已报告 {formatCount(model.tokens.known_usage_attempts)} · 未报告 {formatCount(model.tokens.unknown_usage_attempts)}</dd></div>
        <div><dt>缓存计数样本</dt><dd>已报告 {formatCount(model.tokens.cache_measured_attempts)} · 未报告 {formatCount(model.tokens.cache_missing_attempts)} · 全部输入 {formatCount(model.tokens.input_tokens)}</dd></div>
        <div><dt>上游账号 ID</dt><dd className="direct-monitor-identifier">{model.provider_key.id ?? "—"}</dd></div>
      </dl>
      </div>
    </td></tr>
  </>;
}

type RequestState = { key: string; data: DirectModelMetricsResponse | null; error: string | null };
const DEFAULT_RANGE_MS = 24 * 60 * 60 * 1000;

/** 输入完整条件后再查询，避免录入 UUID 时逐字触发无效请求。 */
function ModelScopeFilters({ request, onApply }: { request: DirectModelMetricsQuery; onApply: (patch: Record<string, string>) => void }) {
  const [modelId, setModelId] = useState(request.model_id ?? "");
  const [accountId, setAccountId] = useState(request.provider_key_id ?? "");
  const [tenantId, setTenantId] = useState(request.tenant_id ?? "");
  const [keyId, setKeyId] = useState(request.api_key_id ?? "");
  const [tier, setTier] = useState(request.service_tier ?? "");
  const active = [request.model_id, request.provider_key_id, request.tenant_id, request.api_key_id, request.service_tier].filter(Boolean).length;
  const dirty = modelId !== (request.model_id ?? "") || accountId !== (request.provider_key_id ?? "") || tenantId !== (request.tenant_id ?? "") || keyId !== (request.api_key_id ?? "") || tier !== (request.service_tier ?? "");
  return <details className="direct-monitor-extra"><summary>模型与账号筛选{active > 0 ? `（${active}）` : ""}<ChevronDown size={16} aria-hidden /></summary>
    <form onSubmit={event => { event.preventDefault(); onApply({ model_id: modelId.trim(), provider_key_id: accountId.trim(), tenant_id: tenantId.trim(), api_key_id: keyId.trim(), service_tier: tier.trim() }); }}>
      <label><span>直连模型 ID</span><input value={modelId} onChange={event => setModelId(event.target.value)} autoComplete="off" /></label>
      <label><span>上游账号 ID</span><input value={accountId} onChange={event => setAccountId(event.target.value)} autoComplete="off" /></label>
      <label><span>实际服务档位</span><input value={tier} onChange={event => setTier(event.target.value)} placeholder="例如 default、fast" autoComplete="off" /></label>
      <label><span>组织 ID</span><input value={tenantId} onChange={event => setTenantId(event.target.value)} autoComplete="off" /></label>
      <label><span>密钥 ID</span><input value={keyId} onChange={event => setKeyId(event.target.value)} autoComplete="off" /></label>
      <Button type="submit" kind="secondary" disabled={!dirty}><Check size={16} aria-hidden />应用筛选</Button>
    </form>
  </details>;
}

/** 按实际服务目标读取整段时间的尝试指标，展示筛选不会改变指标分母。 */
export function DirectModelMonitoring() {
  const [query, update] = useHashQuery();
  const id = useId();
  const [initial] = useState(() => { const to = new Date(); return { from: new Date(to.getTime() - DEFAULT_RANGE_MS).toISOString(), to: to.toISOString() }; });
  const from = query.get("from") ?? initial.from;
  const to = query.get("to") ?? initial.to;
  const rawBucket = query.get("bucket") ?? "hour";
  const bucket: MetricsBucket = rawBucket === "minute" || rawBucket === "day" ? rawBucket : "hour";
  const rawOutcome = query.get("outcome") ?? "";
  const outcome = rawOutcome === "succeeded" || rawOutcome === "failed" || rawOutcome === "unknown" ? rawOutcome : undefined;
  const request: DirectModelMetricsQuery = { from, to, bucket, model_id: query.get("model_id") || undefined, provider_key_id: query.get("provider_key_id") || undefined, tenant_id: query.get("tenant_id") || undefined, api_key_id: query.get("api_key_id") || undefined, service_tier: query.get("service_tier") || undefined, outcome };
  const requestKey = JSON.stringify(request);
  const rangeError = metricsRangeError(from, to, bucket) ?? (!["minute", "hour", "day"].includes(rawBucket) ? "链接中的时间粒度无效，请重新选择。" : rawOutcome && !outcome ? "链接中的结果筛选无效，请重新选择。" : null);
  const [revision, setRevision] = useState(0);
  const loadKey = `${requestKey}/${revision}`;
  const [state, setState] = useState<RequestState | null>(null);
  const [search, setSearch] = useState("");
  const [order, setOrder] = useState("name");
  const shown = state?.key === loadKey ? state : null;
  const data = rangeError ? null : shown?.data ?? null;
  const error = rangeError ?? shown?.error ?? null;
  const busy = !error && !data;
  useEffect(() => {
    let live = true;
    setState({ key: loadKey, data: null, error: null });
    if (!rangeError) fetchDirectModelMetrics(request).then(data => { if (live) setState({ key: loadKey, data, error: null }); }, error => { if (live) setState({ key: loadKey, data: null, error: errorText(error) }); });
    return () => { live = false; };
  }, [requestKey, revision, rangeError]);
  const models = useMemo(() => {
    const needle = search.trim().toLocaleLowerCase();
    return [...(data?.models ?? [])].filter(model => !needle || [model.model.name, model.model.id, model.provider_key.name].some(value => value.toLocaleLowerCase().includes(needle))).sort((left, right) => {
      const a = order === "failure" ? left.failure_rate : order === "latency" ? left.upstream_ttft_ms.p95 : order === "tps" ? left.throughput_tps.avg : null;
      const b = order === "failure" ? right.failure_rate : order === "latency" ? right.upstream_ttft_ms.p95 : order === "tps" ? right.throughput_tps.avg : null;
      return a === null && b !== null ? 1 : a !== null && b === null ? -1 : a !== null && b !== null && a !== b ? b - a : left.model.name.localeCompare(right.model.name);
    });
  }, [data, search, order]);
  const table = useRecordTable({ items: models, resetKey: `${requestKey}/${search}/${order}`, initialPageSize: 10 });
  const totals = data?.totals;
  const dateValue = (value: string) => Number.isFinite(Date.parse(value)) ? beijingInputValue(new Date(value)) : value;
  const changeDate = (field: "from" | "to", value: string) => update({ from, to, [field]: parseBeijingInput(value) ?? value });
  const change = (patch: Record<string, string>) => update({ from, to, ...patch });
  const trends = [
    { title: "上游失败率", points: data?.series.map(period => ({ label: metricPeriodLabel(period.period_start, bucket), value: period.failure_rate, displayValue: formatRatio(period.failure_rate) })) ?? [], format: (value: number) => formatRatio(value) },
    { title: "首 token 延迟 P95", points: data?.series.map(period => ({ label: metricPeriodLabel(period.period_start, bucket), value: period.first_token_latency_ms.p95, displayValue: formatDuration(period.first_token_latency_ms.p95) })) ?? [], format: formatDuration },
  ];
  return <div className="direct-model-monitoring">
    <PageHeader title="上游监测" description="按实际直连模型统计上游尝试；包含失败重试，区分上游首帧与客户端首 token 延迟。" actions={<Button kind="quiet" iconOnly aria-label="刷新上游监测" title="刷新上游监测" disabled={busy} onClick={() => setRevision(value => value + 1)}><RefreshCw size={16} aria-hidden /></Button>} />
    <Card className="direct-monitor-scope" aria-label="上游监测范围">
      <label><span>开始时间</span><input type="datetime-local" step="1" value={dateValue(from)} onChange={event => changeDate("from", event.target.value)} /></label>
      <label><span>结束时间</span><input type="datetime-local" step="1" value={dateValue(to)} onChange={event => changeDate("to", event.target.value)} /></label>
      <label><span>时间粒度</span><Select value={rawBucket} onValueChange={selectedValue => change({ bucket: selectedValue })}><option value="minute">每分钟</option><option value="hour">每小时</option><option value="day">每天</option></Select></label>
      <label><span>尝试结果</span><Select value={rawOutcome} onValueChange={selectedValue => change({ outcome: selectedValue })}><option value="">全部结果</option><option value="succeeded">成功</option><option value="failed">失败</option><option value="unknown">结果未知</option></Select></label>
      <ModelScopeFilters key={requestKey} request={request} onApply={change} />
    </Card>
    {error ? <div className="note crit" role="alert">{error}{!rangeError && <Button kind="quiet" onClick={() => setRevision(value => value + 1)}>重试</Button>}</div> : busy ? <p role="status" className="direct-monitor-state">正在读取上游监测</p> : null}
    <div className="direct-monitor-kpis" aria-label="上游监测概览" aria-busy={busy || undefined}>
      <section><h2>上游尝试</h2><strong>{formatCount(totals?.attempts)}</strong><p>已排除 {formatCount(totals?.excluded_attempts)} · 派发未知 {formatCount(totals?.unmeasured_attempts)}</p></section>
      <section><h2>失败率</h2><strong>{formatRatio(totals?.failure_rate)}</strong><p>失败 {formatCount(totals?.failed_attempts)} · 成功 {formatCount(totals?.succeeded_attempts)} · 未知 {formatCount(totals?.unknown_outcome_attempts)}</p></section>
      <section><h2>首 token 延迟 · P50</h2><strong>{formatDuration(totals?.first_token_latency_ms.p50)}</strong><p>实测 {formatCount(totals?.first_token_latency_ms.sample_count)} · 上游首帧 P50 {formatDuration(totals?.upstream_ttft_ms.p50)}</p></section>
      <section><h2>缓存 token 命中率</h2><strong>{formatRatio(totals?.tokens.cache_hit_rate)}</strong><p>命中 {formatCount(totals?.tokens.cache_measured_hit_tokens)} / 已采样输入 {formatCount(totals?.tokens.cache_measured_input_tokens)} · 未报告 {formatCount(totals?.tokens.cache_missing_attempts)}</p></section>
      <section><h2 title={OUTPUT_THROUGHPUT_NOTE}>输出 TPS</h2><strong>{formatTps(totals?.throughput_tps.avg)}</strong><p>实测 {formatCount(totals?.throughput_tps.sample_count)} · 缺失 {formatCount(totals?.throughput_tps.missing_count)}</p></section>
    </div>
    {data && <p className="direct-monitor-window">{fmtBeijing(data.from)} — {fmtBeijing(data.to)}（UTC+8，结束时间不含）</p>}
    <div className="direct-monitor-trends">{trends.map(trend => <Card key={trend.title}><CardHeader title={trend.title} /><TrendChart accessibleLabel={trend.title} points={trend.points} formatValue={trend.format} emptyLabel="当前范围没有实测数据" /></Card>)}</div>
    <Card className="direct-monitor-catalogue" aria-labelledby={`${id}-models`}>
      <div className="direct-monitor-list-toolbar"><h2 id={`${id}-models`}>直连模型<span>{formatCount(data ? models.length : null)} / {formatCount(data?.models.length)}</span></h2>
        <label className="direct-monitor-search"><Search size={16} aria-hidden /><span className="vh">搜索监测模型</span><input type="search" placeholder="搜索已加载模型或账号" value={search} onChange={event => setSearch(event.target.value)} /></label>
        <label><span className="vh">监测模型排序</span><Select aria-label="监测模型排序" value={order} onValueChange={selectedValue => setOrder(selectedValue)}><option value="name">按模型名称</option><option value="failure">失败率从高到低</option><option value="latency">首帧 P95 从高到低</option><option value="tps">输出 TPS 从高到低</option></Select></label>
      </div>
      <p className="direct-monitor-list-note">搜索与排序只调整模型列表；概览和趋势仍使用完整筛选范围。</p>
      {data && models.length === 0 ? <EmptyState text={search ? "没有匹配的模型，请调整搜索。" : "当前范围没有直连模型，请调整筛选。"} /> : <DataTable label="直连模型监测" className="direct-monitor-table"><thead><tr><th>模型 / 上游账号</th><th className="num">上游尝试</th><th className="num">失败率</th><th className="num">首 token 延迟 · P50</th><th className="num">缓存 token 命中率</th><th className="num" title={OUTPUT_THROUGHPUT_NOTE}>输出 TPS</th><th className="right">操作</th></tr></thead><tbody>{table.rows.map(model => <ModelRow key={`${model.model.id}/${model.provider_key.id ?? ""}`} model={model} />)}</tbody></DataTable>}
      {data && <RecordTablePager label="监测模型分页" page={table.page} pageSize={table.pageSize} total={table.filteredCount} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} />}
    </Card>
    <details className="direct-monitor-method"><summary>统计口径<ChevronDown size={16} aria-hidden /></summary><div>
      <p><ArrowUpRight size={16} aria-hidden />按实际直连目标和上游账号归属上游尝试，包含失败重试；与按最终请求统计的门户成功率不同。</p>
      <p>排除记录包括响应缓存命中、本地拒绝、后台任务的受理，以及查询、取消等对象管理请求；后台任务在最终结果确定后计入上游尝试，避免重复统计。</p>
      <p><ArrowDownRight size={16} aria-hidden />失败率分母只包含已知成功与失败；无流量或未知结果不会当作成功。</p>
      <p>上游首帧记录首个流式帧，可能只是元数据；客户端首 token 记录首次交付生成内容。两者不是同一时钟，不能直接相减；缺失记录保留为未知。</p>
      <p>缓存 token 命中率仅计算明确上报缓存计数的尝试，采用命中 token / 同批样本输入；区分输入内的缓存子集与额外缓存读写。未报告缓存计数的尝试单独展示，不按未命中计算。</p>
      <p>输出 TPS = 输出 Token（含推理，不含输入）÷ 完整上游耗时（毫秒换算为秒）。汇总使用有效输出 Token 合计 ÷ 有效完整上游耗时合计（毫秒换算为秒）；P50 / P95 使用同一单次完整上游耗时口径。不要求流式或 TTFT；无有效采样显示 —。含网关估算用量的记录不纳入实测输出 TPS，计入缺失；标记无法区分仅输入估算，因此也可能排除输出已由上游报告的记录。调用记录保留 ≈ 参考速度。</p>
    </div></details>
  </div>;
}
