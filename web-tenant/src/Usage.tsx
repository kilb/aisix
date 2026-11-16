import { Select } from "./ui/Select";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { ChevronRight } from "lucide-react";
import {
  ApiError,
  keys as keyApi,
  models as modelApi,
  usageSummary,
  type PortalModel,
  type SessionMember,
  type TenantKey,
  type UsageSummaryRow,
} from "./lib/api";
import { formulaCalls as usageCalls, type FormulaCall as Call } from "./lib/formulaApi";
import { discountedRate, feeMicroCny } from "./lib/formula";
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtCnyPerMillion, fmtMicroCny, perThousandOfPerMillion } from "./lib/money";
import { beijingRange, defaultCalendarRange, fmtBeijing, periodLabel, todayBeijing } from "./lib/time";
import { filterRecords, useRecordTable, type RecordPageSize } from "./lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "./ui/RecordTable";
import { Button } from "./ui/Button";
import { DataTable } from "./ui/DataTable";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { BentoGrid, Card, CardHeader, CardMessage, ChartSkeleton, PageHeader, span } from "./components/page";
import { currentQueryResult, type QueryResult } from "./lib/queryResult";
import { useHashQuery } from "./lib/useHashQuery";
import { usageRange, usageDateValue, usageRequestKey, metricViewOf } from "./lib/usageNavigation";
import { ApiMetricsPanel } from "./components/ApiMetricsPanel";
import { UsagePerformance } from "./components/UsagePerformance";
import { UpstreamTimingDetails } from "./components/UpstreamTimingDetails";
import { UsageIoAuditDetails, UsageTokenEstimate } from "./components/UsageIoAuditDetails";
import { UsageRecordFilters } from "./components/UsageRecordFilters";
import { usageRecordFilterParams, usageRecordFilterValues } from "./lib/usageRecordFilters";
import { formatCount } from "./lib/chartFormat";
import { CallLogDrawer } from "./CallLogDrawer";
import "./UsageAnalytics.css";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** What each billed quantity is called; one not listed shows its raw name. */
const DIMENSION_LABEL: Record<string, string> = {
  input: "输入",
  output: "输出",
  cache_read: "缓存读",
  cache_write: "缓存写",
  cache_write_1h: "1 小时缓存写",
  reasoning: "推理",
  audio_input: "音频输入",
  audio_output: "音频输出",
  image_input: "图片输入",
  image_output: "图片输出",
  web_search: "联网搜索",
};
const DIMENSION_ORDER = Object.keys(DIMENSION_LABEL);

const dimensionLabel = (d: string) => DIMENSION_LABEL[d] ?? d;

/** The quantities billed per call rather than per token: their price reads per thousand calls. */
const PER_CALL: ReadonlySet<string> = new Set(["web_search"]);

/**
 * A line's unit price, `perMillion` as the API sends it (¥ per million
 * units): a token's as it is, under the column's 元/百万, and a call's, such
 * as a web search, per thousand with its own unit, `¥72.00 / 千次`, exact.
 */
function unitPrice(dimension: string, perMillion: string): string {
  return PER_CALL.has(dimension) ? `${fmtCnyPerMillion(perThousandOfPerMillion(perMillion))} / 千次` : fmtCnyPerMillion(perMillion);
}

/** 租户可见的失败原因；未知类型使用通用说明。 */
const ERROR_LABEL: Record<string, string> = {
  model_not_found: "模型不存在",
  client_disconnected: "客户端断开连接",
  invalid_request: "请求无效",
  rate_limited: "请求过于频繁",
  budget_exceeded: "额度已用尽",
  content_filtered: "内容被拦截",
  upstream_unavailable: "上游暂不可用",
  failed: "失败",
};

/** A signed amount for a correction: `+¥0.002` or `-¥0.002`. */
function signed(micro: string): string {
  return BigInt(micro) >= 0n ? `+${fmtMicroCny(micro)}` : fmtMicroCny(micro);
}

/** Basis points as the share taken off, in integer arithmetic: 1000 is `10%`, 1250 is `12.5%`. */
function percentOf(bps: number): string {
  const fraction = String(bps % 100).padStart(2, "0").replace(/0+$/, "");
  return `${Math.floor(bps / 100)}${fraction ? `.${fraction}` : ""}%`;
}

/** What a call's 费用 cell says. */
function chargeText(c: Call): string {
  const { charge } = c;
  switch (charge.status) {
    case "charged": {
      const amount = charge.calculation.final_micro_cny;
      const adjusted = charge.calculation.adjusted_micro_cny !== "0" ? "（含调整）" : "";
      return `${amount === null ? "金额未知" : fmtMicroCny(amount)}${adjusted}${charge.estimated ? "（估算）" : ""}`;
    }
    case "exempt":
      return c.cache_hit ? "不计费（缓存命中）" : "不计费";
    case "nothing_to_bill":
      return "不计费";
    case "pending":
      return "待计价";
    default:
      return charge.status;
  }
}

