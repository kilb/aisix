import { Fragment, useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { charges, tenants as tenantsApi, type AdminKey, type Charge, type ChargeFilter, type Tenant } from "../../lib/api";
import { chargeReason } from "../../lib/chargeReason";
import { errorText } from "../../lib/fmt";
import { fmtMicroCny, fmtUtc } from "../../lib/money";
import { modelText, reasonLabel, takeChargesFilter } from "../../lib/pricingStatus";
import { chargeFromLines, dimensionLabel, lineUsd, rateText, reportedCounters, usageSummary } from "../../lib/chargeUsage";
import { usdRateText } from "../../lib/rateUnits";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { BentoGrid, Card, CardFooter, CardHeader, PageHeader, span } from "../../components/page";
import { formatCount } from "../../lib/chartFormat";
import { cn } from "../../lib/cn";

const STATUSES: [string, string][] = [
  ["charged", "已计价"],
  ["exempt", "免计费"],
  ["nothing_to_bill", "无需计费"],
  ["unpriced", "未计价"],
];

const statusLabel = (s: string) => STATUSES.find(([v]) => v === s)?.[1] ?? s;

// 桌面/平板：12 列完整记录表，窄屏在卡片内横向滚动。
/**
 * 计费明细 (under 计费): every charge with its pricing reason, the upstream
 * account and model that served it and its usage, filterable and paged. A
 * row opens to what checks it: the billed quantities at their rates, the
 * counters the upstream reported, and the charge recomputed from the lines
 * by the engine's formula beside the amount billed. A link
 * from 计价状态 opens it on that kind's calls (its status, reason and model).
 * The charges list filters by none of the rest of a 计价状态 row's scope (its
 * group or tier, the last 24 hours, corrected calls left out), so the filter
 * says it is wider and its rows need not add up to that row's count.
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
  // The charges whose check row is open, by seq.
  const [open, setOpen] = useState<Set<number>>(() => new Set());
  const generation = useRef(0);

  const loadFirst = useCallback(async (f: ChargeFilter) => {
    const g = ++generation.current;
    setRows(null);
    setError(null);
    // A new first page has no cursor yet: the one on screen belongs to the
    // filter it was read for, so a 加载更多 clicked while this loads would
    // otherwise ask for this filter with the previous filter's cursor.
    setNext(null);
    setLoadingMore(false);
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
    // A continuation extends the page on screen; without one (its filter is
    // still loading, or it failed) there is nothing to extend.
    if (next === null || rows === null || loadingMore) return;
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
      // Only this generation's own continuation ends: one that a filter
      // change has already superseded must not unlock the new one.
      if (g === generation.current) setLoadingMore(false);
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
    <>
      <PageHeader
        title="计费明细"
        eyebrow="计费 / 调用记录"
        dataLead="失败但已产生用量的调用，仍按实际用量计费；没有用量的调用不收费。"
        description={
          <>
            每一次调用的计价结果，最新的在前。售价和成本都以人民币计：成本是这次调用按上游账号的成本换算率算出的平台成本，未知时显示 —。
            未计价的行写明原因；无需计费的行写明调用怎样结束（上游失败、没有可用的上游、调用方中断、网关出错、请求被拒绝或没有用量），这些行不收费。
          </>
        }
        actions={
          <Button type="button" icon="refresh" onClick={() => void loadFirst(filter)}>
            刷新
          </Button>
        }
      />
      <BentoGrid>
      <Card className={span.d12}>
        <CardHeader
          number={1}
          title="调用计费记录"
          description={<>
            {rows === null ? (error ? "当前筛选结果读取失败，请重试。" : "正在读取当前筛选结果。") : `当前筛选已加载 ${formatCount(rows.length)} 条${next === null ? "，已到记录末尾。" : "，可继续加载。"}`}
            每一次调用的计价结果，最新的在前。售价和成本都以人民币计：成本是这次调用按上游账号的成本换算率算出的平台成本，未知时显示 —。
            未计价的行写明原因；无需计费的行写明调用怎样结束（上游失败、没有可用的上游、调用方中断、网关出错、请求被拒绝或没有用量），这些行不收费。
          </>}
        />
        <form
          className="flex flex-wrap items-center gap-x-4 gap-y-3"
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
          <div className="flex min-w-0 items-center gap-2 max-md:w-full md:ml-auto">
            <label className="f filter min-w-0 flex-1">
              <span>事件</span>
              <input
                className="h-11 min-w-0 text-caption md:w-56"
                value={eventDraft}
                onChange={(e) => setEventDraft(e.target.value)}
                placeholder="事件编号"
              />
            </label>
            <Button type="submit" size="sm">
              查找
            </Button>
          </div>
        </form>
        {linked.length > 0 && (
          <div className="mt-3 flex flex-wrap items-center gap-3 rounded-cell border border-border bg-well/50 px-3 py-2" role="status">
            <span className="hint">只看 {linked.join("，")}（不限分组/档位与时间，也含已通过重计价更正的调用，条数可能与计价状态的次数不同）</span>
            <Button type="button" size="sm" className="ml-auto" onClick={() => set({ error_code: undefined, model_id: undefined })}>
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
                <th>上游</th>
                <th>用量</th>
                <th className="right">
                  <span className="vh">复核</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((c) => {
                const shown = open.has(c.seq);
                return (
                  <Fragment key={c.seq}>
                    <tr>
                      <td className="tabular text-muted-foreground">{fmtUtc(c.occurred_at)}</td>
                      <td>{tenantName(c.tenant_id)}</td>
                      <td className="font-medium">{modelText(c.model_name, c.model_deleted)}</td>
                      <td>{statusLabel(c.status)}</td>
                      <td>{chargeReason(c)}</td>
                      <td className="num">{c.charge_micro_cny === null ? "—" : fmtMicroCny(c.charge_micro_cny)}</td>
                      <td className="num">{c.cost_micro_cny === null ? "—" : fmtMicroCny(c.cost_micro_cny)}</td>
                      <td>{c.service_tier ?? "—"}</td>
                      <td>
                        <Upstream c={c} />
                      </td>
                      <td className="tabular text-caption">{usageSummary(c.usage) ?? "—"}</td>
                      <td className="right">
                        <Button
                          kind="quiet"
                          size="sm"
                          aria-expanded={shown}
                          aria-controls={`charge-check-${c.seq}`}
                          aria-label={`${shown ? "收起" : "展开"}用量明细 ${c.event_id}`}
                          onClick={() =>
                            setOpen((prev) => {
                              const next = new Set(prev);
                              if (next.has(c.seq)) next.delete(c.seq);
                              else next.add(c.seq);
                              return next;
                            })
                          }
                        >
                          {shown ? "收起" : "明细"}
                        </Button>
                      </td>
                    </tr>
                    {shown && (
                      <tr className="dt-detail" id={`charge-check-${c.seq}`}>
                        <td colSpan={11}>
                          <ChargeCheck c={c} />
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
          <CardFooter className="mt-0 justify-center">
            <Button type="button" busy={loadingMore} busyLabel="加载中…" onClick={() => void loadMore()}>
              加载更多
            </Button>
          </CardFooter>
        )}
      </Card>
      </BentoGrid>
    </>
  );
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
function ChargeCheck({ c }: { c: Charge }) {
  const reported = reportedCounters(c.usage);
  const recomputed = chargeFromLines(c.lines, c.cny_per_usd_e4, c.discount_bps);
  return (
    <div className="grid gap-4 rounded-cell border border-border bg-well/50 p-4 text-ink-soft lg:grid-cols-[minmax(0,3fr)_minmax(0,2fr)]">
      <section aria-label={`计费数量 ${c.event_id}`}>
        <h4 className="mb-2 text-caption font-medium">计费数量</h4>
        {c.lines === null ? (
          <p className="text-caption text-muted-foreground">
            这次调用的金额由一次未记录费率明细的重新定价更正，费率明细不可知。
          </p>
        ) : c.lines.length === 0 ? (
          <p className="text-caption text-muted-foreground">这次调用没有计费的数量。</p>
        ) : (
          <table className="w-full text-caption">
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
                <tr key={l.dimension}>
                  <td className={ROW}>{dimensionLabel(l.dimension)}</td>
                  <td className={cn(ROW, "text-right")}>{l.quantity.toLocaleString("en-US")}</td>
                  <td className={cn(ROW, "text-right")}>{usdRateText(l.dimension, l.rate)}</td>
                  <td className={cn(ROW, "text-right")}>{lineUsd(l) ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
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
      <section aria-label={`上游报告的用量 ${c.event_id}`}>
        <h4 className="mb-2 text-caption font-medium">上游报告的用量</h4>
        {reported.length === 0 ? (
          <p className="text-caption text-muted-foreground">这次调用没有上游用量记录。</p>
        ) : (
          <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-6 text-caption">
            {reported.map((r) => (
              <Fragment key={r.key}>
                <dt className="border-t border-border py-1.5 text-text-2">{r.label}</dt>
                <dd className="border-t border-border py-1.5 text-right tabular-nums">{r.value.toLocaleString("en-US")}</dd>
              </Fragment>
            ))}
          </dl>
        )}
        <p className="mt-3 text-meta text-muted-foreground">
          事件 <code>{c.event_id}</code>
        </p>
      </section>
    </div>
  );
}
