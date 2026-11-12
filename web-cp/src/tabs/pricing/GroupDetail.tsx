import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowLeft } from "lucide-react";
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
import { errorText, fmtInt } from "../../lib/fmt";
import { fmtCnyPerMillion, fmtMicroCny } from "../../lib/money";
import { fmtBeijing } from "../../lib/time";
import { BaselineSourceBadge } from "./Baselines";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { CardFooter, Count, FormGrid, WithForms } from "./kit";

/** How far back the margin is measured. */
const MARGIN_DAYS = 30;

/** A rate typed by a person that buys nothing: `0`, however it is written. An empty field is not a rate at all. */
export function isZeroRate(text: string): boolean {
  const typed = text.trim();
  return typed !== "" && /^0*(\.0*)?$/.test(typed);
}

/**
 * The 30-day margin: the profit as a share of the charge whose cost is
 * known, the charge, cost and profit it is measured on, and beside them the
 * part of the charge whose cost is not known, named apart rather than
 * hidden. Every figure is the API's own; nothing here is computed.
 */
function MarginCell({ row }: { row: MarginRow | undefined }) {
  if (row === undefined) return <>—</>;
  return (
    <>
      {row.margin_pct === null ? "—" : `${row.margin_pct}%`}
      {row.rows > 0 && (
        <span className="cell-note">
          售价 {fmtMicroCny(row.charged_with_cost_micro_cny)}，成本 {fmtMicroCny(row.cost_micro_cny)}，利润{" "}
          {fmtMicroCny(row.profit_micro_cny)}
        </span>
      )}
      {row.rows_without_cost > 0 && (
        <span className="cell-note">
          另有 {row.rows_without_cost} 笔成本未知，计 {fmtMicroCny(row.charged_without_cost_micro_cny)}
        </span>
      )}
    </>
  );
}

/** What the helpers below read of a plan: a group's rate plan, or an account's cost-rate plan. */
type Plan = {
  current: { cny_per_usd: string | null } | null;
  upcoming: { effective_from: string; cny_per_usd: string | null }[];
};

/** A plan's scheduled revisions, as their starts and what takes effect then. */
export function scheduledWords(plan: Plan, word: string): string {
  return plan.upcoming.map((r) => `${fmtBeijing(r.effective_from)} 起${word} ${r.cny_per_usd ?? "结束覆盖"}`).join("、");
}

/** The rate a row shows: the model's override while one is in force, else the default. */
export function inForce(model: Plan, group: Plan): { label: string; rate: string | null } {
  const override = model.current?.cny_per_usd;
  if (override !== null && override !== undefined) return { label: "覆盖", rate: override };
  // An override whose current revision ends it, and no override at all, both
  // price at the group's default.
  return { label: "默认", rate: group.current?.cny_per_usd ?? null };
}