function CallStatusLamp({ call }: { call: Call }) {
  if (call.status === "succeeded") return <Lamp state="applied">成功</Lamp>;
  if (call.status === "retried") return <Lamp state="pending">已重试</Lamp>;
  if (call.status === "unknown") return <Lamp state="pending">结果未知</Lamp>;
  if (call.status !== "failed") return <Lamp state="pending">{call.status}</Lamp>;
  const reason = call.error_class === null ? "失败" : (ERROR_LABEL[call.error_class] ?? "调用未完成");
  return <Lamp state="rejected">{`失败（${reason}）`}</Lamp>;
}

/** 从保存的计价行复算；不请求目录现价，不将未知金额当作免费。 */
function FeeCalculation({ call }: { call: Call }) {
  const { charge } = call;
  const f = charge.calculation;
  const result = charge.lines === null ? null : feeMicroCny(charge.lines, charge.discount_bps);
  const positive = f.final_micro_cny !== null && BigInt(f.final_micro_cny) > 0n;
  return (
    <section aria-label="费用计算" className="grid gap-2">
      <h3>费用计算</h3>
      {call.status === "failed" && (
        <dl><div><dt>失败原因</dt><dd>{ERROR_LABEL[call.error_class ?? ""] ?? "调用未完成，请凭请求 ID 联系管理员"}</dd></div></dl>
      )}
      {call.status === "failed" && charge.status === "charged" && positive && (
        <>
          <p>调用未完成，但已产生用量，按已产生用量计费</p>

        </>
      )}
      {charge.estimated && <p className="hint">含估算用量：上游未完整返回用量，部分 token 由网关估算，费用按该用量计费。</p>}
      {call.status === "retried" && <p className="hint">这里展示本次尝试的费用，同一请求的其他计费尝试单独记录。</p>}
      {f.final_micro_cny === null ? <p>待计价，最终金额未知。</p> : (
        <>
          <dl className="grid gap-x-6 gap-y-1 sm:grid-cols-3">
            <div><dt>原始费用</dt><dd>{f.original_micro_cny === null ? "原调用待计价，未形成原始费用" : fmtMicroCny(f.original_micro_cny)}</dd></div>
            <div><dt>累计调整</dt><dd>{signed(f.adjusted_micro_cny)}</dd></div>
            <div><dt>最终费用</dt><dd>{fmtMicroCny(f.final_micro_cny)}{charge.estimated ? "（估算）" : ""}</dd></div>
          </dl>
          <p className="tabular break-words">
            {f.original_micro_cny === null ? "0（未形成原始费用）" : fmtMicroCny(f.original_micro_cny)} {signed(f.adjusted_micro_cny)} = {fmtMicroCny(f.final_micro_cny)}
          </p>
          {result === null ? <p className="hint">历史明细缺失或计量、折扣不支持精确复算，无法复算。</p> : (
            <>
              <p className="break-words">
                按本次最终计价明细复算：（{charge.lines?.length ? charge.lines.map((line) =>
                  `${dimensionLabel(line.dimension)} ${line.quantity} × ${line.rate_cny} 元/百万${PER_CALL.has(line.dimension) ? "次" : "token"}`,
                ).join(" + ") : "无可计费项，0"}）× {percentOf(10000 - (charge.discount_bps ?? 0))}，整笔向上取整到微元 = {fmtMicroCny(result)}
              </p>
              {result !== f.final_micro_cny && <p className="note crit" role="alert">明细复算与记录的最终费用不一致，请凭请求 ID 联系管理员。</p>}
            </>
          )}
        </>
      )}
      <p className="hint">首 token 延迟从请求接收到首次交付文本、推理或工具内容；无内容、非流式和旧记录未采集时显示 —。模型处理耗时为本次模型请求开始至结束，包含网络传输，不是整次请求总耗时。</p>
      {call.latency_ms !== null && <p className="hint">响应延迟（旧采集口径）：{call.latency_ms} ms；不同调用协议的计时终点可能不同。</p>}
      <UsageIoAuditDetails data={call} />
    </section>
  );
}

/** 实际记录用量与按字节估算分开，缓存与推理子项不在列表重复相加。 */
function CallTokens({ call }: { call: Call }) {
  return <dl className="tenant-call-tokens" aria-label="记录 Token 消耗">
    <div><dt>输入</dt><dd>{formatCount(call.usage_input_tokens)}{call.usage_input_tokens == null ? "" : " Token"}</dd></div>
    <div><dt>输出</dt><dd>{formatCount(call.usage?.completion_tokens)}{call.usage?.completion_tokens == null ? "" : " Token"}</dd></div>
  </dl>;
}

