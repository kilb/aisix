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
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { TwoStep } from "../../ui/TwoStep";
import { useToast } from "../../ui/Toast";
import { Card, CardHeader, PageHeader, Split } from "../../components/page";
import { Lamp } from "../../ui/Lamp";
import { PublicModelDetail } from "./PublicModelDetail";
import { CardFooter, Count } from "./kit";

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
    // The page's own box draws nothing (`contents`): its cards do. It holds
    // the heading with the list and the form, as the specs read it.
    <div className="panel contents">
      <PageHeader
        title="公共模型"
        description="租户请求的模型名。标识只生成一次，授权、基准价和分组换算率都跟着它，所以名称不改、也不接管已有模型；显示名重复时请直接使用已有模型。"
        actions={
          <Button type="button" icon="refresh" onClick={() => void load()}>
            刷新
          </Button>
        }
      />
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
      <Split
        asideWidth="360px"
        main={
          <Card className="mb-0">
            <CardHeader className="mb-0" title={<>全部公共模型{rows && <Count n={rows.length} />}</>} />
            {rows === null ? (
              !error && <Loading />
            ) : rows.length === 0 ? (
              <EmptyState text="还没有公共模型。" />
            ) : (
              <DataTable label="公共模型">
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
                        </td>
                        <td className={m.served_by.some((s) => s.enabled) ? undefined : "text-muted-foreground"}>{tiersOf(m)}</td>
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
        }
        aside={
          canEdit ? (
            <Card className="mb-0">
              <CardHeader title="新建公共模型" description="新建后，请在任一上游账号的「模型」里勾选它。" />
              <form ref={form} aria-label="新建公共模型" onSubmit={(e) => void create(e)}>
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
                <CardFooter className="mt-2">
                  <Button kind="primary" type="submit" busy={creating} busyLabel="新建中…">
                    新建
                  </Button>
                </CardFooter>
              </form>
            </Card>
          ) : undefined
        }
      />
    </div>
  );
}
