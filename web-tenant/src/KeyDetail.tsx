import { useEffect, useLayoutEffect, useState } from "react";
import { ArrowLeft, Pencil } from "lucide-react";
import { KeyForm, stateOf, when } from "./Keys";
import { ApiError, groups as groupApi, keys as keyApi, models as modelApi, org as readOrg, usageSummary, type Org, type PortalGroup, type PortalModel, type SessionMember, type TenantKey, type TenantStatus, type UsageSummaryRow } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { exactInteger, formatCount } from "./lib/chartFormat";
import { fmtMicroCny } from "./lib/money";
import { beijingRange, defaultCalendarRange, fmtBeijing, periodLabel } from "./lib/time";
import { metricsRangeError } from "./lib/apiMetrics";
import { usageDateValue, usageRange } from "./lib/usageNavigation";
import { useHashQuery } from "./lib/useHashQuery";
import { useRecordTable } from "./lib/recordTable";
import { ApiMetricsPanel } from "./components/ApiMetricsPanel";
import { BentoGrid, Card, CardHeader, CardMessage, PageHeader, span } from "./components/page";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { RecordTablePager } from "./ui/RecordTable";

const DIMENSIONS: Record<string, string> = {
  input: "输入", output: "输出", cache_read: "缓存读", cache_write: "缓存写", cache_write_1h: "1 小时缓存写",
  reasoning: "推理", audio_input: "音频输入", audio_output: "音频输出", image_input: "图片输入", image_output: "图片输出", web_search: "联网搜索",
};

function failure(error: unknown): string {
  return error instanceof ApiError ? formMessageFor(error) : error instanceof Error ? error.message : String(error);
}

function KeyQuantities({ quantities }: { quantities: UsageSummaryRow["quantities"] }) {
  if (quantities === null || typeof quantities !== "object") return <span>未知（历史计量明细缺失）</span>;
  const lines = Object.entries(quantities);
  if (lines.length === 0) return <span>未提供计量明细</span>;
  return <dl className="grid gap-x-5 gap-y-1 sm:grid-cols-2">{lines.map(([dimension, quantity]) => {
    const value = exactInteger(quantity);
    return <div key={dimension} className="flex flex-wrap justify-between gap-x-3"><dt>{DIMENSIONS[dimension] ?? dimension}</dt><dd className="tabular-nums">{value === null || value < 0n ? "未知" : formatCount(value)}{dimension === "web_search" ? " 次" : Object.hasOwn(DIMENSIONS, dimension) ? " token" : ""}</dd></div>;
  })}</dl>;
}