function CallLogRow({ call: c, onDetails }: { call: Call; onDetails: (trigger: HTMLButtonElement) => void }) {
  return <tr>
    <td className="usage-row-action">
      <Button kind="quiet" size="sm" aria-haspopup="dialog" aria-label="查看调用详情" title="查看调用详情" className="tenant-icon-action" onClick={event => onDetails(event.currentTarget)}>
        <ChevronRight aria-hidden="true" />
      </Button>
    </td>
    <td className="tabular usage-call-time" data-label="时间"><time dateTime={c.occurred_at}><span>{fmtBeijing(c.occurred_at).split(" ")[0]}</span><span>{fmtBeijing(c.occurred_at).split(" ")[1] ?? "—"}</span></time></td>
    <td data-label="模型 / 请求" className="usage-request-identity">
      <strong>{c.model.name || "—"}</strong>
      <span className="cell-note">密钥 {c.api_key.label || "—"} · 成员 {c.member ? (c.member.display_name ?? "成员") : "—"}</span>
      <span className="cell-note">{c.request_id ? <code>{c.request_id}</code> : "—"}</span>
    </td>
    <td data-label="状态">
      <CallStatusLamp call={c} />
      <span className="cell-note">分组 {c.charge.group ? c.charge.group.display_name : "—"}</span>
    </td>
    <td className="num" data-label="响应性能"><UsagePerformance data={c} /></td>
    <td className="num tenant-call-usage" data-label="Token 消耗">
      <CallTokens call={c} />
      {c.usage_estimated === true && <span className="cell-note">含网关估算用量</span>}
      <UsageTokenEstimate data={c} />
    </td>
    <td className="num" data-label="费用">{chargeText(c)}</td>
  </tr>;
}

/** 同一条记录的详情按小标题分区，保留历史费用公式与完整计价行。 */
function CallLogDetails({ call: c }: { call: Call }) {
  return <>
    <section className="tenant-call-section" aria-label="请求信息">
      <h3>请求信息</h3>
      <dl className="tenant-call-facts">
        <div><dt>请求 ID</dt><dd><code>{c.request_id || "—"}</code></dd></div>
        <div><dt>调用时间</dt><dd>{fmtBeijing(c.occurred_at)}</dd></div>
        <div><dt>模型</dt><dd>{c.model.name || "—"}</dd></div>
        <div><dt>状态</dt><dd><CallStatusLamp call={c} /></dd></div>
        <div><dt>密钥</dt><dd>{c.api_key.label || "—"}</dd></div>
        <div><dt>成员</dt><dd>{c.member?.display_name ?? (c.member ? "成员" : "—")}</dd></div>
        <div><dt>计价分组</dt><dd>{c.charge.group?.display_name ?? "—"}</dd></div>
        <div><dt>费用</dt><dd>{chargeText(c)}</dd></div>
      </dl>
    </section>
    <section className="tenant-call-section" aria-label="响应性能">
      <h3>响应性能</h3>
      <UsagePerformance data={c} />
      <p className="hint">首字仅显示实测值；输出 TPS 为输出 Token（含推理，不含输入）÷ 完整上游耗时。含估算用量时标为 ≈，仅供参考；用量标记无法区分输入或输出估算。无有效指标或旧记录未采集时显示 —，不以其它指标代替。</p>
    </section>
    <UpstreamTimingDetails data={c.upstream_timing} />
    <section className="tenant-call-section" aria-label="Token 消耗">
      <h3>Token 消耗</h3>
      <CallTokens call={c} />
      <p className="hint">输入与输出均为已记录用量，未记录的用量显示 —。{c.usage_estimated === true ? "本次含网关估算用量，费用按该用量计费。" : ""}</p>
      <UsageTokenEstimate data={c} />
    </section>
    <div className="tenant-call-section"><FeeCalculation call={c} /></div>
    <section className="tenant-call-section" aria-label="计费明细">
      <h3>计费明细</h3>
      {c.charge.lines === null ? <p className="hint">这次调用的金额由一次未记录费率明细的重新定价更正，费率明细不可知。</p>
        : c.charge.lines.length === 0 ? <p className="hint">{c.charge.status === "pending" ? "尚未计价，计费项未知" : "这次调用没有计费项"}</p>
        : <DataTable label="计费明细" compact>
          <thead><tr><th>计量</th><th className="num">数量</th><th className="num">折扣前单价（元/百万）</th><th className="num">折后单价（元/百万）</th></tr></thead>
          <tbody>{c.charge.lines.map(line => <tr key={line.dimension}>
            <td data-label="计量">{dimensionLabel(line.dimension)}</td>
            <td className="num" data-label="数量">{line.quantity}</td>
            <td className="num" data-label="折扣前单价（元/百万）">{line.rate_cny === null ? "—" : unitPrice(line.dimension, line.rate_cny)}</td>
            <td className="num" data-label="折后单价（元/百万）">{(() => {
              const rate = line.rate_cny === null ? null : discountedRate(line.rate_cny, c.charge.discount_bps);
              return rate === null ? "—" : unitPrice(line.dimension, rate);
            })()}</td>
          </tr>)}</tbody>
        </DataTable>}
      {(c.charge.discount_bps ?? 0) > 0 && <p className="hint">本次按组织折扣计费：在折扣前的价格上减免 {percentOf(c.charge.discount_bps ?? 0)}。</p>}
    </section>
  </>;
}

