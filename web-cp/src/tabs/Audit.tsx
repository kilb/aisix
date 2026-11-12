import { auditSearchText } from "../lib/operationalRecords";
import { useRecordTable } from "../lib/recordTable";
import { RecordTools, RecordPager } from "../components/RecordTools";
import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../App";
import { audit, tenants as tenantsApi, type AuditEntry, type Tenant } from "../lib/api";
import { redacted } from "../lib/auditDetail";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import {
  BentoGrid,
  Card,
  CardFooter,
  CardHeader,
  CardMessage,
  ChartSkeleton,
  Metric,
  MetricSkeleton,
  PageHeader,
  span,
  Toolbar,
} from "../components/page";
import { RankBars } from "../components/charts";
import { RefreshCw } from "lucide-react";

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
    <span className="audit-detail min-w-[22ch] max-w-[72ch]">
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
    label: TARGET_KINDS.find((target) => target.kind === kind)?.label ?? (kind || "未标注对象"),
    value,
    displayValue: fmtInt(value),
  })).sort((a, b) => b.value - a.value || a.label.localeCompare(b.label)).slice(0, 5);

  const newest = rows?.[0] ?? null;
  const oldest = rows?.at(-1) ?? null;
  const leadingKind = kindItems[0] ?? null;

  // 桌面和平板：范围 6 + 对象分布 6；筛选与完整记录 12。
  return (
    <>
      <PageHeader
        title="审计日志"
        eyebrow="变更追踪 · 当前筛选结果"
        dataLead={rows === null
          ? "按对象类型和租户查看平台变更记录。"
          : <>当前已加载 {fmtInt(rows.length)} 条记录，{next === null ? "已到当前筛选结果末尾。" : "更早的记录可继续加载。"}</>}
        description="每一项变更都与它的审计记录在同一事务里写入，最新的在前。详情只列字段名和非机密的值。"
        actions={
          <Button type="button" onClick={() => void loadFirst(filter)}>
            <RefreshCw aria-hidden />
            刷新
          </Button>
        }
      />
      <BentoGrid>
        <Card tone="inverse" className={span.d6}>
          <CardHeader number={1} title="已加载记录 · 当前筛选" />
          {rows === null
            ? (error ? <CardMessage onRetry={() => void loadFirst(filter)}>审计记录读取失败。</CardMessage> : <MetricSkeleton />)
            : <Metric size="hero">{fmtInt(rows.length)}</Metric>}
          <CardFooter>
            <p className="text-caption text-on-inverse-muted">{rows === null ? "读取记录后显示当前范围。" : next === null ? "已加载当前筛选下的全部记录。" : "当前为部分记录，不能代表全部审计量。"}</p>
            {newest !== null && oldest !== null && (
              <dl className="mt-4 grid gap-4 border-t border-surface/12 pt-4 tablet:grid-cols-2">
                <div>
                  <dt className="font-mono text-micro text-on-inverse-muted">本次加载 · 最新</dt>
                  <dd className="mt-2 text-caption"><time dateTime={newest.at}>{fmtTime(newest.at)}</time></dd>
                </div>
                <div>
                  <dt className="font-mono text-micro text-on-inverse-muted">本次加载 · 最早</dt>
                  <dd className="mt-2 text-caption"><time dateTime={oldest.at}>{fmtTime(oldest.at)}</time></dd>
                </div>
              </dl>
            )}
          </CardFooter>
        </Card>
        <Card className={span.d6}>
          <CardHeader number={2} title="对象类型 · 已加载记录前五" description="仅汇总当前已加载记录；不包含尚未读取的更早记录。" />
          <CardFooter>
            {rows === null ? (error
              ? <CardMessage onRetry={() => void loadFirst(filter)}>审计记录读取失败，重试后查看分布。</CardMessage>
              : <ChartSkeleton />)
              : <RankBars items={kindItems} accessibleLabel={leadingKind !== null ? `已加载 ${fmtInt(rows.length)} 条记录中，最多的对象类型是${leadingKind.label}，共 ${leadingKind.displayValue} 条；图中列出前五类` : "当前筛选没有已加载的审计记录"} emptyLabel="没有符合条件的记录" />}
          </CardFooter>
        </Card>
        <Card className={span.d12}>
          <CardHeader number={3} title="变更记录" description="每项变更与审计记录在同一事务里写入，最新的在前。详情只列字段名和非机密的值。" />
          <Toolbar>
            <label className="f filter filter-wide">
              <span>对象类型</span>
              <select value={filter.kind} onChange={(e) => setFilter((f) => ({ ...f, kind: e.target.value }))}>
                <option value="">全部</option>
                {TARGET_KINDS.map((k) => (
                  <option key={k.kind} value={k.kind}>
                    {k.label} · {k.kind}
                  </option>
                ))}
              </select>
            </label>
            <label className="f filter filter-wide">
              <span>租户</span>
              <select value={filter.tenant} onChange={(e) => setFilter((f) => ({ ...f, tenant: e.target.value }))}>
                <option value="">全部</option>
                {tenants.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                    {t.is_internal ? "（平台内部）" : ""}
                  </option>
                ))}
              </select>
            </label>
          </Toolbar>
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
                    <th className="num">编号</th>
                    <th>时间</th>
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
                      <td className="num text-muted-foreground">{r.id}</td>
                      <td className="whitespace-normal text-muted-foreground" title={fmtTime(r.at)}>
                        {fmtTime(r.at)}
                      </td>
                      <td className="whitespace-normal">
                        <span className="whitespace-nowrap">{r.actor.email ?? ""}</span>
                        {ACTOR_KIND[r.actor.kind] && (
                          <span className="hint whitespace-nowrap">
                            {r.actor.email ? " · " : ""}
                            {ACTOR_KIND[r.actor.kind]}
                          </span>
                        )}
                      </td>
                      <td>
                        <code className="rounded-chip bg-well px-2 py-1 font-mono text-meta text-ink-soft">{r.action}</code>
                      </td>
                      <td>
                        <span className="inline-flex flex-col gap-0.5 align-middle">
                          {r.target_kind ? <code className="text-caption">{r.target_kind}</code> : "—"}{" "}
                          {r.target_id && (
                            <code className="block max-w-[19ch] text-meta break-all whitespace-normal text-muted-foreground">
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
      </BentoGrid>
    </>
  );
}