/** 单密钥明细只做固定数量的聚合请求，不扫描调用日志。 */
function KeyUsage({ keyId }: { keyId: string }) {
  const [query, update] = useHashQuery();
  const fallback = defaultCalendarRange("day");
  const selected = usageRange(query);
  const from = selected?.from ?? fallback.from;
  const to = selected?.to ?? fallback.to;
  const rangeError = selected === null ? "请选择有效的起止日期，结束日期不能早于开始日期。" : metricsRangeError(from, to, "day");
  const [refresh, setRefresh] = useState(0);
  const requestKey = JSON.stringify([keyId, from, to, refresh]);
  const [answer, setAnswer] = useState<{ key: string; rows: UsageSummaryRow[] | null; error: string | null }>({ key: "", rows: null, error: null });
  useEffect(() => {
    if (rangeError) return;
    let live = true;
    setAnswer({ key: requestKey, rows: null, error: null });
    void usageSummary({ group: "day", from, to, api_key_id: keyId }).then(
      rows => { if (live) setAnswer({ key: requestKey, rows, error: null }); },
      error => { if (live) setAnswer({ key: requestKey, rows: null, error: failure(error) }); },
    );
    return () => { live = false; };
  }, [requestKey, keyId, from, to, rangeError]);
  const current = answer.key === requestKey ? answer : null;
  const table = useRecordTable({ items: current?.rows ?? [], resetKey: requestKey });
  return <BentoGrid>
    <Card className={span.d12} aria-label="密钥用量范围">
      <CardHeader title="用量明细" description="仅统计当前密钥。默认最近 30 个北京自然日，最多查询 92 天。" />
      <div className="usage-filters">
        <Field label="开始日期"><input type="date" value={usageDateValue(query.get("from") ?? from)} onChange={e => update({ from: e.target.value ? beijingRange(e.target.value, e.target.value).from : undefined })} /></Field>
        <Field label="结束日期"><input type="date" value={usageDateValue(query.get("to") ?? to, true)} onChange={e => update({ to: e.target.value ? beijingRange(e.target.value, e.target.value).to : undefined })} /></Field>
        <Button onClick={() => setRefresh(value => value + 1)}>刷新用量</Button>
        <a className="metric-scope-action" href={`#calls?${new URLSearchParams({ api_key_id: keyId, from, to })}`}>查看调用记录</a>
      </div>
      <p className="hint">{fmtBeijing(from)} 至 {fmtBeijing(to)}（UTC+8，不含结束时间）</p>
      {rangeError && <p className="note crit" role="alert">{rangeError}</p>}
    </Card>
    <ApiMetricsPanel scope="tenant" enabled={!rangeError} from={from} to={to} bucket="day" apiKeyId={keyId} refreshKey={refresh} className={span.d12} />
    {!rangeError && <Card className={span.d12} aria-label="密钥计量明细">
      <CardHeader title="按日与模型的计量明细" description="按用量发生时间汇总，包含已计费重试及历史费用调整。缺失记录显示未知，不视为免费或零用量。" />
      {current?.error ? <CardMessage onRetry={() => setRefresh(value => value + 1)}>{current.error}</CardMessage> : current?.rows === null || !current ? <Loading /> : current.rows.length === 0 ? <CardMessage>所选范围内没有用量</CardMessage> : <DataTable label="密钥计量明细">
        <thead><tr><th>日期</th><th>模型</th><th>调用尝试</th><th>已计价费用</th><th>计量维度</th></tr></thead>
        <tbody>{table.rows.map((row, index) => {
          const amount = exactInteger(row.charged_micro_cny);
          const merged = row.model.id === null && row.model.name === "other";
          return <tr key={`${row.period_start}/${row.model.id ?? row.model.name}/${index}`}>
            <td data-label="日期">{merged ? "跨期合并" : periodLabel(row.period_start, "day")}</td><td data-label="模型">{merged ? "其他（合并记录）" : row.model.name || "未授权或未知模型"}</td>
            <td data-label="调用尝试">{formatCount(row.requests)}</td><td data-label="已计价费用" className="tabular-nums">{amount === null ? "未知" : fmtMicroCny(amount)}{row.estimated_requests > 0 ? "（含估算）" : ""}</td>
            <td data-label="计量维度"><KeyQuantities quantities={row.quantities} /></td>
          </tr>;
        })}</tbody>
      </DataTable>}
      {current?.rows && !current.error && <RecordTablePager page={table.page} pageSize={table.pageSize} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} total={table.filteredCount} label="密钥计量记录" />}
    </Card>}
  </BentoGrid>;
}

