import { useCallback, useEffect, useRef, useState } from "react";
import { EffectiveFrom, sendAt, type EffectiveResolver } from "../../components/EffectiveFrom";
import {
  ApiError,
  groupPrices,
  groups,
  margin,
  type Detail,
  type Group,
  type GroupRates,
  type MarginRow,
  type ModelGroupPriceRow,
  type RatePlan,
} from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtCnyPerMillion, fmtMicroCny, fmtUtc } from "../../lib/money";
import { BaselineSourceBadge } from "./Baselines";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import "./Groups.css";

/** How far back the realised rate is measured. */
const REALISED_DAYS = 30;

/** A rate typed by a person that buys nothing: `0`, however it is written. An empty field is not a rate at all. */
function isZeroRate(text: string): boolean {
  const typed = text.trim();
  return typed !== "" && /^0*(\.0*)?$/.test(typed);
}

/**
 * The 30-day realised rate, and beside it the part of the charge it leaves
 * out: rows whose cost is not known count in no ratio, so their charge is
 * named apart rather than hidden.
 */
function RealisedRate({ row }: { row: MarginRow | undefined }) {
  return (
    <>
      {row?.realised_cny_per_usd ?? "—"}
      {row !== undefined && row.rows_without_cost > 0 && (
        <span className="cell-note">
          另有 {row.rows_without_cost} 笔无成本，计 {fmtMicroCny(row.charged_without_cost_micro_cny)}
        </span>
      )}
    </>
  );
}

/** A plan's scheduled revisions, as their starts and what takes effect then. */
function scheduledWords(plan: RatePlan, word: string): string {
  return plan.upcoming.map((r) => `${fmtUtc(r.effective_from)} 起${word} ${r.cny_per_usd ?? "结束覆盖"}`).join("、");
}

/** The rate a row shows: the model's override while one is in force, else the group's default. */
function inForce(model: RatePlan, group: RatePlan): { label: string; rate: string | null } {
  const override = model.current?.cny_per_usd;
  if (override !== null && override !== undefined) return { label: "覆盖", rate: override };
  // An override whose current revision ends it, and no override at all, both
  // price at the group's default.
  return { label: "默认", rate: group.current?.cny_per_usd ?? null };
}

/**
 * One group's per-model rates: its official baseline in USD, the rate that
 * applies (the model's own override, else the group's default), the ¥/M that
 * follows, and the rate the group actually realised over the last 30 days.
 *
 * Every ¥ figure is the API's own decimal string. Nothing here recomputes one:
 * the control plane owns the arithmetic, and a second implementation on this
 * page would be a second answer.
 */