/** The summary's span when no date is chosen: the last 30 days by day, the last 12 months by month. */
function defaultRange(group: "day" | "month"): { from: string; to: string } {
  return defaultCalendarRange(group);
}

/** The longest span the control plane summarises: 92 days by day, 731 days by month. */
const MAX_DAYS = { day: 92, month: 731 } as const;

/** A card that is a region named by its heading. */
function Section({ title, number, children, className = "" }: { title: string; number: number; children: ReactNode; className?: string }) {
  return (
    <Card padding="table" role="region" aria-label={title} className={`${span.d12} usage-record-section ${className}`}>
      <CardHeader number={number} title={title} />
      {children}
    </Card>
  );
}

/** The filters above a list: labelled controls on one wrapping row. */
function Filters({ children }: { children: ReactNode }) {
  return <div className="usage-filters">{children}</div>;
}

/** The key filter's options: every key an owner or admin sees, a member's own. A billing member holds none. */
function KeySelect({ keys, value, onChange }: { keys: TenantKey[]; value: string; onChange: (v: string) => void }) {
  return (
    <Field label="密钥">
      <Select value={value} onValueChange={(selectedValue) => onChange(selectedValue)}>
        <option value="">全部</option>
        {value && !keys.some((key) => key.id === value) && <option value={value}>已选密钥（{value}，当前列表未提供）</option>}
        {keys.map((k) => (
          <option key={k.id} value={k.id}>
            {k.label}
          </option>
        ))}
      </Select>
    </Field>
  );
}

/**
 * 「用量」: what the organisation's keys used, summed by period and model,
 * and every call one by one. A member sees only the calls of their own
 * keys, which the control plane decides. Nothing here names a channel: a
 * call's model is the name its caller sent.
 */
export function Usage({ member, page = "analysis" }: { member: SessionMember; page?: "analysis" | "calls" }) {
  const [keys, setKeys] = useState<TenantKey[] | null>(member.role === "billing" ? [] : null);
  const [granted, setGranted] = useState<PortalModel[]>([]);
  const [readErr, setReadErr] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const failed = (e: unknown) => live && setReadErr(failure(e));
    // A billing member holds no key, so has no key to filter by.
    if (member.role !== "billing") keyApi.list().then((k) => live && setKeys(k), failed);
    modelApi.list().then((m) => live && setGranted(m), failed);
    return () => {
      live = false;
    };
  }, [member.role]);

  const keyList = keys ?? [];
  const showKeys = member.role !== "billing";
  return (
    <div className="measurement-page">
      <PageHeader title={page === "calls" ? "调用记录" : "用量分析"} dataLead={member.role === "member" ? "查看你的密钥范围；每次计费尝试单独记录。" : "查看全组织用量与费用；每次计费尝试单独记录。"} />
      <div className="flex min-w-0 flex-col gap-4">
        {readErr && (
          <div className="note crit" role="alert">
            {readErr}
          </div>
        )}
        {page === "analysis" ? <Summary keys={keyList} showKeys={showKeys} ownScope={member.role === "member"} models={granted} /> : <CallLog keys={keyList} showKeys={showKeys} models={granted} />}
      </div>
    </div>
  );
}

// ── usage summary (用量汇总) ────────────────────────────────────────────────────────────────

function Quantities({ q }: { q: Record<string, number> | null }) {
  // Unknown, not zero: the latest correction to this charge recorded no detail,
  // so what it billed cannot be shown. Say so rather than implying nothing was
  // billed.
  if (q === null) return <>明细不可知</>;
  const names = Object.keys(q).sort((a, b) => {
    const ia = DIMENSION_ORDER.indexOf(a);
    const ib = DIMENSION_ORDER.indexOf(b);
    return (ia < 0 ? DIMENSION_ORDER.length : ia) - (ib < 0 ? DIMENSION_ORDER.length : ib) || a.localeCompare(b);
  });
  if (names.length === 0) return <>—</>;
  return (
    <span className="usage-qty">
      {names.map((d) => (
        <span key={d}>
          {dimensionLabel(d)} {q[d]}
        </span>
      ))}
    </span>
  );
}

