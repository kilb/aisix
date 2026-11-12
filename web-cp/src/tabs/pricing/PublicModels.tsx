import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import {
  ApiError,
  groupPrices,
  groups as groupsApi,
  publicModels,
  type Group,
  type ModelGroupPriceRow,
  type PublicModel,
} from "../../lib/api";
import { isUnknownOutcome } from "../../lib/creditOutcome";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { pricedState, unpricedReason } from "../../lib/publicPricing";
import { deleteRefusalText, deleteWarning } from "../../lib/retireWarning";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, CardMessage, ChartSkeleton, Metric, MetricSkeleton, PageHeader, span } from "../../components/page";
import { RankBars } from "../../components/charts";
import { Lamp } from "../../ui/Lamp";
import { PublicModelDetail } from "./PublicModelDetail";
import { CardFooter, Count, FormGrid, WithForms } from "./kit";

/** How many accounts serve a model, and at which priorities: `2 个（优先级 5、0）`. */
function tiersOf(model: PublicModel): string {
  const accounts = model.served_by.filter((s) => s.enabled);
  if (accounts.length === 0) return "没有可用账号";
  const tiers = [...new Set(accounts.map((s) => s.priority))].sort((a, b) => b - a);
  return `${accounts.length} 个（优先级 ${tiers.join("、")}）`;
}

/** What a restore does, asked before it runs. */
function restoreWarning(model: PublicModel): string {
  return `恢复「${model.name}」：它以原来的标识重新上线，授权、基准价和分组换算率照旧。原来的账号映射不会恢复：在上游账号的「模型」里勾选它，或用「获取上游模型 → 写入映射」。在此之前，请求会以 503 失败（无可用上游）。`;
}

/**
 * 价格 · 公共模型: the names tenants send, the accounts that serve them, and
 * whether each is priced.
 *
 * A public model's id is minted once and is the generated routing document's
 * id; grants, baselines and group rates all key on it. So a name is never
 * renamed and never adopted: a name a live model already holds is refused on
 * the name field, and the answer says the operator may use the model that
 * already has it.
 *
 * A live row's one action is 删除, and it does the whole job: the accounts'
 * mappings for the model go, then the documents, then the model is retired.
 * It offers no 停用 of its own, because every model a retire would accept is
 * one a delete accepts with the same writes — the state that differs is the
 * one a retire refuses, where an account still serves it, and unticking the
 * model on each account in turn was never what the operator wanted. A delete
 * is still refused while a tenant grant or a key selection names the model.
 *
 * A retired row's action is 恢复: the model comes back under its own id, with
 * no account serving it until one maps it again.
 *
 * Every delete or restore reads the list again however it ended: a refusal
 * may answer a model another operator changed, and a lost answer (no reply,
 * a 5xx) says nothing of whether the write landed, so the page says it could
 * not tell and shows what the control plane holds. The newest read wins.
 *
 * 定价 judges a model by the groups that serve it ([`pricedState`]); 授权租户
 * and 密钥 count who can call it now, all-scope tenants included — not the
 * narrower references a delete is refused on, which the delete's own warning
 * states.
 */