export function GroupDetail({
  group,
  canEdit,
  onBack,
  onChanged,
}: {
  group: Group;
  canEdit: boolean;
  onBack: () => void;
  onChanged: () => Promise<void>;
}) {
  const [rates, setRates] = useState<GroupRates | null>(null);
  const [prices, setPrices] = useState<Map<string, ModelGroupPriceRow>>(new Map());
  /** Each model's 30-day margin row: the realised rate, and the charge it leaves out for want of a cost. */
  const [realised, setRealised] = useState<Map<string, MarginRow>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<{ modelId: string; name: string; mode: "override" | "end" } | null>(null);

  const load = useCallback(async () => {
    const to = new Date();
    const from = new Date(to.getTime() - REALISED_DAYS * 86_400_000);
    try {
      const [plans, all, margins] = await Promise.all([
        groups.rates(group.id),
        groupPrices.list(),
        margin.list({ from: from.toISOString(), to: to.toISOString(), group_id: group.id }),
      ]);
      setRates(plans);
      setPrices(new Map(all.map((row) => [row.model_id, row])));
      setRealised(new Map(margins.filter((m) => m.model.id !== null).map((m) => [m.model.id as string, m])));
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, [group.id]);

  useEffect(() => {
    void load();
  }, [load]);

  // A model with no baseline cannot be priced at any rate, so those come first:
  // they are what an operator opens this page to find.
  const rows = rates ? [...rates.models].sort((a, b) => {
    const ap = prices.get(a.model_id)?.baseline.current ? 1 : 0;
    const bp = prices.get(b.model_id)?.baseline.current ? 1 : 0;
    return ap - bp || a.name.localeCompare(b.name);
  }) : [];

  return (
    <div className="panel grp-detail" role="region" aria-label={`分组 ${group.display_name}`}>
      <h2>
        <Button size="sm" type="button" onClick={onBack}>
          返回分组
        </Button>{" "}
        {group.display_name}
        {group.is_default && <span className="tag grp-default">默认分组</span>}
      </h2>
      <p className="hint">
        每个模型的人民币价是它的官方美元价乘以这里的换算率。租户按自己密钥所在这一个分组计价，模型上的覆盖优先于分组默认。
      </p>
      <p className="tabular grp-sub">
        路由名 <span className="mono">{group.name}</span>
        {group.description && <> · {group.description}</>}
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
        <DefaultRate
          group={group}
          plan={rates.default}
          onSaved={async () => {
            await Promise.all([load(), onChanged()]);
          }}
        />
      )}
      {rates === null ? (
        !error && <Loading />
      ) : rows.length === 0 ? (
        <EmptyState text="还没有模型。先在「价格 → 公共模型」中新建公共模型，或在「上游账号 → 模型」中获取上游模型。" />
      ) : (
        <DataTable label="分组换算率">
          <thead>
            <tr>
              <th>模型</th>
              <th className="num">基准价 输入</th>
              <th className="num">基准价 输出</th>
              <th>来源</th>
              <th>换算率</th>
              <th className="num">¥/百万 输入</th>
              <th className="num">¥/百万 输出</th>
              <th className="num">实际换算率（30 天）</th>
              <th className="right">操作</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const price = prices.get(row.model_id)?.groups.find((g) => g.group_id === group.id);
              const baseline = prices.get(row.model_id)?.baseline.current ?? null;
              const rate = inForce(row.rate, rates.default);
              const later = [scheduledWords(row.rate, "覆盖"), rate.label === "默认" ? scheduledWords(rates.default, "默认") : ""]
                .filter(Boolean)
                .join("、");
              const head = baseline?.card?.default?.base;
              return (
                <tr key={row.model_id} data-state={baseline ? "priced" : "unpriced"}>
                  <td className="trunc" title={row.name}>
                    {row.name}
                  </td>
                  <td className="num">
                    {!baseline ? <span className="grp-unpriced">未定价</span> : head?.input ? `$${head.input} / 百万 tokens` : "—"}
                  </td>
                  <td className="num">{!baseline ? "—" : head?.output ? `$${head.output} / 百万 tokens` : "—"}</td>
                  <td>{baseline ? <BaselineSourceBadge revision={baseline} /> : "—"}</td>
                  <td>
                    <span className={rate.label === "覆盖" ? "tag grp-override" : "tag"}>{rate.label}</span>{" "}
                    <span className="tabular">{rate.rate ?? "未设置"}</span>
                    {later && <span className="cell-note">待生效 {later}</span>}
                  </td>
                  <td className="num tabular">
                    {price?.input_per_million_cny === undefined ? "—" : fmtCnyPerMillion(price.input_per_million_cny)}
                  </td>
                  <td className="num tabular">
                    {price?.output_per_million_cny === undefined ? "—" : fmtCnyPerMillion(price.output_per_million_cny)}
                  </td>
                  <td className="num tabular">
                    <RealisedRate row={realised.get(row.model_id)} />
                  </td>
                  <td className="right">
                    {canEdit && (
                      <span className="row-actions">
                        <Button size="sm" onClick={() => setPicked({ modelId: row.model_id, name: row.name, mode: "override" })}>
                          设置覆盖
                        </Button>
                        {row.rate.current?.cny_per_usd !== null && row.rate.current !== null && (
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
        <ModelRate
          key={`${picked.modelId}-${picked.mode}`}
          group={group}
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

/** The group's own default rate: what every model without an override is priced at. */
function DefaultRate({ group, plan, onSaved }: { group: Group; plan: RatePlan; onSaved: () => Promise<void> }) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLDivElement>();
  const [rate, setRate] = useState("");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });

  async function submit() {
    if (saving) return;
    if (rate.trim() === "") {
      setDetails([{ path: "/cny_per_usd", message: "请填写换算率" }]);
      focusRefusal();
      return;
    }
    setSaving(true);
    setDetails([]);
    setFailure(null);
    try {
      const sent = await sendAt(resolver.current, (effective_from) =>
        groups.writeRate(group.id, { model_id: null, cny_per_usd: rate, effective_from }),
      );
      if (sent === "invalid") {
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC 计" }]);
        focusRefusal();
        return;
      }
      toast("已保存");
      setRate("");
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

  const later = scheduledWords(plan, "默认");
  const stray = details.filter((d) => !["/cny_per_usd", "/effective_from"].includes(d.path));
  return (
    <div ref={form} className="grp-form grp-default" role="group" aria-label="修改默认换算率">
      <h3>默认换算率</h3>
      <p className="tabular">
        现在 <span className="grp-now">{plan.current?.cny_per_usd ?? "未设置"}</span>
        {later && <span className="cell-note">待生效 {later}</span>}
      </p>
      <Field
        label="新默认换算率"
        hint="1 美元等于多少人民币，0 到 100，最多四位小数；换算率 0 表示这个分组免费"
        error={problemAt(details, (p) => p === "/cny_per_usd")}
      >
        {(aria) => (
          <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
        )}
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
      <p className="grp-actions">
        <TwoStep
          label="保存默认换算率"
          kind="primary"
          busy={saving}
          busyLabel="保存中…"
          ask={() => isZeroRate(rate)}
          warning={`换算率 0 表示这个分组下的调用不计价：影响 ${group.tenants} 个租户、${group.keys} 个密钥，生效期间的调用不收费，之后改回也不会补收。确认无误再继续。`}
          onConfirm={() => void submit()}
        />
      </p>
    </div>
  );
}

/** A model's override: set one, or end the one in force. */
function ModelRate({
  group,
  model,
  onClose,
  onSaved,
}: {
  group: Group;
  model: { modelId: string; name: string; mode: "override" | "end" };
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLDivElement>();
  const [rate, setRate] = useState("");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });

  const ending = model.mode === "end";

  async function submit() {
    if (saving) return;
    if (!ending && rate.trim() === "") {
      setDetails([{ path: "/cny_per_usd", message: "请填写换算率" }]);
      focusRefusal();
      return;
    }
    setSaving(true);
    setDetails([]);
    setFailure(null);
    try {
      const sent = await sendAt(resolver.current, (effective_from) =>
        groups.writeRate(group.id, {
          model_id: model.modelId,
          cny_per_usd: ending ? null : rate,
          effective_from,
        }),
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

  const stray = details.filter((d) => !["/cny_per_usd", "/effective_from"].includes(d.path));
  return (
    <div ref={form} className="grp-form grp-model-rate" role="group" aria-label={ending ? "结束覆盖" : "设置覆盖"}>
      <h3>{ending ? "结束覆盖" : "设置覆盖"}</h3>
      <p className="tabular">{model.name}</p>
      {ending ? (
        <p className="hint">结束后这个模型回到分组的默认换算率。</p>
      ) : (
        <Field
          label="换算率"
          hint="1 美元等于多少人民币，0 到 100，最多四位小数；要回到分组默认，请用「结束覆盖」"
          error={problemAt(details, (p) => p === "/cny_per_usd")}
        >
          {(aria) => (
            <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
          )}
        </Field>
      )}
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
      <p className="grp-actions">
        <TwoStep
          label={ending ? "结束覆盖" : "保存覆盖"}
          kind={ending ? "secondary" : "primary"}
          busy={saving}
          busyLabel="保存中…"
          ask={() => !ending && isZeroRate(rate)}
          warning={`换算率 0 表示「${model.name}」在这个分组下不计价：影响这个分组的 ${group.tenants} 个租户、${group.keys} 个密钥，生效期间的调用不收费，之后改回也不会补收。确认无误再继续。`}
          onConfirm={() => void submit()}
        />
        <Button onClick={onClose}>关闭</Button>
      </p>
    </div>
  );
}