export function KeyDetail({ keyId, onBack, member, status, onSelfChanged }: {
  keyId: string; onBack: () => void; member: SessionMember; status: TenantStatus; onSelfChanged: () => void;
}) {
  const [query, update] = useHashQuery();
  const editing = query.get("edit") === "1";
  useLayoutEffect(() => { window.scrollTo({ top: 0, left: 0, behavior: "instant" }); }, [keyId, editing]);
  const active = status === "active";
  const [revision, setRevision] = useState(0);
  const [answer, setAnswer] = useState<{ id: string; key: TenantKey | null; error: string | null }>({ id: "", key: null, error: null });
  const [catalog, setCatalog] = useState<PortalModel[] | null>(null);
  const [groups, setGroups] = useState<PortalGroup[] | null>(null);
  const [caps, setCaps] = useState<Org["limits"] | null>(null);
  const [readErr, setReadErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    let live = true;
    setAnswer({ id: keyId, key: null, error: null });
    void keyApi.get(keyId).then(
      key => { if (live) setAnswer({ id: keyId, key, error: null }); },
      error => {
        if (!live) return;
        setAnswer({ id: keyId, key: null, error: error instanceof ApiError && error.status === 404 ? "密钥不存在，或你没有查看权限。" : failure(error) });
        if (error instanceof ApiError && error.code === "tenant_not_active") onSelfChanged();
      },
    );
    return () => { live = false; };
  }, [keyId, revision]);
  useEffect(() => {
    if (!active || !editing) return;
    let live = true;
    setReadErr(null);
    const failed = (error: unknown) => { if (live) setReadErr(failure(error)); };
    void modelApi.list().then(items => { if (live) setCatalog(items); }, failed);
    void groupApi.list().then(items => { if (live) setGroups(items); }, failed);
    void readOrg().then(org => { if (live) setCaps(org.limits); }, failed);
    return () => { live = false; };
  }, [active, editing]);
  const key = answer.id === keyId ? answer.key : null;
  const error = answer.id === keyId ? answer.error : null;
  const header = <PageHeader
    title={editing ? "修改密钥" : key?.label ?? "密钥详情"}
    description={editing ? "修改现有调用范围、限流与有效期。" : "查看该密钥的用量、费用与配置。"}
    actions={<>
      <Button onClick={onBack} disabled={busy}><ArrowLeft aria-hidden />返回密钥列表</Button>
      {key && active && !editing && <Button kind="primary" onClick={() => { setSaved(false); update({ edit: "1" }); }}><Pencil aria-hidden />修改密钥</Button>}
    </>}
  />;
  if (error) return <>{header}<Card><CardMessage onRetry={() => setRevision(value => value + 1)}>{error}</CardMessage></Card></>;
  if (!key) return <>{header}<Loading /></>;
  if (editing) return <>{header}{active ? <KeyForm
    key={`${key.id}/${key.version}/${revision}`}
    initial={key}
    me={member}
    manager={member.role === "owner" || member.role === "admin"}
    catalog={catalog}
    groups={groups}
    caps={caps}
    people={null}
    readErr={readErr}
    onInactive={onSelfChanged}
    onBusyChange={setBusy}
    onCancel={() => update({ edit: undefined })}
    onConflict={() => setRevision(value => value + 1)}
    onSaved={updated => {
      setAnswer({ id: keyId, key: updated, error: null });
      setSaved(true);
      update({ edit: undefined });
    }}
  /> : <Card><p className="note crit">组织已停用，暂时无法修改密钥。</p></Card>}</>;
  const state = stateOf(key);
  return <>
    {header}
    {saved && <p className="note" role="status">密钥修改已保存</p>}
    <Card className="mb-6" aria-label="密钥配置摘要">
      <CardHeader title="密钥配置" />
      <dl className="compact-overview-facts">
        <div><dt>公开前缀</dt><dd><code>{key.prefix}</code></dd></div>
        <div><dt>归属</dt><dd>{key.owner === null ? "组织" : key.owner.id === member.id ? "我" : key.owner.display_name ?? "成员"}</dd></div>
        <div><dt>分组</dt><dd>{key.group?.display_name ?? "未选择分组"}{!key.group_available ? "（不可用）" : ""}</dd></div>
        <div><dt>模型范围</dt><dd>{key.model_ids === null ? "全部组织授权" : `${formatCount(key.model_ids.length)} 个指定模型`}</dd></div>
        <div><dt>并发限制</dt><dd>{key.rate_limit.concurrency === undefined ? "沿用组织上限" : formatCount(key.rate_limit.concurrency)}</dd></div>
        <div><dt>每分钟请求</dt><dd>{key.rate_limit.rpm === undefined ? "沿用组织上限" : formatCount(key.rate_limit.rpm)}</dd></div>
        <div><dt>状态</dt><dd><Lamp state={state.lamp}>{state.text}</Lamp></dd></div>
        <div><dt>有效期</dt><dd>{key.expires_at ? when(key.expires_at) : "永不过期"}</dd></div>
        <div><dt>创建时间</dt><dd>{when(key.created_at)}</dd></div>
      </dl>
    </Card>
    {active ? <KeyUsage key={key.id} keyId={key.id} /> : <p className="note crit">组织已停用，暂时无法查询用量或修改密钥。</p>}
  </>;
}
