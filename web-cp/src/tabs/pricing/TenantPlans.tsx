import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import { RateCardEditor } from "../../components/RateCardEditor";
import {
  ApiError,
  pricing,
  tenantPricing,
  tenants,
  type Detail,
  type PriceRow,
  type RateCard,
  type SellRevision,
  type Tenant,
  type TenantPlan,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import "./TenantPlans.css";

/** `折扣（%）` typed with at most 2 decimals, 0 to 100, as basis points; null when it is not such a number. */
export function discountBps(text: string): number | null {
  const m = /^(\d{1,3})(?:\.(\d{1,2}))?$/.exec(text.trim());
  if (!m) return null;
  const bps = Number(m[1]) * 100 + Number((m[2] ?? "").padEnd(2, "0"));
  return bps <= 10_000 ? bps : null;
}

/** Basis points as a percentage in integer arithmetic: 2000 is `20%`, 1250 is `12.5%`. */
export function fmtPercent(bps: number): string {
  const frac = String(bps % 100).padStart(2, "0").replace(/0+$/, "");
  return `${Math.floor(bps / 100)}${frac ? `.${frac}` : ""}%`;
}

function rates(card: RateCard | null): string {
  const base = card?.default?.base;
  if (!base || base.input === undefined || base.output === undefined) return "—";
  return `输入 ${base.input} 输出 ${base.output}`;
}

/** One revision of an override, for the list of those still to start. */
function upcomingText(r: SellRevision): string {
  return `${fmtUtc(r.effective_from)} 起：${r.card ? rates(r.card) : "结束专属价"}`;
}

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
      <p className="hint">折扣乘在每次计费上；专属价取代该租户在某个模型上的平台价。价格只能从现在或将来开始生效。</p>
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
      setPlan(await tenantPricing.tenantPlan(tenantId));
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
  return (
    <>
      <Discounts tenantId={tenantId} plan={plan} canEdit={canEdit} onSaved={load} />
      <Overrides tenantId={tenantId} plan={plan} canEdit={canEdit} onSaved={load} />
    </>
  );
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

function Overrides({ tenantId, plan, canEdit, onSaved }: { tenantId: string; plan: TenantPlan; canEdit: boolean; onSaved: () => Promise<void> }) {
  const [models, setModels] = useState<PriceRow[]>([]);
  const [editing, setEditing] = useState<string | null>(null);
  const [ending, setEnding] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const toast = useToast();

  useEffect(() => {
    // The models to add an override for; the page works without them.
    pricing.models().then(setModels, () => undefined);
  }, []);

  async function end(modelId: string) {
    setEnding(modelId);
    setFailure(null);
    try {
      const resolver: EffectiveResolver = { immediate: true, stamp: () => new Date().toISOString() };
      await sendAt(resolver, (effective_from) =>
        pricing.createSell({ model_id: modelId, tenant_id: tenantId, effective_from, card: null }),
      );
      toast("已结束专属价");
      await onSaved();
    } catch (err) {
      setFailure(err instanceof ApiError ? messageFor(err) : errorText(err));
    } finally {
      setEnding(null);
    }
  }

  const overridden = new Set(plan.overrides.map((o) => o.model_id));
  const addable = models.filter((m) => !overridden.has(m.model_id));

  return (
    <section className="plan-section" aria-label="专属价">
      <h3>专属价</h3>
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      {plan.overrides.length === 0 ? (
        <EmptyState text="这个租户还没有专属价，按平台价计费。" />
      ) : (
        <DataTable label="专属价">
          <thead>
            <tr>
              <th>模型</th>
              <th>现行</th>
              {canEdit && <th>新专属价</th>}
              {canEdit && <th>结束专属价</th>}
            </tr>
          </thead>
          <tbody>
            {plan.overrides.map((o) => {
              const active = o.current?.card ?? null;
              return (
                <tr key={o.model_id} data-state={active ? "override" : "ended"}>
                  <td>{o.name}</td>
                  <td>
                    {active ? (
                      <span className="tabular">{rates(active)}</span>
                    ) : (
                      <span className="plan-ended">{o.current ? "已结束，按平台价" : "尚未生效"}</span>
                    )}
                    {o.upcoming.length > 0 && (
                      <ul className="plan-upcoming" aria-label={`${o.name} 即将生效`}>
                        {o.upcoming.map((u) => (
                          <li key={u.revision_id}>{upcomingText(u)}</li>
                        ))}
                      </ul>
                    )}
                  </td>
                  {canEdit && (
                    <td>
                      {editing === o.model_id ? (
                        <OverrideForm
                          tenantId={tenantId}
                          modelId={o.model_id}
                          label={`${o.name} 的新专属价`}
                          initial={active}
                          onSaved={async () => {
                            setEditing(null);
                            await onSaved();
                          }}
                        />
                      ) : (
                        <Button size="sm" type="button" aria-label={`为 ${o.name} 设置新专属价`} onClick={() => setEditing(o.model_id)}>
                          改价
                        </Button>
                      )}
                    </td>
                  )}
                  {canEdit && (
                    <td>
                      {active && (
                        <TwoStep
                          size="sm"
                          label="结束专属价"
                          busy={ending === o.model_id}
                          warning="结束后，这个模型按平台价计费。"
                          onConfirm={() => void end(o.model_id)}
                        />
                      )}
                    </td>
                  )}
                </tr>
              );
            })}
          </tbody>
        </DataTable>
      )}
      {canEdit && (
        <div className="plan-form">
          <h3>添加专属价</h3>
          <OverrideForm tenantId={tenantId} modelId={null} choices={addable} label="添加专属价" initial={null} onSaved={onSaved} />
        </div>
      )}
    </section>
  );
}

function OverrideForm({
  tenantId,
  modelId,
  choices,
  label,
  initial,
  onSaved,
}: {
  tenantId: string;
  /** The model the price is for; null lets the form pick one of `choices`. */
  modelId: string | null;
  choices?: PriceRow[];
  label: string;
  initial: RateCard | null;
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [picked, setPicked] = useState("");
  const [card, setCard] = useState<RateCard | null>(initial);
  const [cardOk, setCardOk] = useState(true);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const target = modelId ?? picked;

  async function save(e: React.FormEvent) {
    e.preventDefault();
    if (saving) return;
    setFailure(null);
    const refused: Detail[] = [];
    if (!target) refused.push({ path: "/model_id", message: "请选择模型" });
    if (!cardOk) refused.push({ path: "/card", message: "价格卡 JSON 不合法，修改尚未生效；请先改正" });
    if (refused.length > 0) {
      setDetails(refused);
      focusRefusal();
      return;
    }
    setDetails([]);
    setSaving(true);
    try {
      const created = await sendAt(resolver.current, (effective_from) =>
        pricing.createSell({ model_id: target, tenant_id: tenantId, effective_from, card: card ?? {} }),
      );
      if (created === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      toast("已保存");
      setCard(null);
      setPicked("");
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

  const stray = details.filter((d) => !(d.path.startsWith("/card") || d.path === "/effective_from" || d.path === "/model_id"));
  return (
    <form ref={form} aria-label={label} onSubmit={(e) => void save(e)} noValidate>
      {modelId === null && (
        <Field label="模型" error={problemAt(details, (p) => p === "/model_id")}>
          {(aria) => (
            <select {...aria} value={picked} onChange={(e) => setPicked(e.target.value)}>
              <option value="">请选择模型</option>
              {(choices ?? []).map((m) => (
                <option key={m.model_id} value={m.model_id}>
                  {m.name}
                </option>
              ))}
            </select>
          )}
        </Field>
      )}
      <RateCardEditor value={card} onChange={setCard} details={details} onValidity={setCardOk} />
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
          保存专属价
        </Button>
      </p>
    </form>
  );
}
