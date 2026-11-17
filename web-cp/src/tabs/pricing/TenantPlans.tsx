import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { ApiError, pricing, tenantPricing, tenants, type Detail, type DiscountRevision, type Tenant, type TenantDiscountSummary, type TenantPlan } from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtBeijing } from "../../lib/time";
import { discountBps, fmtPercent } from "../../lib/percent";
import { useRecordTable } from "../../lib/recordTable";
import { tenantHash } from "../../lib/navigation";
import { FormDialog } from "../../ui/FormDialog";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Select } from "../../ui/Select";
import { useToast } from "../../ui/Toast";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { EmptyState } from "../../ui/EmptyState";
import { CardFooter, Count } from "./kit";

type PlanTenant = Pick<TenantDiscountSummary, "tenant_id" | "tenant_name">;

function hasOffer(plan: TenantPlan): boolean {
  return (plan.discount.current?.discount_bps ?? 0) > 0 || plan.discount.upcoming.some((revision) => revision.discount_bps > 0);
}

function discountWords(revision: DiscountRevision | null): string {
  return revision && revision.discount_bps > 0 ? `减免 ${fmtPercent(revision.discount_bps)}` : "无折扣";
}

export function TenantPlans({ canEdit }: TabProps) {
  const [list, setList] = useState<TenantDiscountSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [create, setCreate] = useState(false);
  const [status, setStatus] = useState("offers");
  const listGeneration = useRef(0);
  const load = useCallback(async () => {
    const generation = ++listGeneration.current;
    try {
      const rows = await pricing.tenantPlans();
      if (listGeneration.current !== generation) return;
      setList(rows);
      setError(null);
    } catch (e) {
      if (listGeneration.current === generation) setError(errorText(e));
    }
  }, []);
  useEffect(() => { void load(); }, [load]);
  const records = useRecordTable({
    items: (list ?? []).filter((row) => status === "all" || hasOffer(row)),
    searchText: (row) => [row.tenant_name, row.tenant_id],
    resetKey: status,
  });
  const offers = list?.filter(hasOffer).length ?? 0;

  return <>
      <PageHeader title="租户方案" dataLead={list ? `${fmtInt(offers)} 个租户有当前或预约优惠，${fmtInt(list.length)} 个租户配置过折扣。` : "正在读取租户折扣。"} description="折扣从实际分组售价中减免，例如 20% 表示减免 20%、实付 80%。新折扣只影响现在或将来的计费。" actions={canEdit && <Button kind="primary" icon="plus" onClick={() => setCreate(true)}>新租户折扣</Button>} />
      <Card className="finance-table-card tenant-discount-list">
        <CardHeader title={<>租户折扣{list && <Count n={list.length} />}</>} description="点击租户查看完整修改记录；已恢复原价的租户可在「全部已配置」中查看。" />
        {error && <div className="note crit" role="alert">{error} <Button size="sm" onClick={() => void load()}>重试</Button></div>}
        {list === null ? (!error && <Loading />) : <>
          <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索租户折扣", placeholder: "租户名称或 ID" }}>
            <label className="f filter"><span>折扣状态</span><Select size="compact" aria-label="折扣状态" value={status} onValueChange={setStatus}><option value="offers">当前及预约优惠</option><option value="all">全部已配置</option></Select></label>
          </RecordTableToolbar>
          <DataTable label="租户折扣" tableClassName="tenant-discount-table">
            <colgroup><col className="tenant-discount-col-name" /><col className="tenant-discount-col-offer" /><col className="tenant-discount-col-time" /><col className="tenant-discount-col-next" /><col className="tenant-discount-col-status" /></colgroup>
            <thead><tr><th>租户</th><th>当前优惠</th><th>生效时间</th><th>下一次变更</th><th>状态</th></tr></thead>
            <tbody>
              {records.rows.length === 0 && <tr><td colSpan={5}><EmptyState text={status === "offers" ? "没有当前或预约优惠的租户。可切换「全部已配置」查看历史。" : "没有符合条件的已配置租户。"} /></td></tr>}
              {records.rows.map((row) => {
                const { current, upcoming } = row.discount;
                const next = upcoming[0];
                return <tr data-finance-record key={row.tenant_id}>
                  <td data-label="租户"><a className="link-button bl-name" href={tenantHash(row.tenant_id, "discounts", "pricing")}>{row.tenant_name}</a><span className="cell-note">{row.tenant_id}</span></td>
                  <td data-label="当前优惠">{discountWords(current)}</td>
                  <td data-label="生效时间" className="tabular">{current ? fmtBeijing(current.effective_from) : "未设置"}</td>
                  <td data-label="下一次变更">{next ? <>{discountWords(next)}<span className="cell-note tabular">{fmtBeijing(next.effective_from)}</span>{upcoming.length > 1 && <span className="cell-note">共 {fmtInt(upcoming.length)} 个预约版本</span>}</> : "—"}</td>
                  <td data-label="状态">{(current?.discount_bps ?? 0) > 0 ? "优惠生效中" : upcoming.some((revision) => revision.discount_bps > 0) ? "优惠已预约" : "无优惠"}</td>
                </tr>;
              })}
            </tbody>
          </DataTable>
          <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="租户折扣分页" />
        </>}
      </Card>
    {create && canEdit && <TenantPicker onClose={() => setCreate(false)} onSelect={(tenantId) => {
      setCreate(false);
      window.location.hash = `${tenantHash(tenantId, "discounts", "pricing")}&action=new-discount`;
    }} />}
  </>;
}

