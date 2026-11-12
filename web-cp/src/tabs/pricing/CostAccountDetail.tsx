import { Fragment, useCallback, useEffect, useRef, useState } from "react";
import { ArrowLeft, ChevronDown } from "lucide-react";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import {
  ApiError,
  costRates,
  groupPrices,
  type AccountCostRates,
  type BaselineRevision,
  type CostAccount,
  type CostRatePlan,
  type Detail,
  type ModelGroupPriceRow,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { inForce, isZeroRate, scheduledWords } from "./GroupDetail";
import { BaselineSourceBadge } from "./Baselines";
import { PriceCardDetails } from "./PriceCardDetails";
import { CardFooter, Count, FormGrid, WithForms } from "./kit";

/**
 * One upstream account's cost rates, as a group's detail shows its rates:
 * each model the account serves with its official baseline in USD and the
 * cost rate that applies to it (the model's own override, else the
 * account's default), and the forms that change them.
 */
export function CostAccountDetail({
  account,
  canEdit,
  onBack,
  onChanged,
}: {
  account: CostAccount;
  canEdit: boolean;
  onBack: () => void;
  onChanged: () => Promise<void>;
}) {
  const [rates, setRates] = useState<AccountCostRates | null>(null);
  const [prices, setPrices] = useState<Map<string, ModelGroupPriceRow>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<{ modelId: string; name: string; mode: "override" | "end" } | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());

  const load = useCallback(async () => {
    try {
      const [plans, all] = await Promise.all([costRates.account(account.id), groupPrices.list()]);
      setRates(plans);
      setPrices(new Map(all.map((row) => [row.model_id, row])));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [account.id]);

  useEffect(() => {
    void load();
  }, [load]);

  const name = account.display_name || account.id;
  const unknown = rates?.models.filter((row) => inForce(row.rate, rates.default).rate === null).length ?? 0;
  const forms =
    canEdit && rates ? (
      <>
        <DefaultCostRate
          account={account}
          plan={rates.default}
          onSaved={async () => {
            await Promise.all([load(), onChanged()]);
          }}
        />
        {picked && (
          <ModelCostRate
            key={`${picked.modelId}-${picked.mode}`}
            account={account}
            model={picked}
            onClose={() => setPicked(null)}
            onSaved={async () => {
              setPicked(null);
              await Promise.all([load(), onChanged()]);
            }}
          />
        )}
      </>
    ) : null;
  return (
    <div className="grp-detail" role="region" aria-label={`上游账号 ${name}`}>
      <PageHeader
        title={name}
        eyebrow="价格 / 账号成本明细"
        dataLead={rates ? `${fmtInt(rates.models.length)} 个模型中，${fmtInt(unknown)} 个没有可用的成本换算率。` : "读取此账号的成本默认值与单模型覆盖。"}
        description={
          <>
            这个账号一次调用的成本是模型的官方美元价乘以这里的成本换算率。模型上的覆盖优先于账号默认；都没有时，成本记为未知。
            <span className="tabular mt-1 block">
              提供方 <span className="mono">{account.provider}</span>
            </span>
          </>
        }
        actions={
          <Button type="button" onClick={onBack}>
            <ArrowLeft aria-hidden />
            返回成本价
          </Button>
        }
      />
      {error && (
        <div className="note crit mt-0" role="alert">
          {error}{" "}
          <Button type="button" onClick={() => void load()}>
            重试
          </Button>
        </div>
      )}
      <BentoGrid className="mb-3">
        <Card className={span.d7}>
          <CardHeader number={1} title="账号服务模型 · 全部" />
          {rates ? <Metric size="hero">{fmtInt(rates.models.length)}</Metric> : error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton />}
          <dl className="mt-6 border-t border-ink/7 pt-4">
            <dt className="font-mono text-micro text-muted">现行默认成本换算率</dt>
            <dd className="mt-1 text-stat tabular-nums">{rates === null ? "—" : rates.default.current?.cny_per_usd ?? "未设置"}</dd>
          </dl>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="未设置成本换算率的模型" />
          {rates ? <Metric size="display">{fmtInt(unknown)}</Metric> : error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-caption text-on-inverse-muted">范围：此账号全部模型。默认与单模型覆盖均未设置时，成本记为未知；已设置换算率的调用仍需要生效的模型基准价。</p>
        </Card>
      </BentoGrid>
      <WithForms forms={forms}>
        <Card className="cost-model-card mb-0">
          <CardHeader number={3} className="mb-0" title={<>模型成本换算率{rates && <Count n={rates.models.length} />}</>} description="展开模型查看完整美元基准价、来源、生效时间与预约版本。" />
          {rates === null ? (
            !error && <Loading />
          ) : rates.models.length === 0 ? (
            <EmptyState text="这个账号还没有模型。先在「上游账号 → 模型」中为它勾选公共模型。" />
          ) : (
            <DataTable label="账号成本换算率">
              <colgroup>
                <col className="cost-col-model" />
                <col className="cost-col-price" />
                <col className="cost-col-price" />
                <col className="cost-col-rate" />
                <col className="cost-col-actions" />
              </colgroup>
              <thead>
                <tr>
                  <th>模型</th>
                  <th className="num">基准价 输入</th>
                  <th className="num">基准价 输出</th>
                  <th className="num">成本换算率</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {rates.models.map((row) => {
                  const plan = prices.get(row.model_id)?.baseline;
                  const baseline = plan?.current ?? null;
                  const head = baseline?.card?.default?.base;
                  const rate = inForce(row.rate, rates.default);
                  const open = expanded.has(row.model_id);
                  const detailId = `cost-price-${row.model_id}`;
                  const later = [scheduledWords(row.rate, "覆盖"), rate.label === "默认" ? scheduledWords(rates.default, "默认") : ""]
                    .filter(Boolean)
                    .join("、");
                  return (
                    <Fragment key={row.model_id}>
                      <tr>
                        <td className="font-medium">
                          <button
                            type="button"
                            className="cost-model-toggle"
                            aria-label={`${open ? "收起" : "展开"} ${row.name} 的价格明细`}
                            aria-expanded={open}
                            aria-controls={detailId}
                            onClick={() => setExpanded((previous) => {
                              const next = new Set(previous);
                              if (next.has(row.model_id)) next.delete(row.model_id);
                              else next.add(row.model_id);
                              return next;
                            })}
                          >
                            <ChevronDown aria-hidden />
                            <span>{row.name}</span>
                          </button>
                        </td>
                        <td className="num">
                          {!baseline ? <span className="grp-unpriced">未定价</span> : head?.input !== undefined ? <><span className="cost-price-amount">${head.input}</span><span className="cost-price-unit"> / 百万 tokens</span></> : "—"}
                        </td>
                        <td className="num">
                          {!baseline ? "—" : head?.output !== undefined ? <><span className="cost-price-amount">${head.output}</span><span className="cost-price-unit"> / 百万 tokens</span></> : "—"}
                        </td>
                        <td className="num">
                          <span className={rate.label === "覆盖" ? "tag grp-override" : "tag"}>{rate.label}</span>{" "}
                          <span className="tabular">{rate.rate ?? "未设置"}</span>
                          {later && <span className="cell-note">待生效 {later}</span>}
                        </td>
                        <td className="right">
                          {canEdit && (
                            <span className="row-actions cost-model-actions">
                              <Button size="sm" onClick={() => setPicked({ modelId: row.model_id, name: row.name, mode: "override" })}>
                                设置覆盖
                              </Button>
                              {row.rate.current !== null && row.rate.current.cny_per_usd !== null && (
                                <Button size="sm" onClick={() => setPicked({ modelId: row.model_id, name: row.name, mode: "end" })}>
                                  结束覆盖
                                </Button>
                              )}
                            </span>
                          )}
                        </td>
                      </tr>
                      {open && (
                        <tr className="dt-detail cost-expanded-row">
                          <td colSpan={5}>
                            <CostModelDetails id={detailId} name={row.name} modelId={row.model_id} modelPlan={row.rate} defaultPlan={rates.default} baseline={baseline} upcoming={plan?.upcoming ?? []} />
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  );
                })}
              </tbody>
            </DataTable>
          )}
        </Card>
      </WithForms>
    </div>
  );
}

/** 完整价卡在展开行内展示，美元基准、成本换算率与预约版本分别阅读。 */
function CostModelDetails({
  id, name, modelId, modelPlan, defaultPlan, baseline, upcoming,
}: {
  id: string;
  name: string;
  modelId: string;
  modelPlan: CostRatePlan;
  defaultPlan: CostRatePlan;
  baseline: BaselineRevision | null;
  upcoming: BaselineRevision[];
}) {
  const rate = inForce(modelPlan, defaultPlan);
  const currentRate = rate.label === "覆盖" ? modelPlan.current : defaultPlan.current;
  return (
    <div id={id} className="cost-model-details" role="region" aria-label={`${name} 成本与基准价明细`}>
      <p className="cost-model-identity">模型标识 <code>{modelId}</code></p>
      <section className="cost-detail-section" aria-label="现行完整基准价">
        <h3>现行基准价 <span>美元 · USD</span></h3>
        {baseline ? (
          <>
            <CostBaselineMeta revision={baseline} />
            <PriceCardDetails card={baseline.card} label={`${name} 现行完整基准价`} />
          </>
        ) : <p className="cost-detail-note">未定价：这个模型没有现行基准价，调用成本记为未知。</p>}
      </section>
      <section className="cost-detail-section" aria-label="现行成本换算率">
        <h3>现行成本换算率</h3>
        <dl className="cost-version-meta cost-rate-meta">
          <div><dt>换算率来源</dt><dd>{rate.label === "覆盖" ? "模型覆盖" : "账号默认"}</dd></div>
          <div><dt>人民币 / 美元</dt><dd className="cost-exact-rate">{rate.rate ?? "未设置"}</dd></div>
          <div><dt>生效时间</dt><dd>{currentRate ? fmtUtc(currentRate.effective_from) : "—"}</dd></div>
        </dl>
        <p className="cost-detail-note">{rate.rate === null ? "成本未知：模型覆盖与账号默认均未设置可用的成本换算率。" : baseline ? "调用成本 = 模型的美元基准价 × 此成本换算率。" : "已设置换算率；模型没有现行基准价，调用成本仍记为未知。"}</p>
      </section>
      <section className="cost-detail-section" aria-label="预约价格版本">
        <h3>预约版本 <span>按各版本的生效时间切换</span></h3>
        <div className="cost-rate-schedules">
          <CostRateSchedule label="模型覆盖" plan={modelPlan} />
          <CostRateSchedule label="账号默认" plan={defaultPlan} />
        </div>
        <h4 className="cost-upcoming-title">预约基准价 · {upcoming.length} 个版本</h4>
        {upcoming.length === 0 ? <p className="cost-detail-note">暂无预约基准价版本。</p> : upcoming.map((revision) => (
          <details className="cost-upcoming-baseline" key={revision.revision_id}>
            <summary><span>{fmtUtc(revision.effective_from)} 起</span><span>版本 #{revision.revision_id}</span></summary>
            <CostBaselineMeta revision={revision} />
            <PriceCardDetails card={revision.card} label={`${name} 预约基准价版本 ${revision.revision_id}`} />
          </details>
        ))}
      </section>
    </div>
  );
}

function CostBaselineMeta({ revision }: { revision: BaselineRevision }) {
  return (
    <dl className="cost-version-meta">
      <div><dt>生效时间 · 版本 #{revision.revision_id}</dt><dd>{fmtUtc(revision.effective_from)}</dd></div>
      <div><dt>价格来源</dt><dd><BaselineSourceBadge revision={revision} /></dd></div>
    </dl>
  );
}

function CostRateSchedule({ label, plan }: { label: string; plan: CostRatePlan }) {
  return (
    <div>
      <h4>{label} · {plan.upcoming.length} 个预约版本</h4>
      {plan.upcoming.length === 0 ? <p className="cost-detail-note">暂无预约版本。</p> : (
        <ul className="cost-rate-revisions">
          {plan.upcoming.map((revision) => (
            <li key={revision.revision_id}>
              <span>{fmtUtc(revision.effective_from)} 起</span>
              <span className="cost-exact-rate">{revision.cny_per_usd === null ? "结束覆盖，回到账号默认" : `${revision.cny_per_usd} 人民币 / 美元`}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Sends one cost-rate revision, and holds what the form needs to say about it. */
function useCostRateWrite(account: CostAccount, onSaved: () => Promise<void>) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLElement>();
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });

  async function send(modelId: string | null, rate: string | null): Promise<void> {
    if (saving) return;
    setSaving(true);
    setDetails([]);
    setFailure(null);
    try {
      const sent = await sendAt(resolver.current, (effective_from) =>
        costRates.write(account.id, { model_id: modelId, cny_per_usd: rate, effective_from }),
      );
      if (sent === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      toast("已保存");
      await onSaved();
    } catch (e) {
      if (e instanceof ApiError && e.status === 422 && e.details.length > 0) {
        setDetails(e.details);
        focusRefusal();
      } else {
        setFailure(e instanceof ApiError ? messageFor(e) : errorText(e));
      }
    } finally {
      setSaving(false);
    }
  }

  function refuse(path: string, message: string) {
    setDetails([{ path, message }]);
    focusRefusal();
  }

  const stray = details.filter((d) => !["/cny_per_usd", "/effective_from"].includes(d.path));
  return { form, saving, details, failure, resolver, send, refuse, stray };
}

/** The account's own default cost rate: what every model without an override is costed at. */
function DefaultCostRate({
  account,
  plan,
  onSaved,
}: {
  account: CostAccount;
  plan: CostRatePlan;
  onSaved: () => Promise<void>;
}) {
  const [rate, setRate] = useState("");
  const write = useCostRateWrite(account, async () => {
    setRate("");
    await onSaved();
  });

  function submit() {
    if (rate.trim() === "") {
      write.refuse("/cny_per_usd", "请填写成本换算率");
      return;
    }
    void write.send(null, rate);
  }

  const later = scheduledWords(plan, "默认");
  return (
    <Card ref={write.form} role="group" aria-label="修改默认成本换算率">
      <CardHeader
        number={4}
        title="默认成本换算率"
        description={
          <span className="tabular">
            现在 <span className="grp-now font-medium text-foreground">{plan.current?.cny_per_usd ?? "未设置"}</span>
            {later && <span className="mt-0.5 block text-caption">待生效 {later}</span>}
          </span>
        }
      />
      <FormGrid>
        <Field
          label="新默认成本换算率"
          hint="每 1 美元官方价折合多少人民币成本，0 到 100，最多四位小数；0 表示这个账号的调用不计成本"
          error={problemAt(write.details, (p) => p === "/cny_per_usd")}
        >
          {(aria) => (
            <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
          )}
        </Field>
        <div className="flex flex-col gap-3 @lg:pt-7">
          <EffectiveFrom
            onResolve={(r) => {
              write.resolver.current = r;
            }}
            error={problemAt(write.details, (p) => p === "/effective_from")}
          />
        </div>
      </FormGrid>
      {write.stray.length > 0 && (
        <div className="note crit" role="alert">
          {write.stray.map((d) => d.message).join("；")}
        </div>
      )}
      {write.failure && (
        <div className="note crit" role="alert">
          {write.failure}
        </div>
      )}
      <CardFooter>
        <TwoStep
          label="保存默认成本换算率"
          kind="primary"
          busy={write.saving}
          busyLabel="保存中…"
          ask={() => isZeroRate(rate)}
          warning="成本换算率 0 表示这个账号的调用成本记为 0，利润报表会把售价全部算作利润。确认无误再继续。"
          onConfirm={submit}
        />
      </CardFooter>
    </Card>
  );
}

/** A model's cost override on the account: set one, or end the one in force. */
function ModelCostRate({
  account,
  model,
  onClose,
  onSaved,
}: {
  account: CostAccount;
  model: { modelId: string; name: string; mode: "override" | "end" };
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const [rate, setRate] = useState("");
  const write = useCostRateWrite(account, onSaved);
  const ending = model.mode === "end";
  const { form } = write;
  // Above the table on a narrower screen, the form may open out of view of the row that asked for it.
  useEffect(() => {
    form.current?.scrollIntoView({ block: "nearest" });
  }, [form]);

  function submit() {
    if (!ending && rate.trim() === "") {
      write.refuse("/cny_per_usd", "请填写成本换算率");
      return;
    }
    void write.send(model.modelId, ending ? null : rate);
  }

  return (
    <Card ref={write.form} className="scroll-mt-20" role="group" aria-label={ending ? "结束覆盖" : "设置覆盖"}>
      <CardHeader number={5} title={ending ? "结束覆盖" : "设置覆盖"} description={<span className="tabular font-medium text-ink">{model.name}</span>} />
      {ending ? <p className="hint mb-4">结束后这个模型回到账号的默认成本换算率。</p> : null}
      <FormGrid>
        {!ending && (
          <Field
            label="成本换算率"
            hint="每 1 美元官方价折合多少人民币成本，0 到 100，最多四位小数；要回到账号默认，请用「结束覆盖」"
            error={problemAt(write.details, (p) => p === "/cny_per_usd")}
          >
            {(aria) => (
              <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
            )}
          </Field>
        )}
        <div className={ending ? "flex flex-col gap-3" : "flex flex-col gap-3 @lg:pt-7"}>
          <EffectiveFrom
            onResolve={(r) => {
              write.resolver.current = r;
            }}
            error={problemAt(write.details, (p) => p === "/effective_from")}
          />
        </div>
      </FormGrid>
      {write.stray.length > 0 && (
        <div className="note crit" role="alert">
          {write.stray.map((d) => d.message).join("；")}
        </div>
      )}
      {write.failure && (
        <div className="note crit" role="alert">
          {write.failure}
        </div>
      )}
      <CardFooter>
        <Button onClick={onClose}>关闭</Button>
        <TwoStep
          label={ending ? "结束覆盖" : "保存覆盖"}
          kind={ending ? "secondary" : "primary"}
          busy={write.saving}
          busyLabel="保存中…"
          ask={() => !ending && isZeroRate(rate)}
          warning={`成本换算率 0 表示「${model.name}」经这个账号的调用成本记为 0，利润报表会把售价全部算作利润。确认无误再继续。`}
          onConfirm={submit}
        />
      </CardFooter>
    </Card>
  );
}