/**
 * One group's per-model rates: its official baseline in USD, the rate that
 * applies (the model's own override, else the group's default), the ¥/M that
 * follows, and the margin the group made on it over the last 30 days.
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
  /** Each model's 30-day margin row: its profit and margin, and the charge they leave out for want of a cost. */
  const [margins, setMargins] = useState<Map<string, MarginRow>>(new Map());
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState<{ modelId: string; name: string; mode: "override" | "end" } | null>(null);

  const load = useCallback(async () => {
    const to = new Date();
    const from = new Date(to.getTime() - MARGIN_DAYS * 86_400_000);
    try {
      const [plans, all, rows] = await Promise.all([
        groups.rates(group.id),
        groupPrices.list(),
        margin.list({ from: from.toISOString(), to: to.toISOString(), group_id: group.id }),
      ]);
      setRates(plans);
      setPrices(new Map(all.map((row) => [row.model_id, row])));
      setMargins(new Map(rows.filter((m) => m.model.id !== null).map((m) => [m.model.id as string, m])));
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
  const [priceState, setPriceState] = useState("");
  const records = useRecordTable({
    items: rows.filter((row) => priceState === "" || (priceState === "baseline" ? Boolean(prices.get(row.model_id)?.baseline.current) : priceState === "missing" ? !prices.get(row.model_id)?.baseline.current : row.rate.current?.cny_per_usd != null)),
    searchText: (row) => [row.name, row.model_id],
    resetKey: `${group.id}/${priceState}`,
  });

  const forms =
    canEdit && rates ? (
      <>
        <DefaultRate
          group={group}
          plan={rates.default}
          onSaved={async () => {
            await Promise.all([load(), onChanged()]);
          }}
        />
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
      </>
    ) : null;
  const later = scheduledWords(group.default_rate, "默认");
  const unpriced = rows.filter((row) => !prices.get(row.model_id)?.baseline.current).length;

  return (
    <div className="grp-detail" role="region" aria-label={`分组 ${group.display_name}`}>
      <PageHeader
        eyebrow="价格 / 分组明细"
        dataLead={rates ? `${group.display_name} 的 ${fmtInt(rows.length)} 个模型中，${fmtInt(unpriced)} 个缺少现行基准价。` : "读取分组模型换算率与最近 30 天毛利。"}
        title={
          <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
            <span className="min-w-0 break-all">{group.display_name}</span>
            {group.is_default && <span className="tag grp-default ml-0">默认分组</span>}
          </span>
        }
        description={
          <>
            每个模型的人民币价是它的官方美元价乘以这里的换算率。租户按自己密钥所在这一个分组计价，模型上的覆盖优先于分组默认。
            <span className="tabular mt-1 block">
              路由名 <span className="mono">{group.name}</span>
              {group.description && <> · {group.description}</>}
            </span>
          </>
        }
        actions={
          <Button type="button" onClick={onBack}>
            <ArrowLeft aria-hidden />
            返回分组
          </Button>
        }
      />
      <BentoGrid className="mb-3">
        <Card className={span.d7}>
          <CardHeader number={1} title="默认换算率 · 当前分组" />
          <Metric size={group.default_rate.current?.cny_per_usd == null ? "value" : "hero"}>{group.default_rate.current?.cny_per_usd ?? "未设置"}</Metric>
          <p className="mt-4 text-caption text-muted">{later ? `待生效 ${later}` : "没有预约的默认换算率版本。"}</p>
          <dl className="mt-6 grid grid-cols-3 gap-3 border-t border-ink/7 pt-4">
            <div><dt className="font-mono text-micro text-muted">租户</dt><dd className="mt-1 text-stat tabular-nums">{fmtInt(group.tenants)}</dd></div>
            <div><dt className="font-mono text-micro text-muted">密钥</dt><dd className="mt-1 text-stat tabular-nums">{fmtInt(group.keys)}</dd></div>
            <div><dt className="font-mono text-micro text-muted">上游账号</dt><dd className="mt-1 text-stat tabular-nums">{fmtInt(group.provider_keys.length)}</dd></div>
          </dl>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="缺少现行基准价的模型" />
          {rates ? <Metric size="display">{fmtInt(unpriced)}</Metric> : error ? <CardMessage onRetry={() => void load()}>数据读取失败</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-caption text-on-inverse-muted">范围：此分组全部 {rates ? fmtInt(rows.length) : "—"} 个模型。未定价模型排在明细最前。</p>
          <p className="mt-6 border-t border-surface/12 pt-4 text-caption text-on-inverse-muted">毛利明细只覆盖最近 30 天；已知成本与成本未知收入分开列出。</p>
        </Card>
      </BentoGrid>
      {error && (
        <div className="note crit mt-0" role="alert">
          {error}{" "}
          <Button type="button" onClick={() => void load()}>
            重试
          </Button>
        </div>
      )}
      <WithForms wide forms={forms}>
        <Card className="mb-0">
          <CardHeader number={3} className="mb-0" title={<>模型换算率{rates && <Count n={rows.length} />}</>} />
          {rates === null ? (
            !error && <Loading />
          ) : rows.length === 0 ? (
            <EmptyState text="还没有模型。先在「价格 → 公共模型」中新建公共模型，或在「上游账号 → 模型」中获取上游模型。" />
          ) : (
            <>
            <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索分组模型", placeholder: "模型名称或 ID" }}>
              <label className="f filter"><span>价格状态</span><select value={priceState} onChange={(e) => setPriceState(e.target.value)}><option value="">全部</option><option value="baseline">已有基准价</option><option value="missing">缺少基准价</option><option value="override">已设单模型覆盖</option></select></label>
            </RecordTableToolbar>
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
                  <th className="num">毛利（30 天）</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {records.rows.length === 0 && <tr><td colSpan={9}><EmptyState text="没有符合筛选条件的模型" /></td></tr>}
                {records.rows.map((row) => {
                  const price = prices.get(row.model_id)?.groups.find((g) => g.group_id === group.id);
                  const baseline = prices.get(row.model_id)?.baseline.current ?? null;
                  const rate = inForce(row.rate, rates.default);
                  const later = [scheduledWords(row.rate, "覆盖"), rate.label === "默认" ? scheduledWords(rates.default, "默认") : ""]
                    .filter(Boolean)
                    .join("、");
                  const head = baseline?.card?.default?.base;
                  return (
                    <tr key={row.model_id} data-state={baseline ? "priced" : "unpriced"}>
                      <td className="font-medium">
                        <span className="trunc" title={row.name}>
                          {row.name}
                        </span>
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
                        <MarginCell row={margins.get(row.model_id)} />
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
            <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="分组模型换算率分页" />
            </>
          )}
        </Card>
      </WithForms>
    </div>
  );
}

/** The group's own default rate: what every model without an override is priced at. */
function DefaultRate({ group, plan, onSaved }: { group: Group; plan: RatePlan; onSaved: () => Promise<void> }) {
  const toast = useToast();
  const [form, focusRefusal] = useRefusalFocus<HTMLElement>();
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
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC+8 计" }]);
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
    <Card ref={form} role="group" aria-label="修改默认换算率">
      <CardHeader
        number={4}
        title="默认换算率"
        description={
          <span className="tabular">
            现在 <span className="grp-now font-medium text-foreground">{plan.current?.cny_per_usd ?? "未设置"}</span>
            {later && <span className="mt-0.5 block text-caption">待生效 {later}</span>}
          </span>
        }
      />
      <FormGrid>
        <Field
          label="新默认换算率"
          hint="1 美元等于多少人民币，0 到 100，最多四位小数；换算率 0 表示这个分组免费"
          error={problemAt(details, (p) => p === "/cny_per_usd")}
        >
          {(aria) => (
            <input {...aria} inputMode="decimal" autoComplete="off" value={rate} onChange={(e) => setRate(e.target.value)} />
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
        <TwoStep
          label="保存默认换算率"
          kind="primary"
          busy={saving}
          busyLabel="保存中…"
          ask={() => isZeroRate(rate)}
          warning={`换算率 0 表示这个分组下的调用不计价：影响 ${group.tenants} 个租户、${group.keys} 个密钥，生效期间的调用不收费，之后改回也不会补收。确认无误再继续。`}
          onConfirm={() => void submit()}
        />
      </CardFooter>
    </Card>
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
  const [form, focusRefusal] = useRefusalFocus<HTMLElement>();
  const [rate, setRate] = useState("");
  const [saving, setSaving] = useState(false);
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const resolver = useRef<EffectiveResolver>({ immediate: true, stamp: () => new Date().toISOString() });

  const ending = model.mode === "end";
  // Above the table on a narrower screen, the form may open out of view of the row that asked for it.
  useEffect(() => {
    form.current?.scrollIntoView({ block: "nearest" });
  }, [form]);

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
        setDetails([{ path: "/effective_from", message: "生效时间应写成 年-月-日 时:分，按 UTC+8 计" }]);
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
    <Card ref={form} className="scroll-mt-20" role="group" aria-label={ending ? "结束覆盖" : "设置覆盖"}>
      <CardHeader number={5} title={ending ? "结束覆盖" : "设置覆盖"} description={<span className="tabular font-medium text-ink">{model.name}</span>} />
      {ending ? (
        <p className="hint mb-4">结束后这个模型回到分组的默认换算率。</p>
      ) : null}
      <FormGrid>
        {!ending && (
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
        <div className={ending ? "flex flex-col gap-3" : "flex flex-col gap-3 @lg:pt-7"}>
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
        <Button onClick={onClose}>关闭</Button>
        <TwoStep
          label={ending ? "结束覆盖" : "保存覆盖"}
          kind={ending ? "secondary" : "primary"}
          busy={saving}
          busyLabel="保存中…"
          ask={() => !ending && isZeroRate(rate)}
          warning={`换算率 0 表示「${model.name}」在这个分组下不计价：影响这个分组的 ${group.tenants} 个租户、${group.keys} 个密钥，生效期间的调用不收费，之后改回也不会补收。确认无误再继续。`}
          onConfirm={() => void submit()}
        />
      </CardFooter>
    </Card>
  );
}