export function PublicModels({ canEdit }: TabProps) {
  const toast = useToast();
  const [rows, setRows] = useState<PublicModel[] | null>(null);
  const [prices, setPrices] = useState<Map<string, ModelGroupPriceRow> | null>(null);
  const [liveGroups, setLiveGroups] = useState<Group[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  /** The refusal belongs on the name field: a live model already holds the name. */
  const [atName, setAtName] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  /** The models a delete or restore is on its way for. */
  const [busy, setBusy] = useState<ReadonlySet<string>>(() => new Set());
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  /** The newest read asked for: an answer to an older one is dropped. */
  const latest = useRef(0);

  const load = useCallback(async () => {
    const n = ++latest.current;
    try {
      const [models, priced, groupList] = await Promise.all([publicModels.list(), groupPrices.list(), groupsApi.list()]);
      if (n !== latest.current) return;
      setRows(models);
      setPrices(new Map(priced.map((p) => [p.model_id, p] as const)));
      setLiveGroups(groupList);
      setError(null);
    } catch (e) {
      if (n !== latest.current) return;
      setError(errorText(e));
    }
  }, []);

  const mark = (id: string, on: boolean) =>
    setBusy((all) => {
      const next = new Set(all);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });

  useEffect(() => {
    void load();
  }, [load]);

  const open = rows?.find((m) => m.id === selected) ?? null;
  if (open) {
    return (
      <PublicModelDetail
        model={open}
        canEdit={canEdit}
        onBack={() => setSelected(null)}
        onChanged={load}
      />
    );
  }

  const nameRefusal = atName ? refusal : null;
  // 统计来自完整公共模型列表；定价只在用模型，沿用逐行的服务分组判断。
  const live = rows?.filter((model) => model.retired_at === null) ?? null;
  const unavailable = live?.filter((model) => !model.served_by.some((account) => account.enabled)) ?? null;
  const pricingCounts = live && prices ? { priced: 0, no_baseline: 0, missing_rates: 0 } : null;
  if (pricingCounts && live && prices) {
    for (const model of live) pricingCounts[pricedState(model, prices.get(model.id), liveGroups).kind] += 1;
  }

  async function create(e: React.FormEvent) {
    e.preventDefault();
    setCreating(true);
    setRefusal(null);
    setAtName(false);
    try {
      const model = await publicModels.create(name);
      setName("");
      toast(`已新建公共模型「${model.name}」。请在任一上游账号的「模型」里勾选它。`);
      await load();
    } catch (err) {
      if (err instanceof ApiError) {
        const onName = problemAt(err.details, (p) => p === "/name");
        setAtName(onName !== null);
        setRefusal(onName ?? messageFor(err));
      } else {
        setRefusal(errorText(err));
      }
      focusRefusal();
    } finally {
      setCreating(false);
    }
  }

  async function remove(model: PublicModel) {
    mark(model.id, true);
    setRefusal(null);
    try {
      await publicModels.delete(model.id);
      toast(`已删除「${model.name}」，各账号对它的映射和它的文档都已移除。`);
    } catch (e) {
      setRefusal(
        isUnknownOutcome(e)
          ? `没能确认「${model.name}」是否已删除，已重新读取列表。`
          : e instanceof ApiError
            ? deleteRefusalText(e)
            : errorText(e),
      );
    } finally {
      await load();
      mark(model.id, false);
    }
  }

  async function restore(model: PublicModel) {
    mark(model.id, true);
    setRefusal(null);
    try {
      await publicModels.restore(model.id);
      toast(`已恢复「${model.name}」。还没有账号服务它，请在上游账号的「模型」里重新映射。`);
    } catch (e) {
      setRefusal(
        isUnknownOutcome(e)
          ? `没能确认「${model.name}」是否已恢复，已重新读取列表。`
          : e instanceof ApiError
            ? `无法恢复「${model.name}」：${messageFor(e)}`
            : errorText(e),
      );
    } finally {
      await load();
      mark(model.id, false);
    }
  }

  return (
    <div className="public-models panel contents">
      <PageHeader
        eyebrow="模型管理 / 公共模型"
        title="公共模型"
        dataLead={live && unavailable ? <>当前有 {live.length.toLocaleString("zh-CN")} 个在用模型，其中 {unavailable.length.toLocaleString("zh-CN")} 个没有可用账号。先补齐账号映射，再核对基准价与服务分组换算率。</> : "读取完整模型列表后展示服务与定价状态。"}
        actions={
          <Button type="button" icon="refresh" onClick={() => void load()}>
            刷新
          </Button>
        }
      />
      <p className="mb-6 max-w-3xl text-body text-ink-soft">租户请求的模型名。标识只生成一次，授权、基准价和分组换算率都跟着它，所以名称不改、也不接管已有模型；显示名重复时请直接使用已有模型。</p>
      {error && (
        <div className="note crit mt-0" role="alert">
          读取失败：{error}
        </div>
      )}
      {refusal && !atName && (
        <div className="note crit mt-0" role="alert">
          {refusal}
        </div>
      )}
      <BentoGrid className="mb-3" aria-label="公共模型状态概览">
        <Card className={span.d7}>
          <CardHeader number={1} title="在用公共模型" description="完整列表 · 不含已停用模型" />
          {live ? <Metric size="hero">{live.length.toLocaleString("zh-CN")}</Metric> : error ? <CardMessage onRetry={() => void load()}>未能读取模型状态</CardMessage> : <MetricSkeleton />}
          {pricingCounts ? (
            <RankBars
              className="mt-6"
              items={[
                { label: "已定价", value: pricingCounts.priced, displayValue: `${pricingCounts.priced} 个` },
                { label: "缺少基准价", value: pricingCounts.no_baseline, displayValue: `${pricingCounts.no_baseline} 个` },
                { label: "缺少服务分组换算率", value: pricingCounts.missing_rates, displayValue: `${pricingCounts.missing_rates} 个` },
              ]}
              accessibleLabel={`在用模型 ${live?.length ?? 0} 个：已定价 ${pricingCounts.priced} 个，缺少基准价 ${pricingCounts.no_baseline} 个，缺少服务分组换算率 ${pricingCounts.missing_rates} 个`}
              emptyLabel="还没有在用公共模型"
            />
          ) : !error && <ChartSkeleton />}
          <CardFooter className="justify-start text-caption text-muted">
            {rows && live ? `全部 ${rows.length.toLocaleString("zh-CN")} 个 · 已停用 ${(rows.length - live.length).toLocaleString("zh-CN")} 个` : "定价状态与下方列表来自同一次读取"}
          </CardFooter>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="没有可用账号" description="在用模型 · 至少需要一个启用的账号映射" />
          {unavailable ? <Metric size="display">{unavailable.length.toLocaleString("zh-CN")}</Metric> : error ? <CardMessage onRetry={() => void load()}>未能读取账号状态</CardMessage> : <MetricSkeleton />}
          <p className="mt-4 text-body text-on-inverse-muted">无可用账号时，请求会以 503 失败。恢复模型不会恢复原账号映射，需要到上游账号重新接入。</p>
          <CardFooter className="justify-start border-surface/12 text-caption text-on-inverse-muted">账号数量和优先级可在模型详情中逐层核对。</CardFooter>
        </Card>
      </BentoGrid>
      <WithForms
        forms={
          canEdit ? (
            <Card className="mb-0">
              <CardHeader number={4} title="新建公共模型" description="新建后，请在任一上游账号的「模型」里勾选它。" />
              <form ref={form} aria-label="新建公共模型" onSubmit={(e) => void create(e)}>
                <FormGrid>
                  <Field label="名称" error={nameRefusal}>
                    {(aria) => (
                      <input
                        required
                        value={name}
                        disabled={creating}
                        placeholder="租户请求时填写的模型名"
                        onChange={(e) => {
                          setName(e.target.value);
                          setRefusal(null);
                          setAtName(false);
                        }}
                        {...aria}
                      />
                    )}
                  </Field>
                </FormGrid>
                <CardFooter>
                  <Button kind="primary" type="submit" busy={creating} busyLabel="新建中…">新建</Button>
                </CardFooter>
              </form>
            </Card>
          ) : undefined
        }
      >
        <Card className="mb-0">
          <CardHeader number={3} title={<>全部公共模型{rows && <Count n={rows.length} />}</>} description="名称与稳定标识保留；授权租户和密钥统计包含全部模型授权。" />
          {rows === null ? (
            error ? <CardMessage onRetry={() => void load()}>读取失败，请重试</CardMessage> : <ChartSkeleton />
          ) : rows.length === 0 ? (
            <EmptyState text="还没有公共模型。" />
          ) : (
            <DataTable label="公共模型" scroll>
              <thead>
                <tr>
                  <th>名称</th>
                  <th>服务账号</th>
                  <th>定价</th>
                  <th className="num">授权租户</th>
                  <th className="num">密钥</th>
                  <th>状态</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((m) => {
                  const priced = prices === null ? null : pricedState(m, prices.get(m.id), liveGroups);
                  const retired = m.retired_at !== null;
                  return (
                    <tr key={m.id} data-model={m.id}>
                      <td>
                        <button type="button" className="link-button" onClick={() => setSelected(m.id)}>
                          {m.name}
                        </button>
                        <span className="pm-model-id cell-note mono">{m.id}</span>
                      </td>
                      <td className={m.served_by.some((s) => s.enabled) ? undefined : "text-muted"}>{tiersOf(m)}</td>
                      <td>
                        {retired ? (
                          "—"
                        ) : priced === null ? (
                          "…"
                        ) : priced.kind === "priced" ? (
                          "已定价"
                        ) : (
                          <>
                            <span className="pm-unpriced">未定价</span>
                            <div className="cell-note">{unpricedReason(priced)}</div>
                          </>
                        )}
                      </td>
                      <td className="num">{m.reachable_tenants}</td>
                      <td className="num">{m.reachable_keys}</td>
                      <td>
                        <Lamp state={retired ? "disabled" : "applied"}>{retired ? "已停用" : "在用"}</Lamp>
                      </td>
                      <td className="right">
                        {canEdit && retired && (
                          <TwoStep
                            label="恢复"
                            size="sm"
                            busy={busy.has(m.id)}
                            busyLabel="恢复中…"
                            warning={restoreWarning(m)}
                            onConfirm={() => void restore(m)}
                          />
                        )}
                        {canEdit && !retired && (
                          <TwoStep
                            label="删除"
                            size="sm"
                            kind="danger"
                            busy={busy.has(m.id)}
                            busyLabel="删除中…"
                            // Always asked: refused, it says why; accepted, it cannot be undone.
                            ask={() => true}
                            warning={deleteWarning(m)}
                            onConfirm={() => void remove(m)}
                          />
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
