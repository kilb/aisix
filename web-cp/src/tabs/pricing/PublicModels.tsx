import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { TabProps } from "../../App";
import { Trash2, Undo2 } from "lucide-react";
import {
  ApiError,
  groupPrices,
  groups as groupsApi,
  publicModels,
  type Group,
  type ModelGroupPriceRow,
  type PublicModel,
} from "../../lib/api";
import { runPublicModelWrite, type PublicModelWriteGate } from "../../lib/publicModelWriteGate";
import { isUnknownOutcome } from "../../lib/creditOutcome";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { pricingStates, unpricedReason } from "../../lib/publicPricing";
import { deleteRefusalText, deleteWarning } from "../../lib/retireWarning";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { useRecordTable } from "../../lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "../../ui/RecordTable";
import { FormDialog } from "../../ui/FormDialog";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { Card, CardHeader, CardMessage, ChartSkeleton, PageHeader } from "../../components/page";
import { Lamp } from "../../ui/Lamp";
import { PublicModelDetail } from "./PublicModelDetail";
import { CardFooter, Count, FormGrid } from "./kit";
import { Select } from "../../ui/Select";

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
export function PublicModels({ canEdit, onGoto, initialSection, sectionVisit, renderRouting, writeGate }: TabProps & {
  writeGate: PublicModelWriteGate;
  renderRouting?: (model: PublicModel, onChanged: () => Promise<boolean>, onClose: () => void, onApplied: (model: PublicModel) => void) => ReactNode;
}) {
  const toast = useToast();
  const [rows, setRows] = useState<PublicModel[] | null>(null);
  const [prices, setPrices] = useState<Map<string, ModelGroupPriceRow> | null>(null);
  const [liveGroups, setLiveGroups] = useState<Group[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [refusal, setRefusal] = useState<string | null>(null);
  /** The refusal belongs on the name field: a live model already holds the name. */
  const [atName, setAtName] = useState(false);
  const [selected, setSelected] = useState<string | null>(initialSection ?? null);
  // 同页切换详情也从标题开始；刷新数据不打断当前编辑位置。
  useLayoutEffect(() => {
    if (!selected) return;
    window.scrollTo({ top: 0, left: 0, behavior: "instant" });
    document.getElementById("workspace-main")?.focus({ preventScroll: true });
  }, [selected]);
  useEffect(() => { setSelected(initialSection ?? null); }, [initialSection, sectionVisit]);
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  /** The models a delete or restore is on its way for. */
  const [busy, setBusy] = useState<ReadonlySet<string>>(() => new Set());
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  /** The newest read asked for: an answer to an older one is dropped. */
  const latest = useRef(0);

  const load = useCallback(async () => {
    const n = ++latest.current;
    try {
      const [models, priced, groupList] = await Promise.all([publicModels.list(), groupPrices.list(), groupsApi.list()]);
      if (n !== latest.current) return false;
      setRows(models);
      setPrices(new Map(priced.map((p) => [p.model_id, p] as const)));
      setLiveGroups(groupList);
      setError(null);
      return true;
    } catch (e) {
      if (n !== latest.current) return false;
      setError(errorText(e));
      return false;
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

  const [modelState, setModelState] = useState("");
  const records = useRecordTable({
    items: (rows ?? []).filter((model) => modelState === "" || (modelState === "live" ? model.retired_at === null : modelState === "retired" ? model.retired_at !== null : model.retired_at === null && !model.served_by.some((account) => account.enabled))),
    searchText: (model) => [model.name, model.id],
    resetKey: modelState,
  });
  const pricing = useMemo(() => {
    if (rows === null || prices === null) return null;
    const states = pricingStates(rows, prices, liveGroups);
    const counts = { priced: 0, no_baseline: 0, missing_rates: 0 };
    for (const model of rows) {
      if (model.retired_at === null) counts[states.get(model.id)!.kind] += 1;
    }
    return { states, counts };
  }, [rows, prices, liveGroups]);
  function select(model: string | null): void {
    setCreateOpen(false);
    setSelected(model);
    onGoto("public-routes", model ?? undefined);
  }
  const open = rows?.find((m) => m.id === selected) ?? null;
  if (open) {
    return (
      <PublicModelDetail
        model={open}
        writeGate={writeGate}
        canEdit={canEdit}
        onBack={() => select(null)}
        onChanged={async () => { await load(); }}
        loadError={error}
        routingEditor={renderRouting?.(open, load, () => select(null), (model) => {
          // 写入响应先于重读应用，旧的并发读取不能回退刚保存的目标与设置。
          latest.current++;
          setRows((previous) => previous?.map((row) => row.id === model.id ? model : row) ?? previous);
        })}
      />
    );
  }

  const nameRefusal = atName ? refusal : null;
  // 统计来自完整公共模型列表；定价只在用模型，沿用逐行的服务分组判断。
  const live = rows?.filter((model) => model.retired_at === null) ?? null;
  const unavailable = live?.filter((model) => !model.served_by.some((account) => account.enabled)) ?? null;
  const pricingCounts = pricing?.counts ?? null;

  async function create(e: React.FormEvent) {
    e.preventDefault();
    if (!canEdit) return;
    await runPublicModelWrite(writeGate, async () => {
      setCreating(true);
      setRefusal(null);
      setAtName(false);
      try {
        const model = await publicModels.create(name);
        setName("");
        toast(`已新建公共模型「${model.name}」。请在路由中添加服务账号。`);
        select(model.id);
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
    });
  }

  async function remove(model: PublicModel) {
    if (!canEdit) return;
    await runPublicModelWrite(writeGate, async () => {
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
    });
  }

  async function restore(model: PublicModel) {
    if (!canEdit) return;
    await runPublicModelWrite(writeGate, async () => {
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
    });
  }


  return (
    <div className="public-models contents">
      <PageHeader
        eyebrow="模型管理 / 公共模型与路由"
        title="公共模型与路由"
        dataLead={live && unavailable ? <>当前有 {live.length.toLocaleString("zh-CN")} 个在用模型，其中 {unavailable.length.toLocaleString("zh-CN")} 个没有可用账号。先补齐账号映射，再核对基准价与服务分组换算率。</> : "读取完整模型列表后展示服务与定价状态。"}
        actions={
          <><Button className="finance-icon-button" aria-label="刷新" title="刷新" type="button" icon="refresh" onClick={() => void load()}></Button>{canEdit && <Button kind="primary" icon="plus" aria-label="新建公共模型" disabled={writeGate.busy} onClick={() => setCreateOpen(true)}>新建公共模型</Button>}</>
        }
      />
      <p className="mb-4 max-w-3xl text-caption text-fg-3">租户请求的模型名。标识只生成一次，授权、基准价和分组换算率都跟着它，所以名称不改、也不接管已有模型；显示名重复时请直接使用已有模型。</p>
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
      <section className="public-model-overview" aria-label="公共模型状态概览">
        <dl className="compact-overview-facts">
          <div><dt>在用公共模型</dt><dd>{live ? live.length.toLocaleString("zh-CN") : "—"}</dd></div>
          <div data-tone="success"><dt>已定价</dt><dd>{pricingCounts?.priced ?? "—"}</dd></div>
          <div data-tone="warning"><dt>缺少基准价</dt><dd>{pricingCounts?.no_baseline ?? "—"}</dd></div>
          <div data-tone="warning"><dt>缺少分组换算率</dt><dd>{pricingCounts?.missing_rates ?? "—"}</dd></div>
          <div data-tone="warning"><dt>没有可用账号</dt><dd>{unavailable ? unavailable.length.toLocaleString("zh-CN") : "—"}</dd></div>
        </dl>
        <p className="text-caption text-fg-3">{rows && live ? `全部 ${rows.length.toLocaleString("zh-CN")} 个 · 已停用 ${(rows.length - live.length).toLocaleString("zh-CN")} 个。` : "统计来自完整公共模型列表。"}无可用账号时请求会以 503 失败，进入模型详情可核对路由与账号映射。</p>
      </section>
      <FormDialog open={createOpen} onOpenChange={setCreateOpen} title="新建公共模型" description="设置租户调用的模型名称。创建后，在路由中添加服务账号。" busy={creating || writeGate.busy}>
              <form ref={form} aria-label="新建公共模型" onSubmit={(e) => void create(e)}>
                <FormGrid>
                  <Field label="名称" error={nameRefusal}>
                    {(aria) => (
                      <input
                        required
                        value={name}
                        disabled={creating || writeGate.busy}
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
                  <Button kind="primary" type="submit" disabled={writeGate.busy} busy={creating} busyLabel="新建中…">新建</Button>
                </CardFooter>
              </form>
      </FormDialog>
        <Card className="pm-list-section mb-0">
          <CardHeader number={3} title={<>全部公共模型{rows && <Count n={rows.length} />}</>} description="名称与稳定标识保留；授权租户和密钥统计包含全部模型授权。" />
          {rows === null ? (
            error ? <CardMessage onRetry={() => void load()}>读取失败，请重试</CardMessage> : <ChartSkeleton />
          ) : rows.length === 0 ? (
            <EmptyState text="还没有公共模型。" />
          ) : (
            <>
            <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索公共模型", placeholder: "模型名称或 ID" }}>
              <label className="f filter"><span>模型状态</span><Select size="compact" value={modelState} onValueChange={(value) => setModelState(value)}><option value="">全部</option><option value="live">在用</option><option value="retired">已停用</option><option value="unavailable">在用但无可用账号</option></Select></label>
            </RecordTableToolbar>
            <DataTable label="公共模型" tableClassName="pm-models-table">
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
                {records.rows.length === 0 && <tr><td colSpan={7}><EmptyState text="没有符合筛选条件的公共模型" /></td></tr>}
                {records.rows.map((m) => {
                  const priced = pricing?.states.get(m.id) ?? null;
                  const retired = m.retired_at !== null;
                  return (
                    <tr data-finance-record key={m.id} data-model={m.id}>
                      <td data-label="名称">
                        <button type="button" className="link-button pm-model-link" aria-label={m.name} onClick={() => select(m.id)}>
                          <span>{m.name}</span><span className="pm-model-id mono">{m.id}</span>
                        </button>
                      </td>
                      <td data-label="服务账号" className={m.served_by.some((s) => s.enabled) ? undefined : "text-fg-3"}>{tiersOf(m)}</td>
                      <td data-label="定价">
                        {retired ? (
                          "—"
                        ) : priced === null ? (
                          "…"
                        ) : priced.kind === "priced" ? (
                          <span className="pm-pricing-state" data-state="priced">已定价</span>
                        ) : (
                          <>
                            <span className="pm-pricing-state" data-state="unpriced">未定价</span>
                            <div className="cell-note">{unpricedReason(priced)}</div>
                          </>
                        )}
                      </td>
                      <td data-label="授权租户" className="num">{m.reachable_tenants}</td>
                      <td data-label="密钥" className="num">{m.reachable_keys}</td>
                      <td data-label="状态">
                        <Lamp state={retired ? "disabled" : "applied"}>{retired ? "已停用" : "在用"}</Lamp>
                      </td>
                      <td data-label="操作" className="right">
                        {canEdit && retired && (
                          <TwoStep
                            label="恢复"
                            kind="quiet"
                            icon={<Undo2 aria-hidden />}
                            size="sm"
                            disabled={writeGate.busy}
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
                            kind="quiet"
                            icon={<Trash2 aria-hidden className="text-danger" />}
                            disabled={writeGate.busy}
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
            <RecordTablePager page={records.page} pageSize={records.pageSize} onPageChange={records.setPage} onPageSizeChange={records.setPageSize} total={records.filteredCount} label="公共模型分页" />
            </>
          )}
        </Card>
    </div>
  );
}
