import { useRecordTable } from "../../lib/recordTable";
import { RecordPager } from "../../components/RecordTools";
import { Fragment, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { charges, tenants as tenantsApi, type AdminKey, type Charge, type ChargeFilter, type Tenant } from "../../lib/api";
import { callDetailHash, callOutcomeLabel } from "../../lib/callLog";
import { chargeReason } from "../../lib/chargeReason";
import { errorText } from "../../lib/fmt";
import { fmtMicroCny } from "../../lib/money";
import { beijingInputValue, fmtBeijing, parseBeijingInput } from "../../lib/time";
import { modelText, reasonLabel } from "../../lib/pricingStatus";
import { chargeFromLines, dimensionLabel, lineUsd, rateText, reportedCounters, usageSummary } from "../../lib/chargeUsage";
import { usdRateText } from "../../lib/rateUnits";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { formatCount } from "../../lib/chartFormat";
import { useHashQuery } from "../../lib/useHashQuery";
import { currentQueryResult, type QueryResult } from "../../lib/queryResult";
import { chargesQuery } from "../../lib/analysisNavigation";
import { cn } from "../../lib/cn";
import { ChevronDown, FileText, RotateCcw } from "lucide-react";
import { UsagePerformance } from "../../components/UsagePerformance";
import { UsageIoAuditDetails, UsageTokenEstimate } from "../../components/UsageIoAuditDetails";
import { UsageRecordFilters } from "../../components/UsageRecordFilters";
import { usageRecordFilterValues } from "../../lib/usageRecordFilters";
import "./Charges.css";
import "../../components/CallRecordSearch.css";
import { Select } from "../../ui/Select";

const STATUSES: [string, string][] = [
  ["charged", "已计价"],
  ["exempt", "免计费"],
  ["nothing_to_bill", "无需计费"],
  ["unpriced", "未计价"],
];

const statusLabel = (s: string) => STATUSES.find(([v]) => v === s)?.[1] ?? s;

/**
 * 同一请求由服务端合并并分页，列表汇总请求费用，保留代表尝试的性能。
 * 行内复核仍使用该次尝试保存的计价行与费率，完整尝试链在日志详情展示。
 * 计价状态的深链接保留状态、原因与模型筛选。
 * 时间、调用结果与身份条件均由服务端在分页前筛选；重计价后的记录仍然可见。
 */
export function Charges(_props: TabProps) {
  const [query, update] = useHashQuery();
  const queryKey = query.toString();
  const filter: ChargeFilter = chargesQuery(query);
  const filterKey = JSON.stringify(filter);
  const [eventDraft, setEventDraft] = useState(() => query.get("event_id") ?? "");
  useEffect(() => setEventDraft(query.get("event_id") ?? ""), [queryKey]);
  const [result, setResult] = useState<QueryResult<Charge> | null>(null);
  const shown = currentQueryResult(result, filterKey);
  const rows = shown?.rows ?? null;
  const next = shown?.next ?? null;
  const error = shown?.error ?? null;
  const resultsLoading = rows === null && !error;
  const resultCard = useRef<HTMLElement>(null);
  const [resultHeight, setResultHeight] = useState(0);
  // 只记已完成结果卡的几何高度；加载仍清空旧行，深滚动不因占位变短而被夹回。
  useLayoutEffect(() => {
    const card = resultCard.current;
    if (resultsLoading || !card) return;
    const measure = () => setResultHeight(card.getBoundingClientRect().height);
    const observer = new ResizeObserver(measure);
    observer.observe(card);
    measure();
    return () => observer.disconnect();
  }, [resultsLoading]);
  const activeKey = useRef(filterKey);
  activeKey.current = filterKey;
  const [loadingMore, setLoadingMore] = useState(false);
  const [tenants, setTenants] = useState<Tenant[]>([]);
  const [keys, setKeys] = useState<AdminKey[]>([]);
  // The charges whose check row is open, by seq.
  const [open, setOpen] = useState<Set<number>>(() => new Set());
  const generation = useRef(0);
  const table = useRecordTable({ items: rows ?? [], searchText: (c) => [c.request_id ?? "", c.event_id, c.model_name ?? "", c.requested_model_id, c.tenant_id], resetKey: JSON.stringify(filter) });

  const loadFirst = useCallback(async (f: ChargeFilter) => {
    const g = ++generation.current;
    const requestedKey = JSON.stringify(f);
    setResult({ key: requestedKey, rows: null, next: null, error: null });
    // A new first page has no cursor yet: the one on screen belongs to the
    // filter it was read for, so a 加载更多 clicked while this loads would
    // otherwise ask for this filter with the previous filter's cursor.
    setLoadingMore(false);
    try {
      const page = await charges.list({ ...f, group_requests: true, limit: 50 });
      if (g !== generation.current || activeKey.current !== requestedKey) return;
      setResult({ key: requestedKey, rows: page.items, next: page.next_before, error: null });
    } catch (e) {
      if (g === generation.current && activeKey.current === requestedKey) setResult({ key: requestedKey, rows: null, next: null, error: errorText(e) });
    }
  }, []);

  useEffect(() => {
    void loadFirst(filter);
    return () => { generation.current += 1; };
  }, [filterKey, loadFirst]);

  useEffect(() => {
    tenantsApi.list().then(setTenants, () => undefined);
  }, []);

  // A key belongs to one tenant, so the key filter follows the tenant filter.
  useEffect(() => {
    setKeys([]);
    if (!filter.tenant_id) return;
    const live = { on: true };
    tenantsApi.keys(filter.tenant_id).then((k) => live.on && setKeys(k), () => undefined);
    return () => {
      live.on = false;
    };
  }, [filter.tenant_id]);

  async function loadMore() {
    // A continuation extends the page on screen; without one (its filter is
    // still loading, or it failed) there is nothing to extend.
    if (!shown || next === null || rows === null || loadingMore) return;
    const requestedKey = filterKey;
    const g = generation.current;
    setLoadingMore(true);
    setResult(current => current?.key === requestedKey ? { ...current, error: null } : current);
    try {
      const page = await charges.list({ ...filter, group_requests: true, before: next, limit: 50 });
      if (g !== generation.current || activeKey.current !== requestedKey) return;
      setResult(current => current?.key === requestedKey ? { ...current, rows: [...(current.rows ?? []), ...page.items], next: page.next_before, error: null } : current);
    } catch (e) {
      if (g === generation.current && activeKey.current === requestedKey) setResult(current => current?.key === requestedKey ? { ...current, error: errorText(e) } : current);
    } finally {
      // Only this generation's own continuation ends: one that a filter
      // change has already superseded must not unlock the new one.
      if (g === generation.current) setLoadingMore(false);
    }
  }

  const set = (patch: ChargeFilter) => update(patch as Record<string, string | undefined>);
  const tenantName = (id: string) => tenants.find((t) => t.id === id)?.name ?? id;
  // The model and reason a link from 计价状态 set: the form has no field for them, so they are shown here.
  const linked = [
    filter.error_code ? `原因：${reasonLabel(filter.error_code)}` : null,
    filter.model_id ? `模型：${rows?.find((c) => c.requested_model_id === filter.model_id)?.model_name ?? filter.model_id}` : null,
    filter.model_id === "" ? "模型：未记录" : null,
    filter.group_id !== undefined ? `分组：${filter.group_id || "未记录"}` : null,
    filter.pricing_tier !== undefined ? `档位：${filter.pricing_tier || "未报告"}` : null,
  ].filter((x): x is string => x !== null);

  return (
    <>
      <PageHeader
        title="调用记录"
        eyebrow="CALLS · 请求日志"
        dataLead="同一请求合并一行，优先展示最终成功结果。费用汇总所有实际计费尝试，详情保留完整故障转移与重试链。"
        description="售价与成本以人民币计；未知金额显示 —。缺少有效追踪信息的旧记录无法可靠合并，保留单条展示。"
        actions={
          <Button className="charge-icon-button" aria-label="刷新" title="刷新" type="button" icon="refresh" onClick={() => void loadFirst(filter)}></Button>
        }
      />
      <div className="call-record-layout charge-call-layout" data-loading={resultsLoading}>
        <section className="charge-filter-controls call-filter-card sticky-filters" aria-label="调用记录筛选">
        <UsageRecordFilters scope="admin" value={usageRecordFilterValues(query)} onChange={patch => update(patch)} advancedChildren={<ChargeScopeFilters key={JSON.stringify([filter.from, filter.to, filter.member_id, filter.provider_key_id, filter.request_id, filter.model_id])} filter={filter} onChange={set} />}>
          <label><span>状态</span>
            <Select size="compact" value={filter.status ?? ""} onValueChange={value => set({ status: value || undefined })}>
              <option value="">全部</option>{STATUSES.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
            </Select>
          </label>
          <label><span>租户</span>
            <Select size="compact" value={filter.tenant_id ?? ""} onValueChange={value => set({ tenant_id: value || undefined, api_key_id: undefined })}>
              <option value="">全部租户</option>
              {filter.tenant_id && !tenants.some(tenant => tenant.id === filter.tenant_id) && <option value={filter.tenant_id}>已选租户（{filter.tenant_id}，当前列表未提供）</option>}
              {tenants.map(tenant => <option key={tenant.id} value={tenant.id}>{tenant.name}</option>)}
            </Select>
          </label>
          <label><span>密钥</span>
            <Select size="compact" value={filter.api_key_id ?? ""} disabled={!filter.tenant_id && !filter.api_key_id} onValueChange={value => set({ api_key_id: value || undefined })}>
              <option value="">全部密钥</option>
              {filter.api_key_id && !keys.some(key => key.id === filter.api_key_id) && <option value={filter.api_key_id}>已选密钥（{filter.api_key_id}，当前列表未提供）</option>}
              {keys.map(key => <option key={key.id} value={key.id}>{key.label} · {key.prefix}</option>)}
            </Select>
          </label>
          <form className="charge-event-filter" onSubmit={event => { event.preventDefault(); set({ event_id: eventDraft.trim() || undefined }); }}>
            <label><span>事件</span><input value={eventDraft} onChange={event => setEventDraft(event.target.value)} placeholder="事件编号" /></label>
            <Button type="submit" size="sm">查找</Button>
          </form>
          <label className="call-record-search"><span>搜索已加载调用</span>
            <input type="search" autoComplete="off" value={table.query} onChange={event => table.setQuery(event.target.value)} placeholder="在已加载记录中搜索" />
          </label>
        </UsageRecordFilters>
        {(filter.from || filter.to || filter.member_id || filter.provider_key_id || filter.request_id || filter.exclude_status) && <p className="charge-scope-summary" role="status">当前已应用范围条件：{[filter.exclude_status === "nothing_to_bill" ? "隐藏无需计费" : null, filter.from ? "开始 " + fmtBeijing(filter.from) : null, filter.to ? "结束 " + fmtBeijing(filter.to) + "（不含）" : null, filter.member_id ? "成员 " + filter.member_id : null, filter.provider_key_id ? "上游账号 " + filter.provider_key_id : null, filter.request_id ? "请求 " + filter.request_id : null].filter(Boolean).join("；")}</p>}
        {linked.length > 0 && (
          <div className="charge-linked-scope" role="status">
            <span className="hint">只看 {linked.join("，")}{filter.priced_since && filter.priced_before ? `（计价时间 ${fmtBeijing(filter.priced_since)} 至 ${fmtBeijing(filter.priced_before)}，结束时间不含在内${filter.exclude_corrected === "true" ? "；排除已更正，后续状态变化会改变结果" : ""}）` : "（按上述筛选查询，也含已通过重计价更正的调用）"}</span>
            <Button type="button" size="sm" className="ml-auto" onClick={() => set({ error_code: undefined, model_id: undefined, group_id: undefined, pricing_tier: undefined, priced_since: undefined, priced_before: undefined, exclude_corrected: undefined })}>
              清除
            </Button>
          </div>
        )}
        </section>
      <Card ref={resultCard} style={{ minHeight: resultsLoading ? resultHeight : undefined }} className="charge-workspace call-table-card" padding="table" role="region" aria-label="调用计费记录">
        <CardHeader
          number={1}
          title="调用计费记录"
          description={rows === null ? (error ? "当前筛选结果读取失败，请重试。" : "正在读取当前筛选结果。") : `当前筛选已加载 ${formatCount(rows.length)} 条${next === null ? "，已到记录末尾。" : "，可继续加载。"}`}
        />
        {error && (
          <div className="note crit" role="alert">
            {error}
          </div>
        )}
        {rows === null && !error && <Loading />}
        {rows !== null && rows.length === 0 && <EmptyState text="没有符合条件的计费记录。" />}
        {rows !== null && rows.length > 0 && (
          <>
            {table.filteredCount === 0 && <p className="hint" role="status">没有匹配的已加载记录；可继续读取更早记录。</p>}
            <DataTable label="计费明细" className="charge-records" tableClassName="charge-records-table">
              <colgroup><col className="charge-col-time" /><col className="charge-col-type" /><col className="charge-col-identity" /><col className="charge-col-status" /><col className="charge-col-money" /><col className="charge-col-service" /><col className="charge-col-performance" /><col className="charge-col-tokens" /><col className="charge-col-action" /></colgroup>
              <thead>
                <tr>
                  <th>时间</th>
                  <th>请求类型</th>
                  <th>调用 / 租户</th>
                  <th>计价结果</th>
                  <th className="num">费用（元）</th>
                  <th>服务 / 档位</th>
                  <th>响应性能</th>
                  <th>Token 消耗</th>
                  <th className="right">
                    <span className="vh">日志 / 复核</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {table.rows.map((c) => {
                  const shown = open.has(c.seq);
                  return (
                    <Fragment key={c.seq}>
                      <tr data-finance-record>
                        <td data-label="时间" className="charge-call-time tabular"><time dateTime={c.occurred_at}><span>{fmtBeijing(c.occurred_at).split(" ")[0]}</span><span>{fmtBeijing(c.occurred_at).split(" ")[1] ?? "—"}</span></time></td>
                        <td data-label="请求类型">{c.stream === true ? "流" : c.stream === false ? "非流" : "未知"}</td>
                        <td data-label="调用 / 租户"><span className="charge-model-name">{modelText(c.model_name, c.model_deleted)}</span>
                          <span className="cell-note">{tenantName(c.tenant_id)}</span>
                          <span className="cell-note break-all"><code>{c.request_id || "—"}</code></span>
                        </td>
                        <td data-label="计价结果"><span className="call-outcome">{callOutcomeLabel(c)}</span><span className="cell-note">{statusLabel(c.status)} · {chargeReason(c)}</span>
                          {c.request_group && <>
                            {c.request_group.failover_count > 0 && <span className="cell-note">故障转移 {c.request_group.failover_count} 次</span>}
                            {c.request_group.retry_count > 0 && <span className="cell-note">同目标重试 {c.request_group.retry_count} 次</span>}
                            {!c.request_group.complete && <span className="cell-note">尚无最终结果记录</span>}
                          </>}
                          <span className="cell-note">HTTP {c.status_code ?? "—"}{c.error_class ? ` · ${c.error_class}` : ""}</span>
                          {c.error_message && <span className="call-error-summary">{c.error_message}</span>}
                        </td>
                        <td data-label="费用（元）" className="num"><span className="finance-amount-label">售价</span><span className="finance-amount-value">{c.request_group ? c.request_group.charge_micro_cny === null ? "—" : fmtMicroCny(c.request_group.charge_micro_cny) : c.charge_micro_cny === null ? "—" : fmtMicroCny(c.charge_micro_cny)}</span>
                          <span className="cell-note">成本 {c.request_group ? c.request_group.cost_micro_cny == null ? "—" : fmtMicroCny(c.request_group.cost_micro_cny) : c.cost_micro_cny === null ? "—" : fmtMicroCny(c.cost_micro_cny)}</span>
                        </td>
                        <td data-label="服务 / 档位">
                          <Upstream c={c} />
                          <span className="cell-note">档位 {c.service_tier ?? "—"}</span>
                        </td>
                        <td data-label="响应性能"><UsagePerformance data={c} /></td>
                        <td data-label="Token 消耗" className="tabular text-caption"><dl className="charge-call-tokens" aria-label="记录 Token 消耗" title={usageSummary(c.usage) ?? undefined}>
                          <div><dt>输入</dt><dd>{c.usage_input_tokens == null ? "—" : formatCount(c.usage_input_tokens)}</dd></div>
                          <div><dt>输出</dt><dd>{c.usage?.completion_tokens == null ? "—" : formatCount(c.usage.completion_tokens)}</dd></div>
                        </dl><UsageTokenEstimate data={c} /></td>
                        <td data-label="日志 / 复核" className="right">
                          <a className="call-log-link" href={callDetailHash(query, c.event_id)} aria-label={`查看调用日志 ${c.event_id}`} title="查看调用日志"><FileText aria-hidden /></a>
                          <Button
                            kind="quiet"
                            size="sm"
                            aria-expanded={shown}
                            aria-controls={`charge-check-${c.seq}`}
                            aria-label={`${shown ? "收起" : "展开"}用量明细 ${c.event_id}`}
                            title={shown ? "收起用量明细" : "展开用量明细"}
                            className="charge-icon-button"
                            onClick={() => {
                              setOpen((prev) => {
                                const next = new Set(prev);
                                if (next.has(c.seq)) next.delete(c.seq);
                                else next.add(c.seq);
                                return next;
                              });
                              if (!shown) requestAnimationFrame(() => {
                                // 展开后先让明细起点可见，横向位置仍由局部滚动区管理。
                                document.getElementById(`charge-check-${c.seq}`)?.querySelector("h4")?.scrollIntoView({ block: "nearest", inline: "nearest", behavior: "instant" });
                              });
                            }}
                          >
                            <ChevronDown aria-hidden style={{ transform: shown ? "rotate(180deg)" : undefined }} />
                          </Button>
                        </td>
                      </tr>
                      {shown && (
                        <tr className="dt-detail" id={`charge-check-${c.seq}`}>
                          <td colSpan={9}>
                            <ChargeCheck c={c} />
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </DataTable>
            <RecordPager table={table} label="计费明细" resetKey={JSON.stringify(filter)} hasMore={next !== null} onMore={loadMore} busy={loadingMore} />
          </>
        )}

      </Card>
      </div>
    </>
  );
}

/** 日期按北京时间录入；相关条件一次应用，避免输入半成品时改变查询范围。 */
function ChargeScopeFilters({ filter, onChange }: { filter: ChargeFilter; onChange: (patch: ChargeFilter) => void }) {
  const dateValue = (value: string | undefined) => value && Number.isFinite(Date.parse(value)) ? beijingInputValue(new Date(value)) : value ?? "";
  const [draft, setDraft] = useState({ from: dateValue(filter.from), to: dateValue(filter.to), member_id: filter.member_id ?? "", provider_key_id: filter.provider_key_id ?? "", request_id: filter.request_id ?? "", model_id: filter.model_id ?? "" });
  const [error, setError] = useState("");
  const active = Boolean(filter.from || filter.to || filter.member_id || filter.provider_key_id || filter.request_id || filter.model_id);
  return <section className="charge-scope-filters" aria-label="时间与归属筛选">
    <h3 className="text-caption font-medium">时间与归属筛选{active ? " · 已应用" : ""}</h3>
    <label className="charge-hide-unbilled"><input type="checkbox" checked={filter.exclude_status === "nothing_to_bill"} onChange={event => onChange({ exclude_status: event.target.checked ? "nothing_to_bill" : undefined })} /><span>隐藏无需计费</span></label>
    <form className="mt-4 grid min-w-0 gap-4 sm:grid-cols-2 xl:grid-cols-3" onSubmit={event => {
      event.preventDefault();
      // 未修改的时间保留原始精度，改变成员等条件不能截断深链接的毫秒。
      const from = draft.from === dateValue(filter.from) ? filter.from : draft.from ? parseBeijingInput(draft.from) : undefined;
      const to = draft.to === dateValue(filter.to) ? filter.to : draft.to ? parseBeijingInput(draft.to) : undefined;
      if (from === null || to === null || from && !Number.isFinite(Date.parse(from)) || to && !Number.isFinite(Date.parse(to)) || from && to && Date.parse(to) <= Date.parse(from)) { setError("请输入有效时间，结束时间必须晚于开始时间。"); return; }
      setError("");
      onChange({ from, to, member_id: draft.member_id.trim() || undefined, provider_key_id: draft.provider_key_id.trim() || undefined, request_id: draft.request_id.trim() || undefined, model_id: draft.model_id.trim() || undefined });
    }}>
      <label className="f min-w-0"><span>开始时间（北京时间）</span><input type="datetime-local" step="1" className="min-w-0 w-full" value={draft.from} onChange={event => setDraft({ ...draft, from: event.target.value })} /></label>
      <label className="f min-w-0"><span>结束时间（不含，北京时间）</span><input type="datetime-local" step="1" className="min-w-0 w-full" value={draft.to} onChange={event => setDraft({ ...draft, to: event.target.value })} /></label>
      <label className="f min-w-0"><span>模型 ID</span><input className="min-w-0 w-full" autoComplete="off" placeholder="准确的请求模型 ID" value={draft.model_id} onChange={event => setDraft({ ...draft, model_id: event.target.value })} /></label>
      <label className="f min-w-0"><span>成员 ID</span><input className="min-w-0 w-full" autoComplete="off" value={draft.member_id} onChange={event => setDraft({ ...draft, member_id: event.target.value })} /></label>
      <label className="f min-w-0"><span>上游账号 ID</span><input className="min-w-0 w-full" autoComplete="off" value={draft.provider_key_id} onChange={event => setDraft({ ...draft, provider_key_id: event.target.value })} /></label>
      <label className="f min-w-0"><span>请求 ID</span><input className="min-w-0 w-full" autoComplete="off" value={draft.request_id} onChange={event => setDraft({ ...draft, request_id: event.target.value })} /></label>
      {error && <p role="alert" className="note crit sm:col-span-2 xl:col-span-3">{error}</p>}
      <div className="flex flex-wrap items-center gap-3 sm:col-span-2 xl:col-span-3">
        <Button type="submit">应用时间与归属筛选</Button>
        {active && <Button type="button" kind="quiet" iconOnly aria-label="清除时间与归属筛选" title="清除时间与归属筛选" onClick={() => onChange({ from: undefined, to: undefined, member_id: undefined, provider_key_id: undefined, request_id: undefined, model_id: undefined })}><RotateCcw size={16} aria-hidden /></Button>}
        <span className="hint">与调用结果、计价状态及租户条件组合查询。</span>
      </div>
    </form>
  </section>;
}

/** The upstream account that served the call, and the model it was asked for. */
function Upstream({ c }: { c: Charge }) {
  if (!c.upstream_account_id) return <span className="text-muted-foreground">—</span>;
  const name = c.upstream_account_name ?? c.upstream_account_id.slice(0, 8);
  return (
    <span className="block leading-tight">
      <span className="font-medium">
        {name}
        {c.upstream_account_deleted && "（已删除）"}
      </span>
      {c.upstream_model && <span className="cell-note font-mono">{c.upstream_model}</span>}
    </span>
  );
}

/**
 * The check row sits inside the list's `.dt` table, whose cell rules reach
 * every descendant cell; these utilities give its own table compact cells.
 */
const CELL = "static h-auto border-b border-border bg-transparent px-2 py-1.5 text-meta font-medium text-muted-foreground first:pl-0 last:pr-0";
const ROW = "h-auto border-b border-border px-2 py-1.5 text-caption tabular-nums first:pl-0 last:pr-0";

/**
 * What checks one charge: the lines it billed (quantity × rate = USD), the
 * counters the upstream reported, and the formula with its group rate and
 * discount, recomputed beside the amount billed.
 */
export function ChargeCheck({ c }: { c: Charge }) {
  const reported = reportedCounters(c.usage);
  const recomputed = chargeFromLines(c.lines, c.cny_per_usd_e4, c.discount_bps);
  return (
    <div className="charge-check text-ink-soft">
      <section className="min-w-0" aria-label={`计费数量 ${c.event_id}`}>
        <h4 className="mb-2 text-caption font-medium">计费数量</h4>
        {c.lines === null ? (
          <p className="text-caption text-muted-foreground">
            这次调用的金额由一次未记录费率明细的重新定价更正，费率明细不可知。
          </p>
        ) : c.lines.length === 0 ? (
          <p className="text-caption text-muted-foreground">这次调用没有计费的数量。</p>
        ) : (
          <div className="charge-quantities-scroll" role="region" aria-label="计费数量明细表格滚动区域" tabIndex={0}>
          <table className="charge-quantities-table w-full text-caption">
            <thead className="text-muted-foreground">
              <tr>
                <th className={cn(CELL, "text-left")}>维度</th>
                <th className={cn(CELL, "text-right")}>数量</th>
                <th className={cn(CELL, "text-right")}>基准价</th>
                <th className={cn(CELL, "text-right")}>小计（美元）</th>
              </tr>
            </thead>
            <tbody>
              {c.lines.map((l) => (
                <tr data-finance-record key={l.dimension}>
                  <td data-label="维度" className={ROW}>{dimensionLabel(l.dimension)}</td>
                  <td data-label="数量" className={cn(ROW, "text-right")}>{l.quantity.toLocaleString("en-US")}</td>
                  <td data-label="基准价" className={cn(ROW, "text-right")}>{usdRateText(l.dimension, l.rate)}</td>
                  <td data-label="小计（美元）" className={cn(ROW, "text-right")}>{lineUsd(l) ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
          </div>
        )}
        {c.lines !== null && c.lines.length > 0 && c.cny_per_usd_e4 !== null && (
          <p className="mt-3 text-caption text-text-2">
            售价 = 小计合计 × 分组换算率 {rateText(c.cny_per_usd_e4)}
            {(c.discount_bps ?? 0) > 0 && <> × (1 − 折扣 {((c.discount_bps ?? 0) / 100).toFixed(2).replace(/\.?0+$/, "")}%)</>}，向上取整到 0.000001 元
            {recomputed !== null && (
              <>
                {" "}= <span className="font-normal text-foreground">{fmtMicroCny(recomputed)}</span>
                {c.charge_micro_cny !== null && (
                  <span className="ml-2 text-ink">
                    {recomputed === c.charge_micro_cny ? "与实收一致" : `与实收 ${fmtMicroCny(c.charge_micro_cny)} 不一致`}
                  </span>
                )}
              </>
            )}
          </p>
        )}
        {c.adjusted_micro_cny !== null && (
          <p className="mt-1 text-caption text-text-2">重新计价已更正 {fmtMicroCny(c.adjusted_micro_cny)}。</p>
        )}
      </section>
      <section className="min-w-0" aria-label={`已记录用量 ${c.event_id}`}>
        <h4 className="mb-2 text-caption font-medium">{c.usage_estimated === true ? "包含网关估算的用量" : "已记录用量"}</h4>
        {reported.length === 0 ? (
          <p className="text-caption text-muted-foreground">这次调用没有用量记录。</p>
        ) : (
          <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-6 text-caption">
            {reported.map((r) => (
              <Fragment key={r.key}>
                <dt className="border-t border-border py-1.5 text-text-2">{r.key === "total_tokens" ? "记录总数" : r.label}</dt>
                <dd className="border-t border-border py-1.5 text-right tabular-nums">{r.value.toLocaleString("en-US")}</dd>
              </Fragment>
            ))}
          </dl>
        )}
        <p className="mt-3 text-meta text-muted-foreground">
          事件 <code>{c.event_id}</code>
        </p>
      </section>
      <UsageIoAuditDetails data={c} />
    </div>
  );
}
