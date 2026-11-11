import { useCallback, useEffect, useRef, useState } from "react";
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
import { errorText } from "../../lib/fmt";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { inForce, isZeroRate, scheduledWords } from "./GroupDetail";

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
  return (
    <div className="panel grp-detail" role="region" aria-label={`上游账号 ${name}`}>
      <h2>
        <Button size="sm" type="button" onClick={onBack}>
          返回成本价
        </Button>{" "}
        {name}
      </h2>
      <p className="hint">
        这个账号一次调用的成本是模型的官方美元价乘以这里的成本换算率。模型上的覆盖优先于账号默认；都没有时，成本记为未知。
      </p>
      <p className="tabular grp-sub">
        提供方 <span className="mono">{account.provider}</span>
      </p>
      {error && (
        <div className="note crit" role="alert">
          {error}{" "}
          <Button type="button" onClick={() => void load()}>
            重试
          </Button>
        </div>
      )}
      {canEdit && rates && (
        <DefaultCostRate
          account={account}
          plan={rates.default}
          onSaved={async () => {
            await Promise.all([load(), onChanged()]);
          }}
        />
      )}
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
                  <td className="trunc" title={row.name}>
                    {row.name}
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
    </div>
  );
}

/** Sends one cost-rate revision, and holds what the form needs to say about it. */
function useCostRateWrite(account: CostAccount, onSaved: () => Promise<void>) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLDivElement>();
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
    <div ref={write.form} className="grp-form grp-default" role="group" aria-label="修改默认成本换算率">
      <h3>默认成本换算率</h3>
      <p className="tabular">
        现在 <span className="grp-now">{plan.current?.cny_per_usd ?? "未设置"}</span>
        {later && <span className="cell-note">待生效 {later}</span>}
      </p>
      <Field
        label="新默认成本换算率"
        hint="每 1 美元官方价折合多少人民币成本，0 到 100，最多四位小数；0 表示这个账号的调用不计成本"
        error={problemAt(write.details, (p) => p === "/cny_per_usd")}
      >
        {(aria) => (
          <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
        )}
      </Field>
      <EffectiveFrom
        onResolve={(r) => {
          write.resolver.current = r;
        }}
        error={problemAt(write.details, (p) => p === "/effective_from")}
      />
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
      <p className="grp-actions">
        <TwoStep
          label="保存默认成本换算率"
          kind="primary"
          busy={write.saving}
          busyLabel="保存中…"
          ask={() => isZeroRate(rate)}
          warning="成本换算率 0 表示这个账号的调用成本记为 0，利润报表会把售价全部算作利润。确认无误再继续。"
          onConfirm={submit}
        />
      </p>
    </div>
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

  function submit() {
    if (!ending && rate.trim() === "") {
      write.refuse("/cny_per_usd", "请填写成本换算率");
      return;
    }
    void write.send(model.modelId, ending ? null : rate);
  }

  return (
    <div ref={write.form} className="grp-form grp-model-rate" role="group" aria-label={ending ? "结束覆盖" : "设置覆盖"}>
      <h3>{ending ? "结束覆盖" : "设置覆盖"}</h3>
      <p className="tabular">{model.name}</p>
      {ending ? (
        <p className="hint">结束后这个模型回到账号的默认成本换算率。</p>
      ) : (
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
      <EffectiveFrom
        onResolve={(r) => {
          write.resolver.current = r;
        }}
        error={problemAt(write.details, (p) => p === "/effective_from")}
      />
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
      <p className="grp-actions">
        <TwoStep
          label={ending ? "结束覆盖" : "保存覆盖"}
          kind={ending ? "secondary" : "primary"}
          busy={write.saving}
          busyLabel="保存中…"
          ask={() => !ending && isZeroRate(rate)}
          warning={`成本换算率 0 表示「${model.name}」经这个账号的调用成本记为 0，利润报表会把售价全部算作利润。确认无误再继续。`}
          onConfirm={submit}
        />
        <Button onClick={onClose}>关闭</Button>
      </p>
    </div>
  );
}
