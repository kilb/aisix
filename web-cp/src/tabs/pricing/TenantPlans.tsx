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
import { errorText } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { Loading } from "../../ui/DataTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { useToast } from "../../ui/Toast";
import { discountBps, fmtPercent } from "../../lib/percent";

export function TenantPlans({ canEdit }: TabProps) {
  const [list, setList] = useState<Tenant[] | null>(null);
  const [tenantId, setTenantId] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    tenants.list().then(setList, (e) => setError(errorText(e)));
  }, []);

  return (
    <div className="panel">
      <h2>租户方案</h2>
      <p className="hint">
        折扣乘在按分组价计的每次计费上。模型的价格由这个租户的密钥所在的分组决定；价格只能从现在或将来开始生效。
      </p>
      {error && (
        <div className="note crit" role="alert">
          {error}
        </div>
      )}
      {list === null ? (
        !error && <Loading />
      ) : (
        <Field label="租户">
          <select value={tenantId} onChange={(e) => setTenantId(e.target.value)}>
            <option value="">请选择租户</option>
            {list.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
              </option>
            ))}
          </select>
        </Field>
      )}
      {tenantId && <Plan key={tenantId} tenantId={tenantId} canEdit={canEdit} />}
    </div>
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

  if (error) {
    return (
      <div className="note crit" role="alert">
        {error}{" "}
        <Button type="button" onClick={() => void load()}>
          重试
        </Button>
      </div>
    );
  }
  if (!plan) return <Loading />;
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
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      toast("已保存");
      setPercent("");
      await onSaved();
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
  return (
    <section className="plan-section" aria-label="折扣">
      <h3>折扣</h3>
      <p>
        现行折扣：<strong data-testid="current-discount">{current ? fmtPercent(current.discount_bps) : "无折扣"}</strong>
        {current && <span className="hint">（自 {fmtUtc(current.effective_from)} 起）</span>}
      </p>
      {upcoming.length > 0 && (
        <>
          <p>即将生效：</p>
          <ul className="plan-upcoming" aria-label="即将生效的折扣">
            {upcoming.map((d) => (
              <li key={d.revision_id}>
                {fmtUtc(d.effective_from)} 起 {fmtPercent(d.discount_bps)}
              </li>
            ))}
          </ul>
        </>
      )}
      {canEdit && (
        <form ref={form} className="plan-form" aria-label="新折扣" onSubmit={(e) => void save(e)} noValidate>
          <Field label="折扣（%）" error={problemAt(details, (p) => p === "/discount_bps")}>
            {(aria) => <input {...aria} inputMode="decimal" autoComplete="off" value={percent} onChange={(e) => setPercent(e.target.value)} />}
          </Field>
          <EffectiveFrom
            onResolve={(r) => {
              resolver.current = r;
            }}
            error={problemAt(details, (p) => p === "/effective_from")}
          />
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
          <p>
            <Button type="submit" kind="primary" busy={saving} busyLabel="保存中…">
              保存折扣
            </Button>
          </p>
        </form>
      )}
    </section>
  );
}
