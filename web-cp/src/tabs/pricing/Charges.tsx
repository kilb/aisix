import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { charges, tenants as tenantsApi, type AdminKey, type Charge, type ChargeFilter, type Tenant } from "../../lib/api";
import { chargeReason } from "../../lib/chargeReason";
import { errorText } from "../../lib/fmt";
import { fmtMicroCny, fmtUtc } from "../../lib/money";
import { modelText, reasonLabel, takeChargesFilter } from "../../lib/pricingStatus";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";

const STATUSES: [string, string][] = [
  ["charged", "已计价"],
  ["exempt", "免计费"],
  ["nothing_to_bill", "无需计费"],
  ["unpriced", "未计价"],
];

const statusLabel = (s: string) => STATUSES.find(([v]) => v === s)?.[1] ?? s;

/**
 * 计费明细: every charge with its pricing reason, filterable and paged. A link
 * from 计价状态 opens it on that kind's calls (its status, reason and model).
 */
export function Charges(_props: TabProps) {
  const [filter, setFilter] = useState<ChargeFilter>(() => takeChargesFilter() ?? {});
  const [eventDraft, setEventDraft] = useState("");
  const [rows, setRows] = useState<Charge[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [tenants, setTenants] = useState<Tenant[]>([]);
  const [keys, setKeys] = useState<AdminKey[]>([]);
  const generation = useRef(0);

  const loadFirst = useCallback(async (f: ChargeFilter) => {
    const g = ++generation.current;
    setRows(null);
    setError(null);
    try {
      const page = await charges.list(f);
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
    if (next === null) return;
    const g = generation.current;
    setLoadingMore(true);
    setError(null);
    try {
      const page = await charges.list({ ...filter, before: next });
      if (g !== generation.current) return;
      setRows((prev) => [...(prev ?? []), ...page.items]);
      setNext(page.next_before);
    } catch (e) {
      if (g === generation.current) setError(errorText(e));
    } finally {
      setLoadingMore(false);
    }
  }

  const set = (patch: ChargeFilter) => setFilter((f) => ({ ...f, ...patch }));
  const tenantName = (id: string) => tenants.find((t) => t.id === id)?.name ?? id;
  // The model and reason a link from 计价状态 set: the form has no field for them, so they are shown here.
  const linked = [
    filter.error_code ? `原因：${reasonLabel(filter.error_code)}` : null,
    filter.model_id ? `模型：${rows?.find((c) => c.requested_model_id === filter.model_id)?.model_name ?? filter.model_id}` : null,
  ].filter((x): x is string => x !== null);

  return (
    <div className="panel">
      <h2>计费明细</h2>
      <p className="hint">
        每一次调用的计价结果，最新的在前。售价和成本都以人民币计：成本是这次调用按上游账号的成本换算率算出的平台成本，未知时显示 —。
        未计价的行写明原因；无需计费的行写明调用怎样结束（上游失败、请求被拒绝或没有用量），这些行不收费。
      </p>
      <form
        className="r"
        onSubmit={(e) => {
          e.preventDefault();
          set({ event_id: eventDraft.trim() || undefined });
        }}
      >
        <label className="f filter">
          <span>状态</span>
          <select value={filter.status ?? ""} onChange={(e) => set({ status: e.target.value || undefined })}>
            <option value="">全部</option>
            {STATUSES.map(([v, l]) => (
              <option key={v} value={v}>
                {l}
              </option>
            ))}
          </select>
        </label>
        <label className="f filter">
          <span>租户</span>
          <select
            value={filter.tenant_id ?? ""}
            onChange={(e) => set({ tenant_id: e.target.value || undefined, api_key_id: undefined })}
          >
            <option value="">全部</option>
            {tenants.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </select>
        </label>
        <label className="f filter">
          <span>密钥</span>
          <select
            value={filter.api_key_id ?? ""}
            disabled={!filter.tenant_id}
            onChange={(e) => set({ api_key_id: e.target.value || undefined })}
          >
            <option value="">全部</option>
            {keys.map((k) => (
              <option key={k.id} value={k.id}>
                {k.label} · {k.prefix}
              </option>
            ))}
          </select>
        </label>
        <label className="f filter">
          <input
            type="checkbox"
            checked={filter.exclude_status === "nothing_to_bill"}
            onChange={(e) => set({ exclude_status: e.target.checked ? "nothing_to_bill" : undefined })}
          />{" "}
          <span>隐藏无需计费</span>
        </label>
        <label className="f filter">
          <span>事件</span>
          <input value={eventDraft} onChange={(e) => setEventDraft(e.target.value)} placeholder="事件编号" />
        </label>
        <Button type="submit" size="sm">
          查找
        </Button>
        <Button type="button" size="sm" icon="refresh" onClick={() => void loadFirst(filter)}>
          刷新
        </Button>
      </form>
      {linked.length > 0 && (
        <div className="r" role="status">
          <span className="hint">只看 {linked.join("，")}</span>
          <Button type="button" size="sm" onClick={() => set({ error_code: undefined, model_id: undefined })}>
            清除
          </Button>
        </div>
      )}
      {error && (
        <div className="note crit" role="alert">
          {error}
        </div>
      )}
      {rows === null && !error && <Loading />}
      {rows !== null && rows.length === 0 && <EmptyState text="没有符合条件的计费记录。" />}
      {rows !== null && rows.length > 0 && (
        <DataTable label="计费明细" scroll>
          <thead>
            <tr>
              <th>时间（UTC）</th>
              <th>租户</th>
              <th>模型</th>
              <th>状态</th>
              <th>原因</th>
              <th className="num">售价（元）</th>
              <th className="num">成本（元）</th>
              <th>档位</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((c) => (
              <tr key={c.seq}>
                <td className="tabular">{fmtUtc(c.occurred_at)}</td>
                <td>{tenantName(c.tenant_id)}</td>
                <td>{modelText(c.model_name, c.model_deleted)}</td>
                <td>{statusLabel(c.status)}</td>
                <td>{chargeReason(c)}</td>
                <td className="num">{c.charge_micro_cny === null ? "—" : fmtMicroCny(c.charge_micro_cny)}</td>
                <td className="num">{c.cost_micro_cny === null ? "—" : fmtMicroCny(c.cost_micro_cny)}</td>
                <td>{c.service_tier ?? "—"}</td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
      {next !== null && (
        <div className="r">
          <Button type="button" busy={loadingMore} busyLabel="加载中…" onClick={() => void loadMore()}>
            加载更多
          </Button>
        </div>
      )}
    </div>
  );
}
