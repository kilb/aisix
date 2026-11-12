import { Fragment, useEffect, useRef, useState, type ReactNode } from "react";
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
import { fmtCnyPerMillion, fmtMicroCny, fmtUtc, perThousandOfPerMillion, periodLabel, todayUtc, utcRange } from "./lib/money";
import { Button } from "./ui/Button";
import { DataTable } from "./ui/DataTable";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, ChartSkeleton, PageHeader, span } from "./components/page";
import { UsageCostCard, UsageCostTrendCard, UsageModelsCard, UsageRequestCard } from "./components/UsageAnalytics";
import { ApiMetricsPanel } from "./components/ApiMetricsPanel";

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
          {charge.estimated && <p className="hint">本次包含估算用量。</p>}
        </>
      )}
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
    </section>
  );
}

/** The UTC instants a pair of dates names, or null when the end is before the start. */
function rangeOf(fromDate: string, toDate: string): { from?: string; to?: string } | null {
  if (fromDate && toDate && toDate < fromDate) return null;
  return {
    ...(fromDate ? { from: utcRange(fromDate, fromDate).from } : {}),
    ...(toDate ? { to: utcRange(toDate, toDate).to } : {}),
  };
}

/** The summary's span when no date is chosen: the last 30 days by day, the last 12 months by month. */
function defaultRange(group: "day" | "month"): { from: string; to: string } {
  const now = new Date();
  const start =
    group === "day"
      ? new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - 29))
      : new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - 11, 1));
  return utcRange(start.toISOString().slice(0, 10), todayUtc(now));
}

/** The longest span the control plane summarises: 92 days by day, 731 days by month. */
const MAX_DAYS = { day: 92, month: 731 } as const;

/** A card that is a region named by its heading. */
function Section({ title, number, children }: { title: string; number: number; children: ReactNode }) {
  return (
    <Card role="region" aria-label={title} className={span.d12}>
      <CardHeader number={number} title={title} />
      {children}
    </Card>
  );
}

/** The filters above a list: labelled controls on one wrapping row. */
function Filters({ children }: { children: ReactNode }) {
  return <div className="usage-filters flex flex-wrap items-start gap-x-3 gap-y-4 [&>.field]:w-full sm:[&>.field]:w-48">{children}</div>;
}

/** The key filter's options: every key an owner or admin sees, a member's own. A billing member holds none. */
function KeySelect({ keys, value, onChange }: { keys: TenantKey[]; value: string; onChange: (v: string) => void }) {
  return (
    <Field label="密钥">
      <select value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="">全部</option>
        {keys.map((k) => (
          <option key={k.id} value={k.id}>
            {k.label}
          </option>
        ))}
      </select>
    </Field>
  );
}

/**
 * 「用量」: what the organisation's keys used, summed by period and model,
 * and every call one by one. A member sees only the calls of their own
 * keys, which the control plane decides. Nothing here names a channel: a
 * call's model is the name its caller sent.
 */