/** A summary row's model: the `other` row that holds the rest reads 其他. */
function summaryModel(r: UsageSummaryRow): string {
  if (r.model.id === null && r.model.name === "other") return "其他";
  return r.model.name === "" ? "未授权或未知模型" : r.model.name;
}

function Summary({ keys, showKeys, ownScope, models }: { keys: TenantKey[]; showKeys: boolean; ownScope: boolean; models: PortalModel[] }) {
  const [query, update] = useHashQuery();
  const group = query.get("group") === "month" ? "month" : "day";
  const setGroup = (value: string) => update({ group: value });
  const fromDate = usageDateValue(query.get("from"));
  const toDate = usageDateValue(query.get("to"), true);
  const keyId = query.get("api_key_id") ?? "";
  const setFromDate = (value: string) => update({ from: value ? beijingRange(value, value).from : undefined });
  const setToDate = (value: string) => update({ to: value ? beijingRange(value, value).to : undefined });
  const setKeyId = (value: string) => update({ api_key_id: value, from: range?.from ?? defaultRange(group).from, to: range?.to ?? defaultRange(group).to });
  const [result, setResult] = useState<QueryResult<UsageSummaryRow> | null>(null);
  const [refresh, setRefresh] = useState(0);
  const range = usageRange(query);
  // A summary needs both ends: a date left empty reads as the default span.
  const fallback = defaultRange(group);
  const rangeDays = range ? (Date.parse(range.to ?? fallback.to) - Date.parse(range.from ?? fallback.from)) / 86_400_000 : 0;
  const tooLong = rangeDays > MAX_DAYS[group];
  const request = { group, from: range?.from ?? fallback.from, to: range?.to ?? fallback.to, ...(keyId ? { api_key_id: keyId } : {}) } as const;
  const requestKey = usageRequestKey(query, { ...request, refresh, tooLong });
  const shown = currentQueryResult(result, requestKey);
  const rows = shown?.rows ?? null;
  const err = shown?.error ?? null;
  const activeKey = useRef(requestKey);
  activeKey.current = requestKey;
  // The latest request: an answer to an older filter is dropped.
  const ticket = useRef(0);

  useEffect(() => {
    const mine = ++ticket.current;
    if (!range || tooLong) {
      setResult({ key: requestKey, rows: [], next: null, error: null });
      return;
    }
    setResult({ key: requestKey, rows: null, next: null, error: null });
    usageSummary(request).then(
      (items) => {
        if (mine !== ticket.current || activeKey.current !== requestKey) return;
        setResult({ key: requestKey, rows: [...items].reverse(), next: null, error: null });
      },
      (e: unknown) => {
        if (mine !== ticket.current || activeKey.current !== requestKey) return;
        setResult({ key: requestKey, rows: [], next: null, error: e instanceof ApiError ? formMessageFor(e) : String(e) });
      },
    );
    return () => { ticket.current += 1; };
  }, [requestKey]);

  const searchQuery = query.get("q") ?? "";
  const table = useRecordTable({
    items: filterRecords(rows ?? [], searchQuery, row => [summaryModel(row)]),
    resetKey: `${requestKey}/${searchQuery}`,
  });

  const spanText =
    fromDate === "" && toDate === ""
      ? group === "day"
        ? "最近 30 天"
        : "最近 12 个月"
      : `${fromDate || periodLabel(fallback.from, "day")} – ${toDate || todayBeijing()}`;

  const visualError = !range ? "结束日期不能早于开始日期，请调整汇总筛选。" : tooLong ? "时间范围过大，请缩短汇总区间。" : err;
  const rangeLabel = `${spanText} · ${keyId ? "所选密钥" : ownScope ? "你的密钥" : "全组织"}`;
  // 无效日期仍保留指标卡框，由指标组件解释范围错误并停止请求。
  const metricsFrom = range?.from ?? (query.get("from") || fallback.from);
  const metricsTo = range?.to ?? (query.get("to") || fallback.to);
  return (
    <BentoGrid className="usage-analysis-grid">
      <Card className="usage-analysis-scope metric-scope" padding="sm" aria-label="用量分析筛选">
        <div className="usage-analysis-filters">
          <Field label="开始日期"><input type="date" value={fromDate} onChange={e => setFromDate(e.target.value)} /></Field>
          <Field label="结束日期"><input type="date" value={toDate} onChange={e => setToDate(e.target.value)} /></Field>
          {showKeys && <KeySelect keys={keys} value={keyId} onChange={setKeyId} />}
          <Field label="模型"><Select value={query.get("model_id") ?? ""} onValueChange={selectedValue => update({ model_id: selectedValue, from: metricsFrom, to: metricsTo })}><option value="">全部</option>{query.get("model_id") && !models.some(model => model.id === query.get("model_id")) && <option value={query.get("model_id")!}>已选模型（{query.get("model_id")}，当前列表未提供）</option>}{models.map(model => <option key={model.id} value={model.id}>{model.name}</option>)}</Select></Field>
          <div className="usage-analysis-actions">
            <Button onClick={() => setRefresh(value => value + 1)}>刷新分析</Button>
            <a className="metric-scope-action" href={`#calls?${new URLSearchParams({ ...Object.fromEntries(query), from: metricsFrom, to: metricsTo })}`}>查看调用记录</a>
          </div>
        </div>
        <div className="usage-analysis-range">
          <p>{fmtBeijing(metricsFrom)} — {fmtBeijing(metricsTo)}（UTC+8）</p>
          <p>分析最多查询 92 天或 1500 个时间桶；按月汇总可查询原有历史范围。</p>
        </div>
      </Card>
      <ApiMetricsPanel refreshKey={refresh} scope="tenant" enabled={range !== null} from={metricsFrom} to={metricsTo} apiKeyId={keyId || undefined} modelId={query.get("model_id") || undefined} memberId={query.get("member_id") || undefined} view={metricViewOf(query.get("view"))} onViewChange={view => update({ view, from: metricsFrom, to: metricsTo })} numberStart={1} className={span.d12} />
      <Section title="用量汇总" number={7}>
        <p className="hint">{rangeLabel}</p>
        {visualError && <p className="note crit" role="alert">{visualError}</p>}
        {(query.has("model_id") || query.has("member_id")) && <p className="note warn">下方计费汇总仅按时间和密钥筛选，不应用分析区的模型或成员筛选。</p>}
        <RecordTableToolbar search={{ value: searchQuery, onChange: value => update({ q: value }), label: "搜索汇总模型", placeholder: "模型名称" }}>
          <Field label="汇总方式">
            <Select value={group} onValueChange={(selectedValue) => setGroup(selectedValue as "day" | "month")}>
              <option value="day">按天</option>
              <option value="month">按月</option>
            </Select>
          </Field>
        </RecordTableToolbar>
        <details className="usage-summary-notes">
          <summary>计费与汇总口径</summary>
          <p>含估算用量：上游未完整返回用量，部分 token 由网关估算，费用按该用量计费。</p>
          <p>
            按用量发生的时间汇总，不是按计费入账的时间：重新计价后的调整，记在用量发生的那一期。
            调用尝试次数包含最终尝试，以及已计费或重新计价的重试。
            {fromDate === "" && toDate === "" && (group === "day" ? "未选日期时汇总最近 30 天。" : "未选日期时汇总最近 12 个月。")}
          </p>
        </details>
        {rows === null && <ChartSkeleton />}
        {rows !== null && rows.length === 0 && !err && !tooLong && range && <CardMessage>所选范围内没有用量</CardMessage>}
        {rows !== null && rows.length > 0 && table.filteredCount === 0 && <CardMessage>没有匹配的汇总模型，请调整搜索条件。</CardMessage>}
        {rows !== null && table.filteredCount > 0 && (
          <DataTable label="用量汇总" tableClassName="usage-summary-table">
            <colgroup><col className="usage-summary-period" /><col className="usage-summary-model" /><col className="usage-summary-count" /><col className="usage-summary-cost" /><col className="usage-summary-quantities" /></colgroup>
            <thead>
              <tr>
                <th>期间</th>
                <th>模型</th>
                <th className="num">调用次数</th>
                <th className="num">费用</th>
                <th>计量</th>
              </tr>
            </thead>
            <tbody>
              {table.rows.map((r) => (
                <tr key={`${r.period_start}/${r.model.id ?? r.model.name}`}>
                  <td className="tabular" data-label="期间">{periodLabel(r.period_start, group)}</td>
                  <td data-label="模型">{summaryModel(r)}</td>
                  <td className="num" data-label="调用次数">{r.requests}</td>
                  <td className="num" data-label="费用">
                    {Number.isSafeInteger(r.charged_micro_cny) ? fmtMicroCny(r.charged_micro_cny) : "未知"}
                    {r.estimated_requests > 0 ? "（含估算）" : ""}
                  </td>
                  <td data-label="计量">
                    <Quantities q={r.quantities} />
                  </td>
                </tr>
              ))}
            </tbody>
          </DataTable>
        )}
        {rows !== null && <RecordTablePager page={table.page} pageSize={table.pageSize} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} total={table.filteredCount} label="用量汇总" />}
      </Section>
    </BentoGrid>
  );
}

