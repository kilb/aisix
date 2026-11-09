import { Fragment, useEffect, useRef, useState, type ReactNode } from "react";
import {
  ApiError,
  keys as keyApi,
  models as modelApi,
  usageCalls,
  usageSummary,
  type Call,
  type PortalModel,
  type SessionMember,
  type TenantKey,
  type UsageSummaryRow,
} from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtCnyPerMillion, fmtMicroCny, fmtUtc, periodLabel, todayUtc, utcRange } from "./lib/money";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import "./usage.css";

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

/** What a failed call's reason is called; one not listed shows raw. */
const ERROR_LABEL: Record<string, string> = {
  model_not_found: "模型不存在",
  invalid_request: "请求无效",
  rate_limited: "请求过于频繁",
  budget_exceeded: "额度已用尽",
  content_filtered: "内容被拦截",
  upstream_unavailable: "上游暂不可用",
  failed: "失败",
};

/** A signed amount for a correction: `+¥0.002` or `-¥0.002`. */
function signed(micro: number): string {
  return micro > 0 ? `+${fmtMicroCny(micro)}` : fmtMicroCny(micro);
}

/** What a call's 费用 cell says. */
function chargeText(c: Call): string {
  const { charge } = c;
  switch (charge.status) {
    case "charged": {
      // Unpriced when made, then corrected by re-pricing: the whole charge is the adjustment.
      if (charge.amount_micro_cny === null && charge.adjusted_micro_cny !== 0) {
        return `${fmtMicroCny(charge.adjusted_micro_cny)}（重新计价）${charge.estimated ? "（估算）" : ""}`;
      }
      const amount = charge.amount_micro_cny === null ? "—" : fmtMicroCny(charge.amount_micro_cny);
      const adjusted = charge.adjusted_micro_cny !== 0 ? `（已调整 ${signed(charge.adjusted_micro_cny)}）` : "";
      return `${amount}${adjusted}${charge.estimated ? "（估算）" : ""}`;
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
  const reason = call.error_class === null ? "失败" : (ERROR_LABEL[call.error_class] ?? call.error_class);
  return <Lamp state="rejected">{`失败（${reason}）`}</Lamp>;
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

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="panel list" role="region" aria-label={title}>
      <div className="panel-head">
        <h2>{title}</h2>
      </div>
      {children}
    </div>
  );
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
    <div className="stack">
      {readErr && (
        <div className="note crit" role="alert">
          {readErr}
        </div>
      )}
      <Summary keys={keyList} showKeys={showKeys} />
      <CallLog keys={keyList} showKeys={showKeys} models={granted} />
    </div>
  );
}

// ── usage summary (用量汇总) ────────────────────────────────────────────────────────────────

function Quantities({ q }: { q: Record<string, number> }) {
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

function Summary({ keys, showKeys }: { keys: TenantKey[]; showKeys: boolean }) {
  const [group, setGroup] = useState<"day" | "month">("day");
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [keyId, setKeyId] = useState("");
  const [rows, setRows] = useState<UsageSummaryRow[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const range = rangeOf(fromDate, toDate);
  // A summary needs both ends: a date left empty reads as the default span.
  const fallback = defaultRange(group);
  const span = range ? (Date.parse(range.to ?? fallback.to) - Date.parse(range.from ?? fallback.from)) / 86_400_000 : 0;
  const tooLong = span > MAX_DAYS[group];
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
  }, [group, fromDate, toDate, keyId, tooLong]);

  return (
    <Section title="用量汇总">
      <div className="panel-notes">
        <div className="usage-filters">
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
        </div>
        <p className="hint">
          按用量发生的时间汇总，不是按计费入账的时间：重新计价后的调整，记在用量发生的那一期。
          {fromDate === "" && toDate === "" && (group === "day" ? "未选日期时汇总最近 30 天（UTC）。" : "未选日期时汇总最近 12 个月（UTC）。")}
        </p>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
      </div>
      {rows === null && <Loading />}
      {rows !== null && rows.length === 0 && !err && !tooLong && <EmptyState text="所选范围内没有用量" mark="route" />}
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
                  {fmtMicroCny(r.charged_micro_cny)}
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
    <Section title="调用记录">
      <div className="panel-notes">
        <div className="usage-filters">
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
        </div>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
      </div>
      {rows === null && <Loading />}
      {rows !== null && rows.length === 0 && !err && <EmptyState text="所选条件下没有调用记录" mark="log" />}
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
                    <td className="num">{chargeText(c)}</td>
                  </tr>
                  {expanded && (
                    <tr className="usage-detail">
                      <td colSpan={9}>
                        <h3>计费明细</h3>
                        {c.charge.lines.length === 0 ? (
                          <p className="hint">这次调用没有计费项</p>
                        ) : (
                          <DataTable label="计费明细" compact>
                            <thead>
                              <tr>
                                <th>计量</th>
                                <th className="num">数量</th>
                                <th className="num">单价（元/百万）</th>
                              </tr>
                            </thead>
                            <tbody>
                              {c.charge.lines.map((l) => (
                                <tr key={l.dimension}>
                                  <td>{dimensionLabel(l.dimension)}</td>
                                  <td className="num">{l.quantity}</td>
                                  <td className="num">{l.rate_cny === null ? "—" : fmtCnyPerMillion(l.rate_cny)}</td>
                                </tr>
                              ))}
                            </tbody>
                          </DataTable>
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
        <div className="usage-more">
          <Button busy={more} busyLabel="加载中…" onClick={() => void loadMore()}>
            加载更多
          </Button>
        </div>
      )}
    </Section>
  );
}
