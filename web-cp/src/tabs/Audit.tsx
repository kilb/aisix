import "./operations.css";
import "./Audit.css";
import { auditSearchText } from "../lib/operationalRecords";
import { useRecordTable } from "../lib/recordTable";
import { RecordTools, RecordPager } from "../components/RecordTools";
import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../App";
import { audit, tenants as tenantsApi, type AuditEntry, type Tenant } from "../lib/api";
import { redacted } from "../lib/auditDetail";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { OperationalTable as DataTable, Loading } from "../components/OperationalTable";
import { EmptyState } from "../ui/EmptyState";
import { Card, CardHeader, Metric, MetricSkeleton, PageHeader, Toolbar } from "../components/page";
import { RefreshCw } from "lucide-react";
import { Select } from "../ui/Select";

/**
 * The `target_kind` values the control plane records, by the names this
 * page shows. No route lists them; a row of a kind missing here still
 * shows, by its raw name.
 */
const TARGET_KINDS: { kind: string; label: string }[] = [
  { kind: "operator", label: "运维账号" },
  { kind: "tenant", label: "租户" },
  { kind: "member", label: "租户成员" },
  { kind: "member_invite", label: "成员邀请" },
  { kind: "api_key", label: "租户密钥" },
  { kind: "gateway_credential", label: "网关凭据" },
  { kind: "settings", label: "平台设置" },
  { kind: "environment", label: "环境与下发" },
  { kind: "models", label: "模型" },
  { kind: "provider_keys", label: "上游账号" },
  { kind: "cache_policies", label: "缓存策略" },
  { kind: "rate_limit_policies", label: "限流策略" },
  { kind: "guardrails", label: "护栏" },
  { kind: "guardrail_attachments", label: "护栏挂载" },
  { kind: "observability_exporters", label: "可观测性导出" },
  { kind: "budget_policy", label: "预算策略" },
];

const ACTOR_KIND: Record<string, string> = { member: "租户成员", system: "系统", anonymous: "匿名" };

/** A value as stored: strings bare, a list of plain values comma-separated, anything else as JSON. */
function valueText(value: unknown): string {
  if (typeof value === "string") return value;
  if (Array.isArray(value) && value.length > 0 && value.every((v) => v === null || typeof v !== "object")) {
    return value.map((v) => (typeof v === "string" ? v : JSON.stringify(v))).join(", ");
  }
  return JSON.stringify(value) ?? "—";
}

/** A row's `detail`: each field's name and value. */
function DetailSummary({ detail }: { detail: unknown }) {
  const shown = redacted(detail);
  if (shown === null || shown === undefined) return <span className="hint">—</span>;
  if (typeof shown !== "object" || Array.isArray(shown)) return <>{valueText(shown)}</>;
  const entries = Object.entries(shown);
  if (entries.length === 0) return <span className="hint">—</span>;
  return (
    <span className="audit-detail">
      {entries.map(([k, v]) => (
        <span key={k} className="audit-field">
          <span className="lab">{k}</span> {valueText(v)}
        </span>
      ))}
    </span>
  );
}

interface Filter {
  kind: string;
  tenant: string;
}

const query = (f: Filter) => ({ target_kind: f.kind || undefined, tenant_id: f.tenant || undefined });

/**
 * "审计日志": who changed what, newest first, a page at a time as the
 * control plane pages it, narrowed by target kind and by tenant.
 */