// ── call log (调用记录) ────────────────────────────────────────────────────────────────

/** How long typing in 请求 ID settles before the log is read again. */
const TYPING_MS = 300;

function CallLog({ keys, showKeys, models }: { keys: TenantKey[]; showKeys: boolean; models: PortalModel[] }) {
  const [query, update] = useHashQuery();
  const fromDate = usageDateValue(query.get("from"));
  const toDate = usageDateValue(query.get("to"), true);
  const keyId = query.get("api_key_id") ?? "";
  const setFromDate = (value: string) => update({ from: value ? beijingRange(value, value).from : undefined });
  const setToDate = (value: string) => update({ to: value ? beijingRange(value, value).to : undefined });
  const setKeyId = (value: string) => update({ api_key_id: value });
  const modelId = query.get("model_id") ?? "";
  const setModelId = (value: string) => update({ model_id: value });
  const [typed, setTyped] = useState(() => query.get("request_id") ?? "");
  const requestId = query.get("request_id") ?? "";
  const [requestEditing, setRequestEditing] = useState(false);
  useEffect(() => { setTyped(requestId); setRequestEditing(false); }, [requestId]);
  const [result, setResult] = useState<QueryResult<Call> | null>(null);
  const [pageSize, setPageSize] = useState<RecordPageSize>(25);
  const [more, setMore] = useState(false);
  const morePending = useRef(false);
  // 旧状态深链映射到服务端结果筛选，首次操作写入新的标准参数。
  const recordFilters = { ...usageRecordFilterValues(query), outcome: query.get("outcome") ?? query.get("status") ?? "" };
  const range = usageRange(query);
  const request = {
    ...range,
    limit: pageSize,
    ...(keyId ? { api_key_id: keyId } : {}),
    ...(modelId ? { model_id: modelId } : {}),
    ...(requestId ? { request_id: requestId } : {}),
    ...(query.get("member_id") ? { member_id: query.get("member_id")! } : {}),
    ...usageRecordFilterParams(recordFilters, "tenant"),
  };
  const requestKey = usageRequestKey(query, request);
  const shown = currentQueryResult(result, requestKey);
  const rows = shown?.rows ?? null;
  const next = shown?.next ?? null;
  const err = shown?.error ?? null;
  const activeKey = useRef(requestKey);
  activeKey.current = requestKey;
  const searchQuery = query.get("q") ?? "";
  const table = useRecordTable({
    items: filterRecords(rows ?? [], searchQuery,
      call => [call.model.name, call.api_key.label, call.api_key.prefix, call.member?.display_name ?? "", call.request_id ?? ""]),
    resetKey: `${requestKey}/${searchQuery}`,
  });
  const loadedSearch = useRef("");
  loadedSearch.current = searchQuery;
  const [selected, setSelected] = useState<Call | null>(null);
  const detailTrigger = useRef<HTMLButtonElement>(null);
  const ticket = useRef(0);

  useEffect(() => {
    if (!requestEditing) return;
    const t = window.setTimeout(() => update({ request_id: typed.trim() }), TYPING_MS);
    return () => window.clearTimeout(t);
  }, [typed, requestEditing, update]);

  useEffect(() => {
    const mine = ++ticket.current;
    morePending.current = false;
    setMore(false);
    setSelected(null);
    if (!range) {
      setResult({ key: requestKey, rows: [], next: null, error: null });
      return;
    }
    setResult({ key: requestKey, rows: null, next: null, error: null });
    usageCalls(request).then(
      (p) => {
        if (mine !== ticket.current || activeKey.current !== requestKey) return;
        setResult({ key: requestKey, rows: p.items, next: p.next_before, error: null });
      },
      (e: unknown) => {
        if (mine !== ticket.current || activeKey.current !== requestKey) return;
        setResult({ key: requestKey, rows: [], next: null, error: e instanceof ApiError ? formMessageFor(e) : String(e) });
      },
    );
    return () => { ticket.current += 1; };
  }, [requestKey]);

  async function loadMore() {
    if (!range || !shown || next === null || morePending.current) return;
    const mine = ticket.current;
    const searchAtLoad = loadedSearch.current;
    morePending.current = true;
    setMore(true);
    try {
      const p = await usageCalls({ ...request, before: next });
      if (mine !== ticket.current || activeKey.current !== requestKey) return;
      setResult(current => {
        if (current?.key !== requestKey) return current;
        const seen = new Set((current.rows ?? []).map(r => r.event_id));
        return { ...current, rows: [...(current.rows ?? []), ...p.items.filter(r => !seen.has(r.event_id))], next: p.next_before, error: null };
      });
      if (loadedSearch.current === searchAtLoad) {
        table.setPage(table.filteredCount >= table.page * table.pageSize ? table.page + 1 : table.page);
      }
    } catch (e) {
      if (mine === ticket.current && activeKey.current === requestKey) setResult(current => current?.key === requestKey ? { ...current, error: failure(e) } : current);
    } finally {
      if (mine === ticket.current && activeKey.current === requestKey) {
        morePending.current = false;
        setMore(false);
      }
    }
  }

  return (
    <Section title="调用记录" number={8} className="usage-calls-section">
      <UsageRecordFilters scope="tenant" value={recordFilters} onChange={patch => update({ outcome: recordFilters.outcome, ...patch, status: undefined })}>
        <Field label="时间起" error={range ? null : "结束日期不能早于开始日期"}>
          <input type="date" value={fromDate} max={toDate || undefined} onChange={(e) => setFromDate(e.target.value)} />
        </Field>
        <Field label="时间止">
          <input type="date" value={toDate} min={fromDate || undefined} onChange={(e) => setToDate(e.target.value)} />
        </Field>
        {showKeys && <KeySelect keys={keys} value={keyId} onChange={setKeyId} />}
        <Field label="模型">
          <Select value={modelId} onValueChange={(selectedValue) => setModelId(selectedValue)}>
            <option value="">全部</option>
            {modelId && !models.some((model) => model.id === modelId) && <option value={modelId}>已选模型（{modelId}，当前列表未提供）</option>}
            {models.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </Select>
        </Field>
        <Field label="请求 ID">
          <input autoComplete="off" value={typed} onChange={(e) => { setTyped(e.target.value); setRequestEditing(true); }} />
        </Field>
      </UsageRecordFilters>
      <RecordTableToolbar search={{ value: searchQuery, onChange: value => update({ q: value }), label: "搜索已加载调用", placeholder: "模型、密钥、成员或请求 ID" }} />
      {query.get("member_id") && <p className="hint" role="status">已按成员 {query.get("member_id")} 筛选。</p>}
      <p className="hint">文字搜索仅筛选已加载记录；调用结果与更多筛选作用于全部匹配记录。</p>
      <p className="hint">首字仅显示实测值；输出 TPS 为输出 Token（含推理，不含输入）÷ 完整上游耗时。含估算用量时标为 ≈，仅供参考；用量标记无法区分输入或输出估算。无有效指标或旧记录未采集时显示 —，不以其它指标代替。</p>
      <p className="mt-4 text-caption text-fg-3">调用记录按计费尝试逐条展示，同一请求可能有多条记录。{rows !== null ? `已加载 ${rows.length.toLocaleString("en-US")} 条${next !== null ? "，还有更多记录" : ""}；这些记录不代表整个汇总窗口的成功率或延迟分布。` : ""}</p>
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
      {rows === null && <ChartSkeleton />}
      {rows !== null && rows.length === 0 && !err && <CardMessage>所选条件下没有调用记录</CardMessage>}
      {rows !== null && rows.length > 0 && (
        <DataTable label="调用记录" className="tenant-call-table">
          <colgroup><col className="usage-column-action" /><col className="usage-column-time" /><col className="usage-column-request" /><col className="usage-column-state" /><col className="usage-column-timing" /><col className="usage-column-tokens" /><col className="usage-column-charge" /></colgroup>
          <thead>
            <tr>
              <th>
                <span className="vh">明细</span>
              </th>
              <th>时间</th>
              <th>模型 / 请求</th>
              <th>状态</th>
              <th className="num">响应性能</th>
              <th className="num">Token 消耗</th>
              <th className="num">费用</th>
            </tr>
          </thead>
          <tbody>
            {table.rows.map(call => <CallLogRow key={call.event_id} call={call} onDetails={trigger => {
              detailTrigger.current = trigger;
              setSelected(call);
            }} />)}
          </tbody>
        </DataTable>
      )}
      <RecordTablePager
        page={table.page} pageSize={table.pageSize}
        onPageChange={(page) => {
          if (page <= table.pageCount) table.setPage(page);
          else void loadMore();
        }}
        onPageSizeChange={value => { setPageSize(value); table.setPageSize(value); }}
        loadedCount={rows?.length} hasNext={rows !== null && (table.page < table.pageCount || next !== null)}
        busy={more} label="调用记录"
      />

      <CallLogDrawer call={selected} onOpenChange={open => { if (!open) setSelected(null); }} returnFocusRef={detailTrigger}>
        {selected && <CallLogDetails call={selected} />}
      </CallLogDrawer>

    </Section>
  );
}