export function TenantDiscountSettings({ tenant, canEdit, refresh, openCreate = false }: { tenant: PlanTenant; canEdit: boolean; refresh: number; openCreate?: boolean }) {
  const [create, setCreate] = useState(openCreate);
  const [revision, setRevision] = useState(0);
  return <>
    <TenantDiscountDetail tenant={tenant} canEdit={canEdit} refresh={`${refresh}:${revision}`} onCreate={() => setCreate(true)} />
    {create && canEdit && <DiscountDialog tenant={tenant} onClose={() => setCreate(false)} onSaved={async () => {
      setRevision((value) => value + 1);
      setCreate(false);
    }} />}
  </>;
}

function TenantDiscountDetail({ tenant, refresh, canEdit, onCreate }: { tenant: PlanTenant; refresh: string; canEdit: boolean; onCreate(): void }) {
  const [plan, setPlan] = useState<TenantPlan | null>(null);
  const [history, setHistory] = useState<DiscountRevision[] | null>(null);
  const [before, setBefore] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [moreError, setMoreError] = useState<string | null>(null);
  const [reading, setReading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const requests = useRef({ generation: 0, refreshing: false, reading: false });
  const load = useCallback(async () => {
    const generation = ++requests.current.generation;
    requests.current.refreshing = true;
    requests.current.reading = false;
    setRefreshing(true);
    setReading(false);
    try {
      const [current, page] = await Promise.all([pricing.tenantPlan(tenant.tenant_id), tenantPricing.listDiscounts({ tenant_id: tenant.tenant_id, limit: 50 })]);
      if (requests.current.generation !== generation) return;
      setPlan(current);
      setHistory(page.items);
      setBefore(page.next_before);
      setError(null);
      setMoreError(null);
    } catch (e) {
      if (requests.current.generation === generation) setError(errorText(e));
    } finally {
      if (requests.current.generation === generation) {
        requests.current.refreshing = false;
        setRefreshing(false);
      }
    }
  }, [tenant.tenant_id]);
  useEffect(() => { void load(); }, [load, refresh]);

  async function more() {
    if (before === null || requests.current.reading || requests.current.refreshing) return;
    const generation = requests.current.generation;
    requests.current.reading = true;
    setReading(true);
    setMoreError(null);
    try {
      const page = await tenantPricing.listDiscounts({ tenant_id: tenant.tenant_id, before, limit: 50 });
      if (requests.current.generation !== generation) return;
      setHistory((previous) => [...(previous ?? []), ...page.items]);
      setBefore(page.next_before);
    } catch (e) {
      if (requests.current.generation === generation) setMoreError(errorText(e));
    } finally {
      if (requests.current.generation === generation) {
        requests.current.reading = false;
        setReading(false);
      }
    }
  }

  return <div role="region" aria-label={`租户折扣 ${tenant.tenant_name}`}>
    <div className="tenant-section-heading"><div><h2>租户折扣</h2><p className="hint">折扣从实际分组售价中减免；0% 表示恢复原价。历史账单保持原有价格。</p></div>{canEdit && <Button kind="primary" icon="plus" onClick={onCreate}>新折扣</Button>}</div>
    {error && <div className="note crit" role="alert">{error} <Button size="sm" onClick={() => void load()}>重试</Button></div>}
    <div className="tenant-discount-summary" role="region" aria-label="折扣">
      {plan ? <dl className="compact-overview-facts">
        <div><dt>当前优惠（减免）</dt><dd data-testid="current-discount">{plan.discount.current && plan.discount.current.discount_bps > 0 ? fmtPercent(plan.discount.current.discount_bps) : "无折扣"}</dd></div>
        <div><dt>生效时间</dt><dd className="text-body">{plan.discount.current ? fmtBeijing(plan.discount.current.effective_from) : "未设置"}</dd></div>
        <div><dt>预约版本</dt><dd>{fmtInt(plan.discount.upcoming.length)}</dd></div>
      </dl> : !error && <Loading />}
      {plan && plan.discount.upcoming.length > 0 && <ul className="tenant-discount-upcoming" aria-label="即将生效的折扣">{plan.discount.upcoming.map((revision) => <li key={revision.revision_id}>{fmtBeijing(revision.effective_from)} 起 {discountWords(revision)}</li>)}</ul>}
      {plan && plan.discount.upcoming.length === 0 && <p className="text-caption text-muted">没有预约的折扣版本。</p>}
    </div>
    <Card className="finance-table-card tenant-discount-history" aria-busy={refreshing}>
      <CardHeader title="折扣修改记录" description="按修改顺序倒序排列，包含已结束、当前与预约版本。时间均为北京时间。" />
      {history === null ? (!error && <Loading />) : <>
        <DataTable label="折扣修改记录" tableClassName="tenant-discount-history-table">
          <colgroup><col className="tenant-discount-history-col-offer" /><col className="tenant-discount-history-col-time" /><col className="tenant-discount-history-col-status" /><col className="tenant-discount-history-col-time" /><col className="tenant-discount-history-col-actor" /></colgroup>
          <thead><tr><th>折扣（减免）</th><th>生效时间</th><th>版本状态</th><th>修改时间</th><th>修改人</th></tr></thead>
          <tbody>{history.length === 0 && <tr><td colSpan={5}><EmptyState text="这个租户还没有折扣修改记录。" /></td></tr>}{history.map((revision) => <tr data-finance-record key={revision.revision_id}>
            <td data-label="折扣（减免）">{fmtPercent(revision.discount_bps)}{revision.discount_bps === 0 && <span className="cell-note">恢复原价</span>}</td>
            <td data-label="生效时间" className="tabular">{fmtBeijing(revision.effective_from)}</td>
            <td data-label="版本状态">{plan?.discount.current?.revision_id === revision.revision_id ? "当前生效" : plan?.discount.upcoming.some((item) => item.revision_id === revision.revision_id) ? "已预约" : Date.parse(revision.effective_from) > Date.now() ? "已被替代" : "已结束"}</td>
            <td data-label="修改时间" className="tabular">{fmtBeijing(revision.created_at)}</td>
            <td data-label="修改人">{revision.created_by ?? "—"}</td>
          </tr>)}</tbody>
        </DataTable>
        {moreError && <div className="note crit" role="alert">{moreError}</div>}
        <div className="tenant-discount-history-footer"><p className="hint" role="status">已读取 {fmtInt(history.length)} 条{before === null ? "，已显示全部修改记录。" : "，可继续读取更早记录。"}</p>{before !== null && <Button onClick={() => void more()} disabled={refreshing} busy={reading} busyLabel="读取中…">读取更早记录</Button>}</div>
      </>}
    </Card>
  </div>;
}

function TenantPicker({ onClose, onSelect }: { onClose(): void; onSelect(tenantId: string): void }) {
  const [list, setList] = useState<Tenant[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [tenantId, setTenantId] = useState("");
  const load = useCallback(async () => {
    try { setList(await tenants.list()); setError(null); }
    catch (cause) { setError(errorText(cause)); }
  }, []);
  useEffect(() => { void load(); }, [load]);
  const needle = query.trim().toLocaleLowerCase();
  const choices = (list ?? []).filter((item) => item.id === tenantId || !needle || item.name.toLocaleLowerCase().includes(needle) || item.id.toLocaleLowerCase().includes(needle));
  return <FormDialog open onOpenChange={(open) => { if (!open) onClose(); }} title="新租户折扣" description="选择租户后，在租户页设置折扣。">
    <form className="tenant-discount-form" aria-label="选择折扣租户" onSubmit={(event) => { event.preventDefault(); if (tenantId) onSelect(tenantId); }}>
      <Field label="搜索租户"><input type="search" autoComplete="off" placeholder="租户名称或 ID" value={query} onChange={(event) => setQuery(event.target.value)} /></Field>
      {error && <div className="note crit" role="alert">{error} <Button size="sm" onClick={() => void load()}>重试</Button></div>}
      <Field label="租户"><Select aria-label="租户" value={tenantId} onValueChange={setTenantId} disabled={list === null} options={[{ value: "", label: list === null ? "读取租户中…" : "请选择租户" }, ...choices.map((item) => ({ value: item.id, label: `${item.name} · ${item.id}` }))]} /></Field>
      {list !== null && choices.length === 0 && <p className="hint" role="status">没有匹配的租户，请修改搜索条件。</p>}
      <CardFooter><Button type="submit" kind="primary" disabled={!tenantId}>前往设置折扣</Button></CardFooter>
    </form>
  </FormDialog>;
}

function DiscountDialog({ tenant, onClose, onSaved }: { tenant: PlanTenant; onClose(): void; onSaved(tenant: PlanTenant): Promise<void> }) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const tenantId = tenant.tenant_id;
  const [percent, setPercent] = useState("");
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const savingRef = useRef(false);
  const title = "新折扣";

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (savingRef.current) return;
    setFailure(null);
    const bps = discountBps(percent);
    const rejected: Detail[] = [];
    if (!tenantId) rejected.push({ path: "/tenant_id", message: "请选择租户" });
    if (bps === null) rejected.push({ path: "/discount_bps", message: "折扣应是 0 到 100 的百分数，最多两位小数，例如 20 或 12.5" });
    if (rejected.length > 0 || bps === null) {
      setDetails(rejected);
      focusRefusal();
      return;
    }
    setDetails([]);
    savingRef.current = true;
    setSaving(true);
    try {
      const created = await sendAt(resolver.current, (effective_from) => tenantPricing.createDiscount({ tenant_id: tenantId, effective_from, discount_bps: bps }));
      if (created === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC+8 计" }]);
        focusRefusal();
        return;
      }
      toast("已保存");
      await onSaved(tenant);
    } catch (err) {
      if (err instanceof ApiError && err.status === 422 && err.details.length > 0) {
        setDetails(err.details);
        focusRefusal();
      } else {
        setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
      }
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  }

  const stray = details.filter((detail) => !["/tenant_id", "/discount_bps", "/effective_from"].includes(detail.path));
  return <FormDialog open onOpenChange={(open) => { if (!open) onClose(); }} busy={saving} title={title} description="只影响现在或将来开始生效的计费。历史账单保持原有价格。">
    <form ref={form} className="tenant-discount-form" aria-label={title} onSubmit={(event) => void save(event)} noValidate>
      <div className="tenant-discount-form-tenant"><span className="text-caption text-muted">租户</span><strong>{tenant.tenant_name}</strong></div>
      <Field label="折扣（%）" hint="填写减免比例，例如 20 表示减免 20%、实付 80%；填写 0 表示恢复原价。" error={problemAt(details, (path) => path === "/discount_bps")}>
        {(aria) => <input {...aria} inputMode="decimal" autoComplete="off" value={percent} onChange={(event) => setPercent(event.target.value)} />}
      </Field>
      <div className="tenant-discount-effective"><EffectiveFrom onResolve={(value) => { resolver.current = value; }} error={problemAt(details, (path) => path === "/effective_from")} /></div>
      {stray.length > 0 && <div className="note crit" role="alert">{stray.map((detail) => detail.message).join("；")}</div>}
      {failure && <div className="note crit" role="alert">{failure}</div>}
      <CardFooter><Button type="submit" kind="primary" busy={saving} busyLabel="保存中…">保存折扣</Button></CardFooter>
    </form>
  </FormDialog>;
}