export function Audit(_: TabProps) {
  const [filter, setFilter] = useState<Filter>({ kind: "", tenant: "" });
  const [rows, setRows] = useState<AuditEntry[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [tenants, setTenants] = useState<Tenant[]>([]);
  const [tenantsError, setTenantsError] = useState<string | null>(null);
  // Bumped by each first-page read, so an older read's answer is dropped.
  const generation = useRef(0);

  const loadFirst = useCallback(async (f: Filter) => {
    const g = ++generation.current;
    setRows(null);
    setNext(null);
    setError(null);
    try {
      const page = await audit.list({ ...query(f), limit: 50 });
      if (g !== generation.current) return;
      setRows(page.items);
      setNext(page.next_before);
    } catch (e) {
      if (g === generation.current) setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void loadFirst(filter);
  }, [filter, loadFirst]);

  useEffect(() => {
    tenantsApi.list().then(setTenants, (e) => setTenantsError(errorText(e)));
  }, []);

  async function loadMore() {
    if (next === null) return;
    const g = generation.current;
    setLoadingMore(true);
    setError(null);
    try {
      const page = await audit.list({ ...query(filter), before: next, limit: 50 });
      if (g !== generation.current) return;
      setRows((prev) => [...(prev ?? []), ...page.items]);
      setNext(page.next_before);
    } catch (e) {
      if (g === generation.current) setError(errorText(e));
    } finally {
      setLoadingMore(false);
    }
  }

  const table = useRecordTable({ items: rows ?? [], searchText: auditSearchText, resetKey: `${filter.kind}:${filter.tenant}` });
  const tenantName = (id: string) => tenants.find((t) => t.id === id)?.name ?? id;
  // 只汇总已加载的筛选结果；游标后的记录不进入任何统计。
  const kindCounts = new Map<string, number>();
  for (const row of rows ?? []) {
    const kind = row.target_kind ?? "";
    kindCounts.set(kind, (kindCounts.get(kind) ?? 0) + 1);
  }
  const kindItems = [...kindCounts].map(([kind, value]) => ({
    kind,
    label: TARGET_KINDS.find((target) => target.kind === kind)?.label ?? (kind || "未标注对象"),
    value,
    displayValue: fmtInt(value),
  })).sort((a, b) => b.value - a.value || a.label.localeCompare(b.label)).slice(0, 5);

  const newest = rows?.[0] ?? null;
  const oldest = rows?.at(-1) ?? null;

  // 统计只描述已加载范围；完整记录始终通栏显示。
  return (
    <div className="cp-operations cp-audit">
      <PageHeader
        title="审计日志"
        eyebrow="变更追踪 · 当前筛选结果"
        dataLead={rows === null
          ? "按对象类型和租户查看平台变更记录。"
          : <>当前已加载 {fmtInt(rows.length)} 条记录，{next === null ? "已到当前筛选结果末尾。" : "更早的记录可继续加载。"}</>}
        description="每一项变更都与它的审计记录在同一事务里写入，最新的在前。详情只列字段名和非机密的值。"
        actions={
          <Button type="button" className="ops-icon-button" aria-label="刷新" title="刷新审计日志" onClick={() => void loadFirst(filter)}><RefreshCw aria-hidden /></Button>
        }
      />
      <section className="audit-overview" aria-label="已加载审计记录概况">
        <Card className="audit-stat">
          <CardHeader title="已加载记录" />
          {rows === null && !error ? <MetricSkeleton /> : <Metric>{rows === null ? "—" : fmtInt(rows.length)}</Metric>}
          <p className="text-caption text-fg-3">{rows === null ? "读取记录后显示当前范围。" : next === null ? "已加载当前筛选下的全部记录。" : "当前为部分记录，可继续加载更早记录。"}</p>
        </Card>
        <Card className="audit-stat">
          <CardHeader title="对象类型" />
          {rows === null && !error ? <MetricSkeleton /> : <Metric>{rows === null ? "—" : fmtInt(kindCounts.size)}</Metric>}
          <p className="text-caption text-fg-3">仅汇总当前已加载的记录</p>
        </Card>
        <Card className="audit-stat audit-range">
          <CardHeader title="本次加载 · 最新" />
          <p>{newest ? <time dateTime={newest.at}>{fmtTime(newest.at)}</time> : "—"}</p>
        </Card>
        <Card className="audit-stat audit-range">
          <CardHeader title="本次加载 · 最早" />
          <p>{oldest ? <time dateTime={oldest.at}>{fmtTime(oldest.at)}</time> : "—"}</p>
        </Card>
      </section>
      <Card className="audit-list">
          <CardHeader title="变更记录" description="最新记录在前；详情仅显示字段名与非机密值。" />
          <Toolbar>
            <label className="f filter filter-wide">
              <span>对象类型</span>
              <Select size="compact" value={filter.kind} onValueChange={(value) => setFilter((f) => ({ ...f, kind: value }))}>
                <option value="">全部</option>
                {TARGET_KINDS.map((k) => (
                  <option key={k.kind} value={k.kind}>
                    {k.label} · {k.kind}
                  </option>
                ))}
              </Select>
            </label>
            <label className="f filter filter-wide">
              <span>租户</span>
              <Select size="compact" value={filter.tenant} onValueChange={(value) => setFilter((f) => ({ ...f, tenant: value }))}>
                <option value="">全部</option>
                {tenants.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                    {t.is_internal ? "（平台内部）" : ""}
                  </option>
                ))}
              </Select>
            </label>
          </Toolbar>
          {rows !== null && kindItems.length > 0 && (
            <section className="audit-kinds" aria-label="对象类型 · 已加载记录前五">
              <h3 className="text-small text-fg-3">对象类型 · 已加载记录前五</h3>
              <dl>{kindItems.map((kind) => <div key={kind.kind}><dt>{kind.label}</dt><dd>{kind.displayValue}</dd></div>)}</dl>
            </section>
          )}
          {tenantsError && <div className="note crit">无法读取租户列表：{tenantsError}</div>}
          {error && (
            <div className="note crit" role="alert">
              {error}{" "}
              <Button type="button" size="sm" onClick={() => void (rows === null ? loadFirst(filter) : loadMore())} disabled={loadingMore}>重试</Button>
            </div>
          )}
          {rows === null && !error && <Loading />}
          {rows !== null && rows.length === 0 && <EmptyState mark="log" text="没有符合条件的记录。" />}
          {rows !== null && rows.length > 0 && (
            <>
              <RecordTools table={table} label="审计日志" loaded></RecordTools>
              <DataTable scroll label="审计日志">
                <thead>
                  <tr>
                    <th>时间 / 编号</th>
                    <th>操作者</th>
                    <th>动作</th>
                    <th>对象</th>
                    <th>租户</th>
                    <th>详情</th>
                  </tr>
                </thead>
                <tbody>
                  {table.rows.map((r) => (
                    <tr key={r.id}>
                      <td className="whitespace-normal text-fg-3" title={fmtTime(r.at)}>
                        {fmtTime(r.at)}
                        <span className="ops-cell-secondary">#{r.id}</span>
                      </td>
                      <td className="whitespace-normal">
                        <span className="ops-cell-main">{r.actor.email ?? ""}</span>
                        {ACTOR_KIND[r.actor.kind] && (
                          <span className="ops-cell-secondary">
                            {ACTOR_KIND[r.actor.kind]}
                          </span>
                        )}
                      </td>
                      <td>
                        <code className="rounded-full bg-sunken px-2 py-1 font-mono text-caption text-fg-2">{r.action}</code>
                      </td>
                      <td>
                        <span className="inline-flex flex-col gap-0.5 align-middle">
                          {r.target_kind ? <code className="text-caption">{r.target_kind}</code> : "—"}{" "}
                          {r.target_id && (
                            <code className="block max-w-[19ch] text-caption break-all whitespace-normal text-fg-3">
                              {r.target_id}
                            </code>
                          )}
                        </span>
                      </td>
                      <td>{r.tenant_id ? tenantName(r.tenant_id) : "—"}</td>
                      <td>
                        <DetailSummary detail={r.detail} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </DataTable>
              <RecordPager table={table} label="审计日志" resetKey={`${filter.kind}:${filter.tenant}`} hasMore={next !== null} onMore={loadMore} busy={loadingMore} />
            </>
          )}

      </Card>
    </div>
  );
}
