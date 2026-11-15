import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import {
  ApiError,
  pricing,
  tenantPricing,
  tenants,
  type Detail,
  type Tenant,
  type TenantPlan,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtBeijing } from "../../lib/time";
import { FormDialog } from "../../ui/FormDialog";
import { Button } from "../../ui/Button";
import { Field, useRefusalFocus } from "../../ui/Field";
import { useToast } from "../../ui/Toast";
import { discountBps, fmtPercent } from "../../lib/percent";
import { Card, CardHeader, CardMessage, MetricSkeleton, PageHeader } from "../../components/page";
import { EmptyState } from "../../ui/EmptyState";
import { CardFooter, FormGrid } from "./kit";
import { Select } from "../../ui/Select";

export function TenantPlans({ canEdit }: TabProps) {
  const [list, setList] = useState<Tenant[] | null>(null);
  const [tenantId, setTenantId] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    tenants.list().then(setList, (e) => setError(errorText(e)));
  }, []);

  return (
    <>
      <PageHeader
        title="租户方案"
        eyebrow="价格 / 租户折扣"
        dataLead={tenantId ? `正在查看 ${list?.find((t) => t.id === tenantId)?.name ?? "所选租户"} 的现行与预约折扣。` : list ? `已载入 ${fmtInt(list.length)} 个租户，选择租户后查看其折扣。` : "读取可选租户。"}
        description="折扣乘在按分组价计的每次计费上。模型的价格由这个租户的密钥所在的分组决定；价格只能从现在或将来开始生效。"
        actions={
          list !== null && (
            <label className="f filter filter-wide">
              <span>租户</span>
              <Select size="compact" value={tenantId} onValueChange={(value) => setTenantId(value)}>
                <option value="">请选择租户</option>
                {list.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                  </option>
                ))}
              </Select>
            </label>
          )
        }
      />
      {error && (
        <div className="note crit mt-0" role="alert">
          {error}
        </div>
      )}
      {list === null ? (
        <Card><CardHeader title="租户折扣" />{error ? <CardMessage>租户列表读取失败，请刷新页面重试</CardMessage> : <MetricSkeleton />}</Card>
      ) : tenantId ? (
        <Plan key={tenantId} tenantId={tenantId} canEdit={canEdit} />
      ) : (
        <Card><dl className="compact-overview-facts"><div><dt>可选租户</dt><dd>{fmtInt(list.length)}</dd></div><div><dt>计费规则</dt><dd className="text-body">分组售价 × 实付比例</dd></div></dl><EmptyState text="选择一个租户，查看和设置它的折扣。" mark="route" /></Card>
      )}
    </>
  );
}

function Plan({ tenantId, canEdit }: { tenantId: string; canEdit: boolean }) {
  const [plan, setPlan] = useState<TenantPlan | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setPlan(await pricing.tenantPlan(tenantId));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [tenantId]);
  useEffect(() => {
    void load();
  }, [load]);

  if (!plan || error) {
    return (
      <Card><CardHeader title="折扣" description="现行折扣与预约版本 · 所选租户" />{error ? <CardMessage onRetry={() => void load()}><span role="alert">{error}</span></CardMessage> : <MetricSkeleton />}</Card>
    );
  }
  return <Discounts tenantId={tenantId} plan={plan} canEdit={canEdit} onSaved={load} />;
}

function Discounts({ tenantId, plan, canEdit, onSaved }: { tenantId: string; plan: TenantPlan; canEdit: boolean; onSaved: () => Promise<void> }) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [percent, setPercent] = useState("");
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const { current, upcoming } = plan.discount;

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setFailure(null);
    const bps = discountBps(percent);
    if (bps === null) {
      setDetails([{ path: "/discount_bps", message: "折扣应是 0 到 100 的百分数，最多两位小数，例如 20 或 12.5" }]);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      const created = await sendAt(resolver.current, (effective_from) =>
        tenantPricing.createDiscount({ tenant_id: tenantId, effective_from, discount_bps: bps }),
      );
      if (created === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC+8 计" }]);
        focusRefusal();
        return;
      }
      toast("已保存");
      setPercent("");
      await onSaved();
      setCreateOpen(false);
    } catch (err) {
      if (err instanceof ApiError && err.status === 422 && err.details.length > 0) {
        setDetails(err.details);
        focusRefusal();
      } else {
        setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
      }
    } finally {
      setSaving(false);
    }
  }

  const stray = details.filter((d) => d.path !== "/discount_bps" && d.path !== "/effective_from");
  const shown = (
    <Card className="mb-3" role="region" aria-label="折扣">
      <dl className="compact-overview-facts">
        <div><dt>现行折扣</dt><dd data-testid="current-discount">{current ? fmtPercent(current.discount_bps) : "无折扣"}</dd></div>
        <div><dt>生效时间</dt><dd className="text-body">{current ? fmtBeijing(current.effective_from) : "未设置"}</dd></div>
        <div><dt>预约版本</dt><dd>{fmtInt(upcoming.length)}</dd></div>
      </dl>
      <p className="text-caption text-muted">折扣从实际分组售价中扣除；未设置折扣时按分组售价计费。</p>
      {upcoming.length > 0 ? <ul className="mt-3 space-y-2 border-t border-line pt-3 text-caption tabular-nums" aria-label="即将生效的折扣">{upcoming.map((d) => <li key={d.revision_id}>{fmtBeijing(d.effective_from)} 起 {fmtPercent(d.discount_bps)}</li>)}</ul> : <p className="mt-2 text-caption text-muted">没有预约的折扣版本。</p>}
    </Card>
  );
  if (!canEdit) return shown;
  return (
    <>
      {shown}
      <div className="flex justify-end"><Button kind="primary" icon="plus" onClick={() => setCreateOpen(true)}>新折扣</Button></div>
      <FormDialog open={createOpen} onOpenChange={setCreateOpen} busy={saving} title="新折扣" description="只影响现在或将来开始生效的计费。历史账单保持原有价格。">
      <Card className="mb-0">
          <form ref={form} aria-label="新折扣" onSubmit={(e) => void save(e)} noValidate>
            <FormGrid>
              <Field label="折扣（%）" error={problemAt(details, (p) => p === "/discount_bps")}>
                {(aria) => (
                  <input {...aria} inputMode="decimal" autoComplete="off" value={percent} onChange={(e) => setPercent(e.target.value)} />
                )}
              </Field>
              <div className="flex flex-col gap-3 @lg:pt-7">
                <EffectiveFrom
                  onResolve={(r) => {
                    resolver.current = r;
                  }}
                  error={problemAt(details, (p) => p === "/effective_from")}
                />
              </div>
            </FormGrid>
            {stray.length > 0 && (
              <div className="note crit" role="alert">
                {stray.map((d) => d.message).join("；")}
              </div>
            )}
            {failure && (
              <div className="note crit" role="alert">
                {failure}
              </div>
            )}
            <CardFooter>
              <Button type="submit" kind="primary" busy={saving} busyLabel="保存中…">
                保存折扣
              </Button>
            </CardFooter>
          </form>
      </Card>
      </FormDialog>
    </>
  );
}
