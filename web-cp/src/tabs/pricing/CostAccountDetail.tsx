import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowLeft } from "lucide-react";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import {
  ApiError,
  costRates,
  groupPrices,
  type AccountCostRates,
  type CostAccount,
  type CostRatePlan,
  type Detail,
  type ModelGroupPriceRow,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText, fmtInt } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { inForce, isZeroRate, scheduledWords } from "./GroupDetail";
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
        <Card className="mb-0">
          <CardHeader number={3} className="mb-0" title={<>模型成本换算率{rates && <Count n={rates.models.length} />}</>} />
          {rates === null ? (
            !error && <Loading />
          ) : rates.models.length === 0 ? (
            <EmptyState text="这个账号还没有模型。先在「上游账号 → 模型」中为它勾选公共模型。" />
          ) : (
            <DataTable label="账号成本换算率">
              <thead>
                <tr>
                  <th>模型</th>
                  <th className="num">基准价 输入</th>
                  <th className="num">基准价 输出</th>
                  <th>成本换算率</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {rates.models.map((row) => {
                  const baseline = prices.get(row.model_id)?.baseline.current ?? null;
                  const head = baseline?.card?.default?.base;
                  const rate = inForce(row.rate, rates.default);
                  const later = [scheduledWords(row.rate, "覆盖"), rate.label === "默认" ? scheduledWords(rates.default, "默认") : ""]
                    .filter(Boolean)
                    .join("、");
                  return (
                    <tr key={row.model_id}>
                      <td className="font-medium">
                        <span className="trunc" title={row.name}>
                          {row.name}
                        </span>
                      </td>
                      <td className="num">
                        {!baseline ? <span className="grp-unpriced">未定价</span> : head?.input ? `$${head.input} / 百万 tokens` : "—"}
                      </td>
                      <td className="num">{!baseline ? "—" : head?.output ? `$${head.output} / 百万 tokens` : "—"}</td>
                      <td>
                        <span className={rate.label === "覆盖" ? "tag grp-override" : "tag"}>{rate.label}</span>{" "}
                        <span className="tabular">{rate.rate ?? "未设置"}</span>
                        {later && <span className="cell-note">待生效 {later}</span>}
                      </td>
                      <td className="right">
                        {canEdit && (
                          <span className="row-actions">
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
