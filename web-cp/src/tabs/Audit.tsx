import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../App";
import { audit, tenants as tenantsApi, type AuditEntry, type Tenant } from "../lib/api";
import { redacted } from "../lib/auditDetail";
import { errorText, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { PageHeader } from "../components/page";
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
      const page = await audit.list(query(f));
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
      const page = await audit.list({ ...query(filter), before: next });
      if (g !== generation.current) return;
      setRows((prev) => [...(prev ?? []), ...page.items]);
      setNext(page.next_before);
    } catch (e) {
      if (g === generation.current) setError(errorText(e));
    } finally {
      setLoadingMore(false);
    }
  }

  const tenantName = (id: string) => tenants.find((t) => t.id === id)?.name ?? id;

  return (
    <>
      <PageHeader
        title="审计日志"
        description="每一项变更都与它的审计记录在同一事务里写入，最新的在前。详情只列字段名和非机密的值。"
        actions={
          <Button type="button" onClick={() => void loadFirst(filter)}>
            <RefreshCw aria-hidden />
            刷新
          </Button>
        }
      />
      <div className="panel mb-0">
        <div className="flex flex-wrap items-center gap-x-6 gap-y-3">
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
        </div>
        {tenantsError && <div className="note crit">无法读取租户列表：{tenantsError}</div>}
        {error && (
          <div className="note crit" role="alert">
            {error}
          </div>
        )}
        {rows === null && !error && <Loading />}
        {rows !== null && rows.length === 0 && <EmptyState mark="log" text="没有符合条件的记录。" />}
        {rows !== null && rows.length > 0 && (
          <DataTable scroll>
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
              {rows.map((r) => (
                <tr key={r.id}>
                  <td className="num text-muted-foreground">{r.id}</td>
                  <td className="whitespace-normal text-muted-foreground" title={r.at}>
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
                    <code className="rounded-md border border-border bg-muted px-1.5 py-0.5 text-xs">{r.action}</code>
                  </td>
                  <td>
                    <span className="inline-flex flex-col gap-0.5 align-middle">
                      {r.target_kind ? <code className="text-[13px]">{r.target_kind}</code> : "—"}{" "}
                      {r.target_id && (
                        <code className="block max-w-[19ch] text-xs break-all whitespace-normal text-muted-foreground">
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
        )}
        {next !== null && (
          <div className="-mx-6 -mb-6 flex justify-center rounded-b-xl border-t border-border bg-muted/40 px-6 py-3">
            <Button type="button" size="sm" disabled={loadingMore} onClick={() => void loadMore()}>
              加载更多
            </Button>
          </div>
        )}
      </div>
    </>
  );
}