export function Usage({ member }: { member: SessionMember }) {
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
    <>
      <PageHeader title="用量" eyebrow="USAGE · 调用与费用" dataLead={member.role === "member" ? "查看你的密钥用量、模型费用与每次计费尝试。汇总与调用记录可分别筛选。" : "查看全组织用量、模型费用与每次计费尝试。汇总与调用记录可分别筛选。"} />
      <div className="flex min-w-0 flex-col gap-3">
        {readErr && (
          <div className="note crit" role="alert">
            {readErr}
          </div>
        )}
        <Summary keys={keyList} showKeys={showKeys} ownScope={member.role === "member"} />
        <CallLog keys={keyList} showKeys={showKeys} models={granted} />
      </div>
    </>
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

function Summary({ keys, showKeys, ownScope }: { keys: TenantKey[]; showKeys: boolean; ownScope: boolean }) {
  const [group, setGroup] = useState<"day" | "month">("day");
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [keyId, setKeyId] = useState("");
  const [rows, setRows] = useState<UsageSummaryRow[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const range = rangeOf(fromDate, toDate);
  // A summary needs both ends: a date left empty reads as the default span.
  const fallback = defaultRange(group);
  const rangeDays = range ? (Date.parse(range.to ?? fallback.to) - Date.parse(range.from ?? fallback.from)) / 86_400_000 : 0;
  const tooLong = rangeDays > MAX_DAYS[group];
  // The latest request: an answer to an older filter is dropped.
  const ticket = useRef(0);

  useEffect(() => {
    const mine = ++ticket.current;
    if (!range || tooLong) {
      setRows([]);
      setErr(null);
      return;
    }
    setRows(null);
    usageSummary({ group, from: range.from ?? fallback.from, to: range.to ?? fallback.to, ...(keyId ? { api_key_id: keyId } : {}) }).then(
      (items) => {
        if (mine !== ticket.current) return;
        setRows([...items].reverse());
        setErr(null);
      },
      (e: unknown) => {
        if (mine !== ticket.current) return;
        setRows([]);
        setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
      },
    );
    // `range` is derived from the two dates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [group, fromDate, toDate, keyId, tooLong, refresh]);

  const spanText =
    fromDate === "" && toDate === ""
      ? group === "day"
        ? "最近 30 天（UTC）"
        : "最近 12 个月（UTC）"
      : `${fromDate || periodLabel(fallback.from, "day")} – ${toDate || todayUtc()}（UTC）`;

  const visualError = !range ? "结束日期不能早于开始日期，请调整汇总筛选。" : tooLong ? "时间范围过大，请缩短汇总区间。" : err;
  const rangeLabel = `${spanText} · ${keyId ? "所选密钥" : ownScope ? "你的密钥" : "全组织"}`;
  const retry = () => setRefresh((value) => value + 1);
  // 无效日期仍保留指标卡框，由指标组件解释范围错误并停止请求。
  const metricsFrom = fromDate ? utcRange(fromDate, fromDate).from : fallback.from;
  const metricsTo = toDate ? utcRange(toDate, toDate).to : fallback.to;
  return (
    <BentoGrid>
      <ApiMetricsPanel compact refreshKey={refresh} scope="tenant" enabled from={metricsFrom} to={metricsTo} apiKeyId={keyId || undefined} numberStart={1} className={span.d12} />
      <UsageRequestCard rows={rows} error={visualError} rangeLabel={rangeLabel} group={group} number={3} className={span.d8} onRetry={retry} />
      <UsageCostCard rows={rows} error={visualError} rangeLabel={rangeLabel} group={group} number={4} className={span.d4} onRetry={retry} />
      <UsageModelsCard rows={rows} error={visualError} rangeLabel={rangeLabel} group={group} number={5} className={span.d4} onRetry={retry} />
      <UsageCostTrendCard rows={rows} error={visualError} rangeLabel={rangeLabel} group={group} number={6} className={span.d8} onRetry={retry} />
      <Section title="用量汇总" number={7}>
        <Filters>
          <Field label="汇总方式">
            <select value={group} onChange={(e) => setGroup(e.target.value as "day" | "month")}>
              <option value="day">按天</option>
              <option value="month">按月</option>
            </select>
          </Field>
          <Field label="开始日期（UTC）" error={range ? null : "结束日期不能早于开始日期"}>
            <input type="date" value={fromDate} max={toDate || undefined} onChange={(e) => setFromDate(e.target.value)} />
          </Field>
          <Field label="结束日期（UTC）" error={tooLong ? `时间范围过大：按天最多 ${MAX_DAYS.day} 天，按月最多 ${MAX_DAYS.month} 天` : null}>
            <input type="date" value={toDate} min={fromDate || undefined} onChange={(e) => setToDate(e.target.value)} />
          </Field>
          {showKeys && <KeySelect keys={keys} value={keyId} onChange={setKeyId} />}
        </Filters>
        <p className="hint">
          按用量发生的时间汇总，不是按计费入账的时间：重新计价后的调整，记在用量发生的那一期。
          调用尝试次数包含最终尝试，以及已计费或重新计价的重试。
          {fromDate === "" && toDate === "" && (group === "day" ? "未选日期时汇总最近 30 天（UTC）。" : "未选日期时汇总最近 12 个月（UTC）。")}
        </p>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        {rows === null && <ChartSkeleton />}
        {rows !== null && rows.length === 0 && !err && !tooLong && <CardMessage>所选范围内没有用量</CardMessage>}
        {rows !== null && rows.length > 0 && (
          <DataTable label="用量汇总">
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
              {rows.map((r) => (
                <tr key={`${r.period_start}/${r.model.id ?? r.model.name}`}>
                  <td className="tabular">{periodLabel(r.period_start, group)}</td>
                  <td>{summaryModel(r)}</td>
                  <td className="num">{r.requests}</td>
                  <td className="num">
                    {Number.isSafeInteger(r.charged_micro_cny) ? fmtMicroCny(r.charged_micro_cny) : "未知"}
                    {r.estimated_requests > 0 ? "（含估算）" : ""}
                  </td>
                  <td>
                    <Quantities q={r.quantities} />
                  </td>
                </tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </Section>
    </BentoGrid>
  );
}

// ── call log (调用记录) ────────────────────────────────────────────────────────────────

/** How long typing in 请求 ID settles before the log is read again. */
const TYPING_MS = 300;

function CallLog({ keys, showKeys, models }: { keys: TenantKey[]; showKeys: boolean; models: PortalModel[] }) {
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [keyId, setKeyId] = useState("");
  const [modelId, setModelId] = useState("");
  const [typed, setTyped] = useState("");
  const [requestId, setRequestId] = useState("");
  const [rows, setRows] = useState<Call[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [more, setMore] = useState(false);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const range = rangeOf(fromDate, toDate);
  const ticket = useRef(0);

  useEffect(() => {
    const t = window.setTimeout(() => setRequestId(typed.trim()), TYPING_MS);
    return () => window.clearTimeout(t);
  }, [typed]);

  const filter = () => ({
    ...range,
    ...(keyId ? { api_key_id: keyId } : {}),
    ...(modelId ? { model_id: modelId } : {}),
    ...(requestId ? { request_id: requestId } : {}),
  });

  useEffect(() => {
    const mine = ++ticket.current;
    if (!range) {
      setRows([]);
      setNext(null);
      setErr(null);
      return;
    }
    setRows(null);
    usageCalls(filter()).then(
      (p) => {
        if (mine !== ticket.current) return;
        setRows(p.items);
        setNext(p.next_before);
        setErr(null);
      },
      (e: unknown) => {
        if (mine !== ticket.current) return;
        setRows([]);
        setNext(null);
        setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
      },
    );
    // `range` and `filter` derive from the state listed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fromDate, toDate, keyId, modelId, requestId]);

  async function loadMore() {
    if (next === null) return;
    const mine = ticket.current;
    setMore(true);
    try {
      const p = await usageCalls({ ...filter(), before: next });
      if (mine !== ticket.current) return;
      setRows((cur) => {
        const seen = new Set((cur ?? []).map((r) => r.event_id));
        return [...(cur ?? []), ...p.items.filter((r) => !seen.has(r.event_id))];
      });
      setNext(p.next_before);
    } catch (e) {
      setErr(failure(e));
    } finally {
      setMore(false);
    }
  }

  function toggle(id: string) {
    setOpen((cur) => {
      const n = new Set(cur);
      if (!n.delete(id)) n.add(id);
      return n;
    });
  }

  return (
    <Section title="调用记录" number={8}>
      <Filters>
        <Field label="时间（UTC）起" error={range ? null : "结束日期不能早于开始日期"}>
          <input type="date" value={fromDate} max={toDate || undefined} onChange={(e) => setFromDate(e.target.value)} />
        </Field>
        <Field label="时间（UTC）止">
          <input type="date" value={toDate} min={fromDate || undefined} onChange={(e) => setToDate(e.target.value)} />
        </Field>
        {showKeys && <KeySelect keys={keys} value={keyId} onChange={setKeyId} />}
        <Field label="模型">
          <select value={modelId} onChange={(e) => setModelId(e.target.value)}>
            <option value="">全部</option>
            {models.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </select>
        </Field>
        <Field label="请求 ID">
          <input autoComplete="off" value={typed} onChange={(e) => setTyped(e.target.value)} />
        </Field>
      </Filters>
      <p className="mt-4 text-caption text-muted">调用记录按计费尝试逐条展示，同一请求可能有多条记录。{rows !== null ? `已加载 ${rows.length.toLocaleString("en-US")} 条${next !== null ? "，还有更多记录" : ""}；这些记录不代表整个汇总窗口的成功率或延迟分布。` : ""}</p>
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
      {rows === null && <ChartSkeleton />}
      {rows !== null && rows.length === 0 && !err && <CardMessage>所选条件下没有调用记录</CardMessage>}
      {rows !== null && rows.length > 0 && (
        <DataTable label="调用记录">
          <thead>
            <tr>
              <th>
                <span className="vh">明细</span>
              </th>
              <th>时间（UTC）</th>
              <th>模型</th>
              <th>密钥</th>
              <th>成员</th>
              <th>请求 ID</th>
              <th>状态</th>
              <th>分组</th>
              <th className="num">首 token 延迟</th>
              <th className="num">模型处理耗时</th>
              <th className="num">费用</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((c) => {
              const expanded = open.has(c.event_id);
              return (
                <Fragment key={c.event_id}>
                  <tr>
                    <td>
                      <Button
                        kind="quiet"
                        size="sm"
                        aria-expanded={expanded}
                        aria-label={expanded ? "收起明细" : "展开明细"}
                        onClick={() => toggle(c.event_id)}
                      >
                        <ChevronRight aria-hidden="true" className={expanded ? "rotate-90 transition-transform" : "transition-transform"} />
                        {expanded ? "收起" : "展开"}
                      </Button>
                    </td>
                    <td className="tabular">{fmtUtc(c.occurred_at)}</td>
                    <td>{c.model.name || "—"}</td>
                    <td>{c.api_key.label || "—"}</td>
                    <td>{c.member ? (c.member.display_name ?? "成员") : "—"}</td>
                    <td>{c.request_id ? <code>{c.request_id}</code> : "—"}</td>
                    <td>
                      <CallStatusLamp call={c} />
                    </td>
                    <td>{c.charge.group ? c.charge.group.display_name : "—"}</td>
                    <td className="num">{c.first_token_latency_ms === null ? "—" : `${c.first_token_latency_ms} ms`}</td>
                    <td className="num">{c.attempt_duration_ms === null ? "—" : `${c.attempt_duration_ms} ms`}</td>
                    <td className="num">{chargeText(c)}</td>
                  </tr>
                  {expanded && (
                    <tr className="usage-detail">
                      <td colSpan={11}>
                        <FeeCalculation call={c} />
                        <h3>计费明细</h3>
                        {c.charge.lines === null ? (
                          <p className="hint">这次调用的金额由一次未记录费率明细的重新定价更正，费率明细不可知。</p>
                        ) : c.charge.lines.length === 0 ? (
                          <p className="hint">{c.charge.status === "pending" ? "尚未计价，计费项未知" : "这次调用没有计费项"}</p>
                        ) : (
                          <DataTable label="计费明细" compact>
                            <thead>
                              <tr>
                                <th>计量</th>
                                <th className="num">数量</th>
                                <th className="num">折扣前单价（元/百万）</th>
                                <th className="num">折后单价（元/百万）</th>
                              </tr>
                            </thead>
                            <tbody>
                              {c.charge.lines.map((l) => (
                                <tr key={l.dimension}>
                                  <td>{dimensionLabel(l.dimension)}</td>
                                  <td className="num">{l.quantity}</td>
                                  <td className="num">{l.rate_cny === null ? "—" : unitPrice(l.dimension, l.rate_cny)}</td>
                                  <td className="num">{(() => {
                                    const rate = l.rate_cny === null ? null : discountedRate(l.rate_cny, c.charge.discount_bps);
                                    return rate === null ? "—" : unitPrice(l.dimension, rate);
                                  })()}</td>
                                </tr>
                              ))}
                            </tbody>
                          </DataTable>
                        )}
                        {(c.charge.discount_bps ?? 0) > 0 && (
                          <p className="hint">
                            本次按组织折扣计费：在折扣前的价格上减免 {percentOf(c.charge.discount_bps ?? 0)}。
                          </p>
                        )}
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </DataTable>
      )}
      {next !== null && (
        <CardFooter className="usage-more flex justify-center border-t border-ink/7 pt-5">
          <Button size="sm" busy={more} busyLabel="加载中…" onClick={() => void loadMore()}>
            加载更多
          </Button>
        </CardFooter>
      )}
    </Section>
  );
}
