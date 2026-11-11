import { Fragment, useEffect, useId, useRef, useState, type ReactNode } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { Field, useRefusalFocus } from "../ui/Field";
import { Msg, type Note } from "../components/Msg";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  ApiError,
  channelModels,
  publicModels,
  tenants,
  type PublicModel,
  type ResourceView,
  type ServedBy,
  type Tenant,
} from "../lib/api";
import { messageFor, problemAt } from "../lib/errors";
import { errorText } from "../lib/fmt";
import { modelClass, modelSource, type ModelClass, type ModelSource } from "../lib/modelSource";
import { draftOf as routingDraftOf, routingBody, sameRouting, statusesOf, type RoutingDraft } from "../lib/publicRouting";
import {
  NEW_TARGET,
  addedBody,
  changedBody,
  draftOf as targetDraftOf,
  targetsOf,
  unsendable,
  withSaved,
  type TargetDraft,
  type TargetField,
} from "../lib/publicTargets";
import { inUseText, retireWarning } from "../lib/retireWarning";
import { defaultAt, enumAt, loadSchema } from "../lib/schema";
import { useResource } from "../lib/useResource";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { TwoStep } from "../ui/TwoStep";
import { UpstreamModelPicker } from "../ui/UpstreamModelPicker";
// The upstream-name field's own styles live with the account page that introduced it.

/** One routing target; fields this page does not edit (such as `tags`) are kept as stored. */
interface Target {
  model_id?: string | null;
  /** The display-name form of the reference, read only without `model_id`. */
  model?: string;
  weight?: number;
  priority?: number;
  [field: string]: unknown;
}

/** The fields of a `model` this page edits; every other field is kept as stored. */
interface ModelDoc {
  display_name?: string;
  provider?: string;
  model_name?: string;
  provider_key_id?: string;
  routing?: { strategy?: string; targets?: Target[]; [field: string]: unknown } | null;
  ensemble?: unknown;
  semantic?: unknown;
  [field: string]: unknown;
}

/** What this page reads of an upstream account. */
interface AccountDoc {
  display_name?: string;
  provider?: string;
  groups?: string[];
  [field: string]: unknown;
}

type Model = ResourceView<ModelDoc>;
type Account = ResourceView<AccountDoc>;

/** The model's dispatch shape. Only direct and routing models have a form here. */
type Shape = "direct" | "routing" | "ensemble" | "semantic";


const SHAPE_LABEL: Record<Shape, string> = {
  direct: "直连",
  routing: "路由",
  ensemble: "集成",
  semantic: "语义路由",
};

/** What the routing form reads from the gateway's `model` schema. */
type SchemaInfo = { strategies: string[]; preset: string | null } | { error: string };

/**
 * The strategy a public model's generated routing document dispatches on when
 * the operator set none (`channels::routing_doc` in the control plane). It is
 * not the schema's own default (`failover`), because `failover` would make a
 * target's `weight` inert, so the form shows this one for "not set".
 */
const GENERATED_STRATEGY = "round_robin";

/**
 * The strategy a public model cannot route by: it orders targets by price,
 * and the targets its accounts generate carry none (`channels::checked_routing`
 * refuses it).
 */
const UNPRICED_STRATEGY = "least_cost";

/** Which form is open: a new model of one shape, or one model. */
type Open = { mode: "new"; shape: "direct" | "routing" } | { mode: "edit"; id: string };

/** How a referrer's kind is named on this page. */
const KIND_LABEL: Record<string, string> = {
  models: "模型",
  cache_policies: "缓存策略",
  guardrail_attachments: "护栏挂载",
  rate_limit_policies: "限流策略",
};

const RENAME_HINT = "改名不会影响已授权的租户和引用它的路由、缓存策略。";
/** References written by name are not rewritten on a rename. */
const RENAME_BY_NAME = "按名称写的配置不会跟着改名：平台自有密钥的模型名单、按模型名匹配的限流条件需要同步修改。";

function shapeOf(doc: ModelDoc): Shape {
  if (doc.routing != null) return "routing";
  if (doc.ensemble != null) return "ensemble";
  if (doc.semantic != null) return "semantic";
  return "direct";
}

function nameOf(m: Model): string {
  return m.doc.display_name || m.id;
}

/** The models and accounts a row or form names, by id. */
interface Catalog {
  models: Map<string, Model>;
  accounts: Map<string, Account>;
}

/** The model a target names: by id, or by display name when it carries no id. */
function targetModel(t: Target, catalog: Catalog): Model | undefined {
  if (t.model_id) return catalog.models.get(t.model_id);
  return [...catalog.models.values()].find((m) => m.doc.display_name === t.model);
}

/** The groups of the account a direct model dispatches through, or why there are none to show. */
function groupsOf(m: Model | undefined, catalog: Catalog): string {
  if (!m || shapeOf(m.doc) !== "direct") return "—";
  const account = m.doc.provider_key_id ? catalog.accounts.get(m.doc.provider_key_id) : undefined;
  if (!account) return "—（上游账号不存在）";
  return account.doc.groups?.length ? account.doc.groups.join("、") : "—（账号未设分组）";
}

/**
 * The platform's model catalog (`models`): direct models on an upstream
 * account and routing models over them, each with the provider groups that
 * can reach it, its rollout, a form for both shapes and a two-step delete.
 * Ensemble and semantic models are listed and edited on 全部资源.
 *
 * Each row also says where it comes from. A model the operator wrote here is
 * edited and deleted here. One a public model generated is not: it is owned
 * by that public model, so a `DELETE` on it is refused as `managed_resource`.
 *
 * - A generated **routing** document is the public model's own name, and it
 *   is where that name's routing settings live, so its 「编辑」 opens a form
 *   for them (`PublicRoutingForm`) — saved through the public model's own
 *   `PATCH`, which regenerates the document — and for its targets, each
 *   edited, added or removed as that account's mapping. Everything the name
 *   needs can be done on this page.
 * - A generated **direct** document is one account's mapping, edited as a
 *   target of that routing form, so it stays read-only here: 查看 shows it.
 *
 * Removing the name itself is 停用, which retires the public model: the
 * routing document, every account's direct document and the account
 * mappings go, or the retire is refused with what still names the model.
 */
export function Models({ canEdit }: TabProps) {
  const models = useResource<ModelDoc>("models");
  const accounts = useResource<AccountDoc>("provider_keys");
  const [publics, setPublics] = useState<PublicModel[] | null>(null);
  const [publicsError, setPublicsError] = useState<string | null>(null);
  const [schema, setSchema] = useState<SchemaInfo | null>(null);
  const [open, setOpen] = useState<Open | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  // Whether 内部直连 rows are listed. Off by default: they are the targets a
  // public model's routing document dispatches to, one per serving account,
  // and they are what made the flat list unreadable (27 public models served
  // by 66 accounts is 93 rows, 66 of them plumbing). The operator's own
  // models and every 对外 row stay listed either way.
  const [showInternal, setShowInternal] = useState(false);
  // The row whose `删除` was clicked once, with the tenants granted it (null while read).
  const [deleting, setDeleting] = useState<{ id: string; holders: Tenant[] | string | null } | null>(null);
  // The public model id whose `停用` is in flight.
  const [retiring, setRetiring] = useState<string | null>(null);
  // The direct model row whose `移除该目标` is in flight.
  const [removingTarget, setRemovingTarget] = useState<string | null>(null);
  // A refused write: a manual delete, a retire, or a save.
  const [deleteFailure, setDeleteFailure] = useState<{ name: string; error: ApiError } | null>(null);
  const [busy, setBusy] = useState(false);
  // Numbers each holder lookup; a result is applied only while its number is the latest.
  const lookup = useRef(0);

  useEffect(() => {
    let live = true;
    const at = "/definitions/Routing/properties/strategy";
    loadSchema("model").then(
      (s) => {
        let info: SchemaInfo;
        try {
          const preset = defaultAt(s, at);
          info = { strategies: enumAt(s, at), preset: typeof preset === "string" ? preset : null };
        } catch (e) {
          info = { error: e instanceof Error ? e.message : String(e) };
        }
        if (live) setSchema(info);
      },
      (e) => live && setSchema({ error: errorText(e) }),
    );
    return () => {
      live = false;
    };
  }, []);

  const catalog: Catalog = {
    models: new Map((models.items ?? []).map((m) => [m.id, m])),
    accounts: new Map((accounts.items ?? []).map((a) => [a.id, a])),
  };
  // What each generated row came from, by public model id; null until the read.
  const publicById = publics === null ? null : new Map(publics.map((p) => [p.id, p] as const));

  // Numbers each read of the public models; an answer is applied only while
  // its number is the latest, so a read that started before a save can never
  // land after the save's own answer and put the old settings back.
  const publicsRead = useRef(0);

  /**
   * Reads the public models again. A failed read leaves the last good list
   * and says so: an empty list would read as "every public model is gone",
   * which is a different — and false — statement.
   */
  async function loadPublics() {
    const ticket = ++publicsRead.current;
    try {
      const list = await publicModels.list();
      if (ticket !== publicsRead.current) return;
      setPublics(list);
      setPublicsError(null);
    } catch (e) {
      if (ticket === publicsRead.current) setPublicsError(errorText(e));
    }
  }

  /**
   * A write's own answer for one public model, applied at once: it is newer
   * than any read still in flight, which is therefore dropped. The routing
   * form is keyed by its document's version, so the reload a save starts
   * remounts it, and it must remount from these settings, not from a list
   * read before the save.
   */
  function applyPublic(view: PublicModel) {
    publicsRead.current++;
    setPublics((list) => list?.map((p) => (p.id === view.id ? view : p)) ?? list);
  }

  // The public models, once per page: they are read to name the 来源 column
  // and to carry a retire's own counts. Only a failed read retries on 刷新.
  useEffect(() => {
    void loadPublics();
  }, []);

  // Every read this page needs, together: the row 刷新 and the retire both do.
  function refresh() {
    void models.reload();
    void accounts.reload();
    void loadPublics();
  }

  // Opening a form clears the last result, a refused delete included.
  function openForm(next: Open) {
    setNote(null);
    setDeleteFailure(null);
    setOpen(next);
  }

  function saved(view: Model) {
    setOpen(null);
    setNote({ kind: "ok", text: `已保存「${nameOf(view)}」。` });
    // The row's badge follows the rollout; a failed read leaves the last view.
    models.watch(view.id).catch(() => undefined);
  }

  // The first step of a delete: name the tenants the model will be withdrawn from.
  async function startDelete(m: Model) {
    setNote(null);
    setDeleteFailure(null);
    const ticket = ++lookup.current;
    setDeleting({ id: m.id, holders: null });
    let holders: Tenant[] | string;
    try {
      holders = (await tenants.list()).filter((t) => t.allowed_model_ids.includes(m.id));
    } catch (e) {
      holders = errorText(e);
    }
    if (lookup.current === ticket) setDeleting((d) => (d?.id === m.id ? { ...d, holders } : d));
  }

  function cancelDelete() {
    lookup.current++;
    setDeleting(null);
  }

  async function remove(m: Model) {
    setBusy(true);
    try {
      await models.remove(m.id, m.version);
      setNote({ kind: "ok", text: `已删除「${nameOf(m)}」。` });
      if (open?.mode === "edit" && open.id === m.id) setOpen(null);
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(String(e), 0, "client", [], [], null);
      setDeleteFailure({ name: nameOf(m), error });
    } finally {
      setBusy(false);
      cancelDelete();
    }
  }

  /**
   * Retires the public model a generated row came from. Its answer is the
   * whole outcome: on success the routing document, every account's direct
   * document and the account mappings are gone; refused as
   * `public_model_in_use`, the counts say what still names it.
   */
  async function retire(id: string) {
    const named = publicById?.get(id)?.name;
    setNote(null);
    setDeleteFailure(null);
    setRetiring(id);
    try {
      await publicModels.retire(id);
      setNote({ kind: "ok", text: `已停用公共模型「${named ?? id}」，它生成的模型已删除。` });
      refresh();
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(String(e), 0, "client", [], [], null);
      setDeleteFailure({ name: named ?? id, error });
    } finally {
      setRetiring(null);
    }
  }

  /**
   * Takes one account out of a public model's routing: the account's mapping
   * goes, which removes its direct document and rewrites the routing document
   * without that target. The public model and every other account are left as
   * they are — retiring the name is the routing row's own action.
   */
  async function removeTarget(m: Model) {
    const source = modelSource(m);
    const account = m.doc.provider_key_id;
    if (source.kind !== "public" || !source.generated || !account) return;
    setNote(null);
    setDeleteFailure(null);
    setRemovingTarget(m.id);
    try {
      await channelModels.remove(account, source.id);
      setNote({
        kind: "ok",
        text: `已移除「${catalog.accounts.get(account)?.doc.display_name ?? account}」对「${
          publicById?.get(source.id)?.name ?? source.id
        }」的服务，该目标已不在路由里。`,
      });
      refresh();
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(String(e), 0, "client", [], [], null);
      setDeleteFailure({ name: nameOf(m), error });
    } finally {
      setRemovingTarget(null);
    }
  }

  // The rows listed, and how many each class holds: the count beside the
  // toggle is what says the page is hiding rows rather than the operator
  // seeing a short list for no stated reason.
  const all = models.items ?? [];
  const counts: Record<ModelClass, number> = { facing: 0, internal: 0, manual: 0 };
  for (const m of all) counts[modelClass(m)] += 1;
  const shown = showInternal ? all : all.filter((m) => modelClass(m) !== "internal");

  const editing = open?.mode === "edit" ? models.items?.find((m) => m.id === open.id) : undefined;
  // A model the operator wrote has the catalog form. A generated routing
  // document is its public model's name, whose settings and targets this page
  // owns (`PublicRoutingForm`); a generated direct document is one account's
  // mapping, changed as a target of that form, so it opens read-only in the
  // same catalog form.
  const editingSource = editing ? modelSource(editing) : null;
  const editingPublic =
    editing && editingSource?.kind === "public" && !editingSource.generated
      ? publicById?.get(editingSource.id)
      : undefined;
  const editable =
    editing !== undefined &&
    (editingSource?.kind === "manual" ||
      editingPublic !== undefined ||
      // Opened read-only below: the catalog form with every field locked.
      (editingSource?.kind === "public" && editingSource.generated));
  const formProps = {
    catalog,
    schema,
    create: models.create,
    save: models.save,
    onSaved: saved,
    onClose: () => setOpen(null),
  };

  return (
    <>
      {open?.mode === "new" && (
        <ModelForm key={`new-${open.shape}`} model={null} shape={open.shape} readOnly={false} onReload={() => undefined} {...formProps} />
      )}
      {editing && editingPublic && (
        // Keyed by the public model alone: the reads a save or a removal
        // starts must not throw away what is still typed in the form, which
        // shows every untouched field as last read.
        <PublicRoutingForm
          key={editingPublic.id}
          publicModel={editingPublic}
          catalog={catalog}
          schema={schema}
          readOnly={!canEdit}
          onClose={() => setOpen(null)}
          onSaved={(view, complete) => {
            // A save that left something unsaved says so in the form, at each refusal.
            setNote(complete && view ? { kind: "ok", text: `已保存「${view.name}」的路由设置。` } : null);
            if (view === null) return;
            // The model as the save left it first, so the form shows the
            // regenerated targets whichever read below lands first.
            applyPublic(view);
            void models.reload();
            void loadPublics();
          }}
          onTargetRemoved={(where, view) => {
            setNote({
              kind: "ok",
              text: `已移除账号「${where}」对「${editingPublic.name}」的映射，该目标已不在路由里。`,
            });
            if (view) applyPublic(view);
            void models.reload();
            void loadPublics();
          }}
        />
      )}
      {editing && editable && !editingPublic && (shapeOf(editing.doc) === "direct" || shapeOf(editing.doc) === "routing") && (
        // Keyed by version: a reload that finds a newer version starts the edit over.
        <ModelForm
          key={`${editing.id}:${editing.version}`}
          model={editing}
          shape={shapeOf(editing.doc) as "direct" | "routing"}
          readOnly={!canEdit || modelSource(editing).kind === "public"}
          onReload={() => void models.reload()}
          {...formProps}
        />
      )}

      <div className="panel">
        <h2>
          模型与路由
          <Button
            size="sm"
            type="button"
            icon="refresh"
            onClick={refresh}
          >
            刷新
          </Button>
        </h2>
        <p className="hint">
          租户按对外名称调用模型。直连模型经一个上游账号调用上游模型；路由模型按选路策略在若干直连模型之间选择。每个模型可服务的分组来自它的上游账号：普通租户只能用到其分组内的账号。
        </p>
        <p className="hint">
          要让租户看到和购买的模型，请在「上游账号 → 模型」中建为公共模型，它生成的直连模型只在内部使用；在这里新建的模型会出现在分组定价和租户授权中。
        </p>
        {canEdit && !open && (
          <p>
            <Button kind="primary" onClick={() => openForm({ mode: "new", shape: "direct" })}>
              新增直连模型
            </Button>{" "}
            <Button kind="primary" onClick={() => openForm({ mode: "new", shape: "routing" })}>
              新增路由模型
            </Button>
          </p>
        )}
        <Msg note={note} />
        <div className="r">
          <label className="f filter">
            <input
              type="checkbox"
              checked={showInternal}
              onChange={(e) => setShowInternal(e.target.checked)}
            />{" "}
            <span>显示内部直连模型（{counts.internal}）</span>
          </label>
        </div>
        {deleteFailure &&
          (deleteFailure.error.code === "version_mismatch" ? (
            <ConflictNote
              onReload={() => {
                setDeleteFailure(null);
                void models.reload();
              }}
            />
          ) : (
            <div className="note crit" role="alert">
              {deleteFailure.error.code === "public_model_in_use"
                ? inUseText(deleteFailure.error)
                : `无法删除「${deleteFailure.name}」：${messageFor(deleteFailure.error)}`}
              {deleteFailure.error.referencedBy.length > 0 && (
                <>
                  <p>被以下资源引用：</p>
                  <ul>
                    {deleteFailure.error.referencedBy.map((r) => (
                      <li key={`${r.kind}/${r.id}`}>
                        {KIND_LABEL[r.kind] ?? r.kind}「{r.name ?? r.id}」
                      </li>
                    ))}
                  </ul>
                </>
              )}
            </div>
          ))}
        {models.error && (
          <div className="note crit" role="alert">
            读取失败：{messageFor(models.error)}
          </div>
        )}
        {accounts.error && (
          <div className="note crit" role="alert">
            上游账号读取失败，账号名与分组无法显示：{messageFor(accounts.error)}
          </div>
        )}
        {publicsError && (
          <div className="note crit" role="alert">
            公共模型读取失败，来源一列的名称无法显示：{publicsError}
          </div>
        )}
        {models.items === null ? (
          !models.error && <Loading />
        ) : models.items.length === 0 ? (
          <EmptyState text="还没有模型。" />
        ) : shown.length === 0 ? (
          <EmptyState text="没有模型。" />
        ) : (
          <DataTable>
            <thead>
              <tr>
                <th>对外名称</th>
                <th>分类</th>
                <th>类型</th>
                <th>上游账号 / 路由目标</th>
                <th>状态</th>
                <th className="right">操作</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((m) => {
                const shape = shapeOf(m.doc);
                const hasForm = shape === "direct" || shape === "routing";
                const src = modelSource(m);
                const from = src.kind === "public" ? publicById?.get(src.id) : undefined;
                return (
                  <Fragment key={m.id}>
                    <tr>
                      <td>{nameOf(m)}</td>
                      <td>
                        <Source model={m} source={src} from={from} publicRead={publicById !== null} catalog={catalog} />
                      </td>
                      <td>{SHAPE_LABEL[shape]}</td>
                      <td>
                        <Upstream model={m} catalog={catalog} />
                      </td>
                      <td>
                        <RolloutBadge rollout={m.rollout} />
                      </td>
                      <td className="right">
                        {src.kind === "public" ? (
                          <>
                            {src.generated ? (
                              // One account's mapping: it is edited as a target
                              // of the routing row's form, so this is a view.
                              <div className="sub">由公共模型生成，在「{from?.name ?? src.id}」的路由里改</div>
                            ) : (
                              <div className="sub">由公共模型生成，路由设置在这里改</div>
                            )}
                            {/* The routing document is the public name itself:
                                its settings and its targets are this page's. */}
                            <Button
                              size="sm"
                              onClick={() => openForm({ mode: "edit", id: m.id })}
                              disabled={!src.generated && !from}
                            >
                              {canEdit && !src.generated ? "编辑" : "查看"}
                            </Button>
                            {/* A direct row is one account's mapping, so the one
                                removal that fits it takes that account out of the
                                routing model and leaves the name alone. Retiring the
                                public model is the routing row's own action. */}
                            {canEdit && src.generated && m.doc.provider_key_id && (
                              <TwoStep
                                label="移除该目标"
                                size="sm"
                                busy={removingTarget === m.id}
                                busyLabel="移除中…"
                                warning={`移除后「${
                                  catalog.accounts.get(m.doc.provider_key_id)?.doc.display_name ??
                                  m.doc.provider_key_id
                                }」不再服务「${publicById?.get(src.id)?.name ?? src.id}」：本账号的映射被删除，它的直连文档随之移除。公共模型和其他账号不受影响。`}
                                onConfirm={() => void removeTarget(m)}
                              />
                            )}
                            {canEdit && !src.generated && from && (
                              <TwoStep
                                label="停用"
                                size="sm"
                                busy={retiring === src.id}
                                busyLabel="停用中…"
                                // Asked on the row the operator clicked: the warning says what goes,
                                // or that the retire would be refused and why.
                                warning={retireWarning(from)}
                                onConfirm={() => void retire(src.id)}
                              />
                            )}
                          </>
                        ) : deleting?.id === m.id ? (
                          <>
                            <Button
                              kind="danger"
                              size="sm"
                              disabled={busy || deleting.holders === null}
                              busy={busy}
                              busyLabel="确认删除"
                              onClick={() => void remove(m)}
                            >
                              确认删除
                            </Button>
                            <Button size="sm" disabled={busy} onClick={cancelDelete}>
                              取消
                            </Button>
                          </>
                        ) : (
                          <>
                            {hasForm && (
                              <Button size="sm" onClick={() => openForm({ mode: "edit", id: m.id })}>
                                {canEdit ? "编辑" : "查看"}
                              </Button>
                            )}
                            {canEdit && (
                              <Button size="sm" disabled={busy} onClick={() => void startDelete(m)}>
                                删除
                              </Button>
                            )}
                          </>
                        )}
                      </td>
                    </tr>
                    {deleting?.id === m.id && (
                      <tr className="dt-detail">
                        <td colSpan={6}>
                          <DeleteWarning holders={deleting.holders} />
                        </td>
                      </tr>
                    )}
                    {src.kind === "public" && !from && publicById !== null && (
                      <tr className="dt-detail">
                        <td colSpan={6}>
                          <p className="hint">
                            它的公共模型「{src.id}」已不在列表里，请刷新；这次停用不能在这里执行。
                          </p>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </DataTable>
        )}
      </div>
    </>
  );
}

/** A direct model's account and upstream model, or a routing model's targets, each with its groups. */
function Upstream({ model, catalog }: { model: Model; catalog: Catalog }) {
  const doc = model.doc;
  switch (shapeOf(doc)) {
    case "direct": {
      const account = doc.provider_key_id ? catalog.accounts.get(doc.provider_key_id) : undefined;
      return (
        <>
          {account?.doc.display_name || doc.provider_key_id || "—"} · <code>{doc.model_name ?? "—"}</code>
          <div className="sub">可服务分组：{groupsOf(model, catalog)}</div>
        </>
      );
    }
    case "routing":
      return (
        <ul className="targets">
          {(doc.routing?.targets ?? []).map((t, i) => {
            const target = targetModel(t, catalog);
            return (
              <li key={i}>
                {target ? nameOf(target) : `${t.model_id ?? t.model ?? "—"}（模型不存在）`}
                <span className="sub"> · 可服务分组：{groupsOf(target, catalog)}</span>
              </li>
            );
          })}
        </ul>
      );
    default:
      return <span className="sub">在「全部资源」页查看与编辑</span>;
  }
}

/**
 * Where a row comes from: the operator wrote it here, or a public model
 * generated it. A generated direct document also names the account it serves
 * through, since several of one public model's rows differ only in that.
 */
function Source({
  model,
  source,
  from,
  publicRead,
  catalog,
}: {
  model: Model;
  source: ModelSource;
  /** The public model it came from, once the list is read. */
  from: PublicModel | undefined;
  /** Whether that list was read: without it a missing name is not a missing model. */
  publicRead: boolean;
  catalog: Catalog;
}) {
  if (source.kind === "manual") return <>手工创建</>;
  const name = from?.name;
  const account = model.doc.provider_key_id ? catalog.accounts.get(model.doc.provider_key_id) : undefined;
  // The row a caller names is 对外; the per-account documents behind it are
  // 内部直连 — reachable only as that row's targets, never granted or priced.
  const shared = (
    <div className="sub">{publicRead ? (name ?? `${source.id}（公共模型已不在列表）`) : "…"}</div>
  );
  if (!source.generated) return <>对外{shared}</>;
  return (
    <>
      内部直连
      {shared}
      <div className="sub">
        {account ? `账号：${account.doc.display_name || account.id}` : "账号：已不存在"}
      </div>
    </>
  );
}

/** What deleting a model withdraws, shown between `删除` and `确认删除` (ruling 13). */
function DeleteWarning({ holders }: { holders: Tenant[] | string | null }) {
  return (
    <section className="note warn" aria-label="删除模型">
      <p>
        <strong>删除后，该模型会从所有租户的授权里移除。</strong>
        被路由、缓存策略等配置引用的模型不能删除。
      </p>
      {holders === null ? (
        <p className="hint">正在查找已授权该模型的租户…</p>
      ) : typeof holders === "string" ? (
        <p>无法列出已授权该模型的租户：{holders}</p>
      ) : holders.length === 0 ? (
        <p>目前没有租户获授权该模型。</p>
      ) : (
        <>
          <p>已授权该模型的租户：</p>
          <ul>
            {holders.map((t) => (
              <li key={t.id}>{t.name}</li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}

/** One target row of the routing form, as typed. */
interface Row {
  key: number;
  /** The stored target it started from, whose other fields are kept; null for a new row. */
  source: Target | null;
  /** The model id the row started with. */
  initialId: string;
  modelId: string;
  weight: string;
  priority: string;
}

function rowFields(r: Row): string {
  return JSON.stringify([r.key, r.modelId, r.weight, r.priority]);
}

/**
 * One model's form: a new direct or routing model (`model` null), an edit,
 * or a viewer's read-only view. An edit writes back the stored document with
 * only the fields changed here replaced, so a field this form does not show,
 * and a field left as it was, is sent exactly as read.
 */
function ModelForm({
  model,
  shape,
  catalog,
  schema,
  readOnly,
  create,
  save,
  onSaved,
  onClose,
  onReload,
}: {
  model: Model | null;
  shape: "direct" | "routing";
  catalog: Catalog;
  schema: SchemaInfo | null;
  readOnly: boolean;
  create: (doc: ModelDoc) => Promise<Model>;
  save: (id: string, doc: ModelDoc, version: number) => Promise<Model>;
  onSaved: (view: Model) => void;
  onClose: () => void;
  onReload: () => void;
}) {
  const isNew = model === null;
  const doc = model?.doc ?? {};
  const routing = doc.routing ?? {};
  const fromSchema = schema && "strategies" in schema ? schema.strategies : null;
  const strategies =
    fromSchema === null
      ? null
      : fromSchema.includes(GENERATED_STRATEGY)
        ? fromSchema
        : [GENERATED_STRATEGY, ...fromSchema];
  const preset = schema && "preset" in schema ? schema.preset : null;
  // As the form starts, to tell what the operator changed. An absent
  // strategy is "" and shows as the schema's default.
  const [initial] = useState(() => ({
    name: doc.display_name ?? "",
    accountId: doc.provider_key_id ?? "",
    provider: doc.provider ?? "",
    upstream: doc.model_name ?? "",
    strategy: routing.strategy ?? "",
    rows: (routing.targets ?? (isNew ? [{}] : [])).map((t, i): Row => {
      const id = t.model_id ?? targetModel(t, catalog)?.id ?? "";
      return {
        key: i,
        source: isNew ? null : t,
        initialId: id,
        modelId: id,
        weight: t.weight?.toString() ?? "",
        priority: t.priority?.toString() ?? "",
      };
    }),
  }));
  const [name, setName] = useState(initial.name);
  const [accountId, setAccountId] = useState(initial.accountId);
  const [provider, setProvider] = useState(initial.provider);
  const [upstream, setUpstream] = useState(initial.upstream);
  const [strategy, setStrategy] = useState(initial.strategy);
  const [rows, setRows] = useState(initial.rows);
  // The target row whose `移除` was clicked once, awaiting `确认移除`.
  const [removing, setRemoving] = useState<number | null>(null);
  const nextKey = useRef(initial.rows.length);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<ApiError | string | null>(null);
  const [conflict, setConflict] = useState(false);
  // The target rows in the order last sent: a refusal names a target by its index.
  const [sentRows, setSentRows] = useState<number[]>([]);

  const title = isNew
    ? shape === "direct"
      ? "新增直连模型"
      : "新增路由模型"
    : readOnly
      ? `模型「${initial.name}」`
      : `编辑模型「${initial.name}」`;
  const locked = readOnly || busy;
  const own = model?.id ?? null;

  // The account options, plus the stored one when it no longer exists.
  const accountOptions = [...catalog.accounts.values()].map((a) => ({ id: a.id, label: a.doc.display_name || a.id }));
  if (initial.accountId && !catalog.accounts.has(initial.accountId)) {
    accountOptions.push({ id: initial.accountId, label: `${initial.accountId}（上游账号不存在）` });
  }
  const account = catalog.accounts.get(accountId);

  // A target is a direct model: the gateway does not follow a group's targets into further groups.
  const directModels = [...catalog.models.values()].filter((m) => m.id !== own && shapeOf(m.doc) === "direct");
  function targetOptions(row: Row) {
    const options = directModels.map((m) => ({ id: m.id, label: nameOf(m) }));
    if (row.modelId && !options.some((o) => o.id === row.modelId)) {
      const found = catalog.models.get(row.modelId);
      options.push({ id: row.modelId, label: found ? nameOf(found) : `${row.modelId}（模型不存在）` });
    }
    return options;
  }

  // The strategy options: the schema's, plus what is stored when the schema lacks it.
  const shownStrategy = strategy || preset || "";
  const strategyOptions = [
    ...(strategies ?? []),
    ...(initial.strategy && !strategies?.includes(initial.strategy) ? [initial.strategy] : []),
  ];

  function chooseAccount(id: string) {
    setAccountId(id);
    // The provider comes from the account.
    const from = catalog.accounts.get(id)?.doc.provider;
    if (from) setProvider(from);
  }

  function setRow(key: number, change: Partial<Row>) {
    setRows((list) => list.map((r) => (r.key === key ? { ...r, ...change } : r)));
  }

  function addRow() {
    const key = nextKey.current++;
    setRows((list) => [...list, { key, source: null, initialId: "", modelId: "", weight: "", priority: "" }]);
  }

  function targetOf(r: Row): Target {
    const t: Target = r.source ? { ...r.source } : {};
    if (!r.source || r.modelId !== r.initialId) {
      // A new reference replaces both of its forms.
      delete t.model;
      if (r.modelId) t.model_id = r.modelId;
      else delete t.model_id;
    }
    for (const [field, typed] of [
      ["weight", r.weight],
      ["priority", r.priority],
    ] as const) {
      if (typed.trim() === "") delete t[field];
      else t[field] = Number(typed);
    }
    return t;
  }

  function changedDoc(): ModelDoc {
    const next: ModelDoc = isNew ? {} : { ...doc };
    // Compared as typed, so a stored value left alone is never rewritten.
    const put = (field: string, typed: string, before: string) => {
      if (!isNew && typed === before) return;
      const value = typed.trim();
      if (value === "") delete next[field];
      else next[field] = value;
    };
    put("display_name", name, initial.name);
    if (shape === "direct") {
      put("provider_key_id", accountId, initial.accountId);
      put("provider", provider, initial.provider);
      put("model_name", upstream, initial.upstream);
      return next;
    }
    const nextRouting = isNew ? {} : { ...routing };
    if (isNew ? shownStrategy !== "" : strategy !== initial.strategy) nextRouting.strategy = shownStrategy;
    const rowsChanged = rows.map(rowFields).join() !== initial.rows.map(rowFields).join();
    if (isNew || rowsChanged) nextRouting.targets = rows.map(targetOf);
    next.routing = nextRouting;
    return next;
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    setConflict(false);
    setSentRows(rows.map((r) => r.key));
    try {
      const body = changedDoc();
      const view = model ? await save(model.id, body, model.version) : await create(body);
      onSaved(view);
    } catch (e) {
      if (e instanceof ApiError && e.code === "version_mismatch") setConflict(true);
      else setErr(e instanceof ApiError ? e : String(e));
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  // Each refusal detail goes to the field it names; the others stay in the list under the form.
  const details = err instanceof ApiError ? err.details : [];
  const at = (path: string) => problemAt(details, (p) => p === path);
  const targetAt = (r: Row, field: string) => at(`/routing/targets/${sentRows.indexOf(r.key)}/${field}`);
  const strategyShown = !(schema && "error" in schema);
  const shown =
    shape === "direct"
      ? ["/display_name", "/provider_key_id", "/provider", "/model_name"]
      : [
          "/display_name",
          ...(strategyShown ? ["/routing/strategy"] : []),
          ...rows
            .filter((r) => sentRows.includes(r.key))
            .flatMap((r) =>
              ["model_id", "weight", "priority"].map((f) => `/routing/targets/${sentRows.indexOf(r.key)}/${f}`),
            ),
        ];
  const listed = details.filter((d) => !shown.includes(d.path));

  return (
    <form ref={form} className="panel" aria-label={title} onSubmit={submit}>
      <h2>{title}</h2>
      <Field label="对外名称" error={at("/display_name")}>
        {(aria) => <input required disabled={locked} value={name} onChange={(e) => setName(e.target.value)} {...aria} />}
      </Field>
      {!isNew && !readOnly && (
        <p className="hint">
          {RENAME_HINT}
          {RENAME_BY_NAME}
        </p>
      )}

      {shape === "direct" ? (
        <>
          <div className="r">
            <Field label="上游账号" error={at("/provider_key_id")}>
              {(aria) => (
                <select
                  required
                  disabled={locked}
                  value={accountId}
                  onChange={(e) => chooseAccount(e.target.value)}
                  {...aria}
                >
                  <option value="">（选择上游账号）</option>
                  {accountOptions.map((o) => (
                    <option key={o.id} value={o.id}>
                      {o.label}
                    </option>
                  ))}
                </select>
              )}
            </Field>
            <Field label="供应商标识" error={at("/provider")}>
              {(aria) => (
                <input required disabled={locked} value={provider} onChange={(e) => setProvider(e.target.value)} {...aria} />
              )}
            </Field>
          </div>
          {account && (
            <p className="hint">
              该账号可服务的分组：{account.doc.groups?.length ? account.doc.groups.join("、") : "—（账号未设分组）"}
            </p>
          )}
          <Field label="上游模型" error={at("/model_name")}>
            {(aria) => (
              <input required disabled={locked} value={upstream} onChange={(e) => setUpstream(e.target.value)} {...aria} />
            )}
          </Field>
        </>
      ) : (
        <>
          {schema && "error" in schema ? (
            <div className="f">
              <span>选路策略</span>
              <div className="note crit" role="alert">
                {schema.error}
              </div>
            </div>
          ) : (
            <Field label="选路策略" error={at("/routing/strategy")}>
              {(aria) => (
                <select
                  disabled={locked || strategies === null}
                  value={shownStrategy}
                  onChange={(e) => setStrategy(e.target.value)}
                  {...aria}
                >
                  {strategyOptions.map((v) => (
                    <option key={v} value={v}>
                      {v}
                    </option>
                  ))}
                </select>
              )}
            </Field>
          )}
          {rows.map((r, i) => {
            const target = catalog.models.get(r.modelId);
            return (
              <div key={r.key} className="target" role="group" aria-label={`目标 ${i + 1}`}>
                <div className="r">
                  <Field label="目标模型" error={targetAt(r, "model_id")}>
                    {(aria) => (
                      <select
                        required
                        disabled={locked}
                        value={r.modelId}
                        onChange={(e) => setRow(r.key, { modelId: e.target.value })}
                        {...aria}
                      >
                        <option value="">（选择直连模型）</option>
                        {targetOptions(r).map((o) => (
                          <option key={o.id} value={o.id}>
                            {o.label}
                          </option>
                        ))}
                      </select>
                    )}
                  </Field>
                  <Field label="权重" error={targetAt(r, "weight")}>
                    {(aria) => (
                      <input
                        type="number"
                        min={0}
                        step={1}
                        disabled={locked}
                        value={r.weight}
                        onChange={(e) => setRow(r.key, { weight: e.target.value })}
                        {...aria}
                      />
                    )}
                  </Field>
                  <Field label="优先级" error={targetAt(r, "priority")}>
                    {(aria) => (
                      <input
                        type="number"
                        step={1}
                        disabled={locked}
                        value={r.priority}
                        onChange={(e) => setRow(r.key, { priority: e.target.value })}
                        {...aria}
                      />
                    )}
                  </Field>
                </div>
                {r.modelId && <p className="sub">可服务分组：{groupsOf(target, catalog)}</p>}
                {!readOnly && rows.length > 1 && (
                  <p>
                    {removing === r.key ? (
                      <>
                        <Button
                          kind="danger"
                          type="button"
                          disabled={busy}
                          onClick={() => {
                            setRows((list) => list.filter((x) => x.key !== r.key));
                            setRemoving(null);
                          }}
                        >
                          确认移除
                        </Button>{" "}
                        <Button type="button" disabled={busy} onClick={() => setRemoving(null)}>
                          取消
                        </Button>
                      </>
                    ) : (
                      <Button type="button" disabled={busy} onClick={() => setRemoving(r.key)}>
                        移除
                      </Button>
                    )}
                  </p>
                )}
              </div>
            );
          })}
          {!readOnly && (
            <p>
              <Button type="button" disabled={busy} onClick={addRow}>
                添加目标
              </Button>
            </p>
          )}
          <p className="hint">权重、优先级留空时用网关的默认值。优先级高的一组先用，这一组都不可用时才用下一组。</p>
        </>
      )}

      {conflict && <ConflictNote onReload={onReload} />}
      {err && (
        <div className="note crit" role="alert">
          {typeof err === "string" ? err : messageFor(err)}
          {listed.length > 0 && (
            <ul>
              {listed.map((d) => (
                <li key={d.path}>
                  <code>{d.path}</code>：{d.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      {readOnly ? (
        <p>
          <Button type="button" onClick={onClose}>
            关闭
          </Button>
        </p>
      ) : (
        <p>
          <Button kind="primary" type="submit" busy={busy} busyLabel="保存中…">
            保存
          </Button>{" "}
          <Button type="button" disabled={busy} onClick={onClose}>
            取消
          </Button>
        </p>
      )}
    </form>
  );
}

/** Where a target row's refusal goes: one of its fields, or the row itself. */
interface RowRefusal {
  fields: Partial<Record<TargetField | "account", string>>;
  /** What none of the row's fields explains; null when they say it all. */
  message: string | null;
}

/** The field of a target row each refusal path names. */
const TARGET_PATHS: Record<string, TargetField | "account"> = {
  "/upstream_model": "upstream",
  "/priority": "priority",
  "/weight": "weight",
  "/provider_key_id": "account",
};

/** A target write's refusal, at the fields of the row it was for. */
function rowRefusal(e: unknown): RowRefusal {
  if (!(e instanceof ApiError)) return { fields: {}, message: errorText(e) };
  const fields: RowRefusal["fields"] = {};
  const rest: string[] = [];
  for (const d of e.details) {
    const field = TARGET_PATHS[d.path];
    if (field === undefined) rest.push(d.message);
    else fields[field] = fields[field] ? `${fields[field]}；${d.message}` : d.message;
  }
  if (rest.length > 0) return { fields, message: rest.join("；") };
  return { fields, message: Object.keys(fields).length > 0 ? null : messageFor(e) };
}

/** A target the form is adding: an account that does not route this model yet. */
interface Addition {
  key: number;
  /** Empty until the operator picks one. */
  accountId: string;
  draft: TargetDraft;
}

/** The refusal key of an added row; a served row's is its account id. */
const addedRow = (key: number) => `new:${key}`;

/** `record` without `key`. */
function without<T>(record: Record<string, T>, key: string): Record<string, T> {
  const next = { ...record };
  delete next[key];
  return next;
}

/**
 * One public model's routing: its settings, and its targets — one per
 * upstream account that serves it — each with the upstream name the account
 * is asked for, its priority and its weight.
 *
 * The settings are the model's own: saved through
 * `PATCH /admin/public-models/{id}`, which regenerates the routing document
 * in the same transaction, so a later account change cannot revert them. A
 * hand edit of the document could not do this — the document is owned by the
 * public model, and a save on it answers `409 managed_resource`.
 *
 * A target is that account's mapping of the model, so it is edited, added
 * and removed as the mapping (`PUT` / `DELETE
 * /admin/provider-keys/{account}/channel-models/{public model id}`), which
 * regenerates the document from the mappings. The account's own 模型 section
 * edits the same rows.
 *
 * One 保存 sends each changed or added target, in order, then the settings
 * if they changed: separate writes, so each keeps its own outcome. A refused
 * write is shown at the field its refusal names, on its own row; a row that
 * was written takes what the control plane now holds and is not sent again,
 * so saving again after a fix sends only what is left. A field the operator
 * has not touched always shows what was last read.
 */
function PublicRoutingForm({
  publicModel,
  catalog,
  schema,
  readOnly,
  onClose,
  onSaved,
  onTargetRemoved,
}: {
  publicModel: PublicModel;
  catalog: Catalog;
  schema: SchemaInfo | null;
  readOnly: boolean;
  onClose: () => void;
  /**
   * A save ended: `view` is the model as it now stands, or null when nothing
   * was written; `complete` says nothing was refused.
   */
  onSaved: (view: PublicModel | null, complete: boolean) => void;
  /** A target was removed; `view` is the model read back after it, or null when that read failed. */
  onTargetRemoved: (accountName: string, view: PublicModel | null) => void;
}) {
  const stored = publicModel.routing ?? {};
  // The settings as typed, or null while untouched: an untouched form shows
  // what was last read.
  const [settings, setSettings] = useState<RoutingDraft | null>(null);
  // The served targets as typed, by account id; a row not here shows what was last read.
  const [edits, setEdits] = useState<Record<string, TargetDraft>>({});
  const [additions, setAdditions] = useState<Addition[]>([]);
  const nextKey = useRef(1);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [busy, setBusy] = useState(false);
  // The last save's outcome: the settings' refusal, each refused row's, the
  // rows it wrote while something else was refused, and what it says overall.
  const [err, setErr] = useState<ApiError | string | null>(null);
  const [refusals, setRefusals] = useState<Record<string, RowRefusal>>({});
  const [written, setWritten] = useState<string[]>([]);
  const [outcome, setOutcome] = useState<string | null>(null);
  // The account whose removal was clicked once.
  const [removing, setRemoving] = useState<string | null>(null);
  const [removingBusy, setRemovingBusy] = useState(false);
  const title = readOnly ? `模型「${publicModel.name}」` : `编辑模型「${publicModel.name}」`;
  const locked = readOnly || busy;

  const draft = settings ?? routingDraftOf(stored, GENERATED_STRATEGY);
  const statuses = statusesOf(draft.statuses);
  const settingsBody = routingBody(draft, stored, GENERATED_STRATEGY, statuses);
  const settingsChanged = settings !== null && !sameRouting(settingsBody, stored);

  const targets = targetsOf(publicModel);
  const shownOf = (t: ServedBy) => edits[t.provider_key_id] ?? targetDraftOf(t);
  const changedTargets = targets.flatMap((t) => {
    const typed = edits[t.provider_key_id];
    const body = typed === undefined ? null : changedBody(typed, t);
    return body === null ? [] : [{ target: t, typed: typed!, body }];
  });
  const dirty = settingsChanged || changedTargets.length > 0 || additions.length > 0;
  // A disabled mapping is not a target; adding its account enables it again.
  const disabledRows = new Map(
    publicModel.served_by.filter((s) => !s.enabled).map((s) => [s.provider_key_id, s] as const),
  );

  function set<K extends keyof RoutingDraft>(key: K, value: RoutingDraft[K]) {
    setSettings((d) => ({ ...(d ?? routingDraftOf(stored, GENERATED_STRATEGY)), [key]: value }));
  }

  /** Forgets the last save's word on one row: the operator is changing it. */
  function settle(row: string) {
    setRefusals((r) => without(r, row));
    setWritten((w) => w.filter((id) => id !== row));
  }

  function editTarget(t: ServedBy, patch: Partial<TargetDraft>) {
    setEdits((e) => ({ ...e, [t.provider_key_id]: { ...(e[t.provider_key_id] ?? targetDraftOf(t)), ...patch } }));
    settle(t.provider_key_id);
  }

  function editAddition(key: number, patch: Partial<Addition>) {
    setAdditions((list) => list.map((a) => (a.key === key ? { ...a, ...patch } : a)));
    settle(addedRow(key));
  }

  function chooseAccount(a: Addition, accountId: string) {
    // An account whose mapping was disabled starts from what it stored, so
    // enabling it again does not silently rewrite its upstream name.
    const before = disabledRows.get(accountId);
    editAddition(a.key, { accountId, draft: before ? targetDraftOf(before) : a.draft });
  }

  function addTarget() {
    const key = nextKey.current++;
    setAdditions((list) => [...list, { key, accountId: "", draft: NEW_TARGET }]);
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    // A removal in flight reloads the targets when it lands; a save from
    // under it would race it on the same document.
    if (removingBusy || !dirty) return;
    // What cannot be sent at all stops the save before anything is written.
    const unsent: Record<string, RowRefusal> = {};
    for (const { target, typed } of changedTargets) {
      const fields = unsendable(typed);
      if (Object.keys(fields).length > 0) unsent[target.provider_key_id] = { fields, message: null };
    }
    for (const a of additions) {
      const fields: RowRefusal["fields"] = unsendable(a.draft);
      if (a.accountId === "") fields.account = "请选择上游账号";
      if (Object.keys(fields).length > 0) unsent[addedRow(a.key)] = { fields, message: null };
    }
    if (Object.keys(unsent).length > 0) {
      setRefusals(unsent);
      setOutcome(null);
      focusRefusal();
      return;
    }

    // Fixed here: what this save writes does not change under it.
    const batch = [
      ...changedTargets.map(({ target, body }) => ({
        row: target.provider_key_id,
        accountId: target.provider_key_id,
        body,
        added: null as number | null,
      })),
      ...additions.map((a) => ({ row: addedRow(a.key), accountId: a.accountId, body: addedBody(a.draft), added: a.key })),
    ];
    const settingsSent = settingsChanged ? settingsBody : null;
    setBusy(true);
    setErr(null);
    setRefusals({});
    setWritten([]);
    setOutcome(null);

    let view = publicModel;
    const refused: Record<string, RowRefusal> = {};
    const wrote: typeof batch = [];
    for (const w of batch) {
      try {
        const saved = await channelModels.put(w.accountId, publicModel.id, w.body);
        const account = catalog.accounts.get(w.accountId);
        view = withSaved(view, w.accountId, saved, {
          display_name: account?.doc.display_name ?? w.accountId,
          groups: account?.doc.groups ?? [],
        });
        wrote.push(w);
      } catch (e) {
        refused[w.row] = rowRefusal(e);
      }
    }
    let settingsSaved = false;
    let settingsRefused: ApiError | string | null = null;
    if (settingsSent !== null) {
      try {
        // Answered after its own commit, so it is also the read-back of every target written above.
        view = await publicModels.patchRouting(publicModel.id, settingsSent);
        settingsSaved = true;
      } catch (e) {
        settingsRefused = e instanceof ApiError ? e : String(e);
      }
    }
    if (wrote.length > 0 && !settingsSaved) {
      try {
        view = await publicModels.get(publicModel.id);
      } catch {
        // The rows as their writes answered them stand in for the read:
        // what was written is shown as written, and is not sent again.
      }
    }

    const failed = Object.keys(refused).length + (settingsRefused === null ? 0 : 1);
    setEdits((prev) => wrote.reduce((next, w) => (w.added === null ? without(next, w.accountId) : next), prev));
    setAdditions((prev) => prev.filter((a) => !wrote.some((w) => w.added === a.key)));
    if (settingsSaved) setSettings(null);
    setErr(settingsRefused);
    setRefusals(refused);
    setWritten(failed > 0 ? wrote.map((w) => w.accountId) : []);
    setOutcome(
      failed === 0
        ? null
        : wrote.length > 0 || settingsSaved
          ? `有 ${failed} 项没有保存，原因标在各自的位置；其余改动已保存。改正后再点「保存」。`
          : "没有保存：原因标在各自的位置。",
    );
    setBusy(false);
    onSaved(wrote.length > 0 || settingsSaved ? view : null, failed === 0);
    if (failed > 0) focusRefusal();
  }

  async function removeTarget(accountId: string, accountName: string) {
    setRemovingBusy(true);
    try {
      await channelModels.remove(accountId, publicModel.id);
    } catch (e) {
      setRefusals((r) => ({ ...r, [accountId]: rowRefusal(e) }));
      setRemovingBusy(false);
      return;
    }
    let view: PublicModel | null = null;
    try {
      view = await publicModels.get(publicModel.id);
    } catch {
      // The page reads the list again after a removal either way.
    }
    setRemoving(null);
    setEdits((e) => without(e, accountId));
    settle(accountId);
    setRemovingBusy(false);
    onTargetRemoved(accountName, view);
  }

  const details = err instanceof ApiError ? err.details : [];
  const at = (path: string) => problemAt(details, (p) => p === path);
  // The strategy options come from the schema, never from a list here: the
  // one the generator uses for "not set" leads, since an operator who wants it
  // back cannot find it otherwise (it is not the schema's own default).
  //
  // `least_cost` is left out: it ranks targets by their price, a generated
  // target carries none, and the control plane refuses it for a public model.
  // A stored one is still shown, below, so the form reads what is there.
  const fromSchema =
    schema && "strategies" in schema ? schema.strategies.filter((v) => v !== UNPRICED_STRATEGY) : null;
  const strategies =
    fromSchema === null
      ? null
      : fromSchema.includes(GENERATED_STRATEGY)
        ? fromSchema
        : [GENERATED_STRATEGY, ...fromSchema];
  const bad = schema && "error" in schema ? schema.error : null;
  const shown = [
    "/routing/strategy",
    "/routing/retries",
    "/routing/max_fallbacks",
    "/routing/retry_on_429",
    "/routing/fallback_on_statuses",
    "/routing/when_all_unavailable",
  ];
  const listed = details.filter((d) => !shown.includes(d.path));

  // The accounts an added row may pick: not one that routes the model
  // already, nor one another added row has picked.
  const routed = new Set(targets.map((t) => t.provider_key_id));
  function accountOptions(a: Addition) {
    const taken = new Set([
      ...routed,
      ...additions.filter((x) => x.key !== a.key && x.accountId !== "").map((x) => x.accountId),
    ]);
    return [...catalog.accounts.values()]
      .filter((account) => !taken.has(account.id))
      .map((account) => ({
        id: account.id,
        label: `${account.doc.display_name || account.id}（${account.doc.groups?.length ? account.doc.groups.join("、") : "未设分组"}）`,
      }));
  }
  const groupsText = (groups: string[] | undefined) => (groups?.length ? groups.join("、") : "—（账号未设分组）");

  return (
    <form ref={form} className="panel" aria-label={title} onSubmit={submit}>
      <h2>{title}</h2>
      <p className="hint">
        公共模型「{publicModel.name}」的路由：每个目标是一个为它服务的上游账号。选路设置和目标都在这里改，点「保存」一并写入；移除目标立即生效。
      </p>
      {bad && (
        <div className="f">
          <span>选路设置</span>
          <div className="note crit" role="alert">
            {bad}
          </div>
        </div>
      )}
      <div className="r">
        <Field label="选路策略" error={at("/routing/strategy")}>
          {(aria) => (
            <select
              disabled={locked || strategies === null}
              value={draft.strategy}
              onChange={(e) => set("strategy", e.target.value)}
              {...aria}
            >
              {[
                ...(strategies ?? []),
                ...(strategies?.includes(draft.strategy) || draft.strategy === "" ? [] : [draft.strategy]),
              ].map((v) => (
                <option key={v} value={v}>
                  {v}
                </option>
              ))}
            </select>
          )}
        </Field>
        <Field label="全部不可用时的处理" error={at("/routing/when_all_unavailable")}>
          {(aria) => (
            <select
              disabled={locked}
              value={draft.whenAllUnavailable}
              onChange={(e) => set("whenAllUnavailable", e.target.value)}
              {...aria}
            >
              <option value="">（网关默认）</option>
              <option value="fail">fail（返回 503）</option>
              <option value="try_anyway">try_anyway（仍然尝试）</option>
            </select>
          )}
        </Field>
      </div>
      <div className="r">
        <Field label="重试次数" hint="留空用网关的默认值。" error={at("/routing/retries")}>
          {(aria) => (
            <input
              type="number"
              min={0}
              step={1}
              disabled={locked}
              value={draft.retries}
              onChange={(e) => set("retries", e.target.value)}
              {...aria}
            />
          )}
        </Field>
        <Field label="最多切换几个目标" hint="留空表示后面的目标都可以试。" error={at("/routing/max_fallbacks")}>
          {(aria) => (
            <input
              type="number"
              min={0}
              step={1}
              disabled={locked}
              value={draft.maxFallbacks}
              onChange={(e) => set("maxFallbacks", e.target.value)}
              {...aria}
            />
          )}
        </Field>
      </div>
      <Field
        label="额外重试的状态码"
        hint="逗号分隔，例如 408, 429。上游用 4xx 表示临时故障时才有用；5xx 本来就会重试。"
        error={at("/routing/fallback_on_statuses")}
      >
        {(aria) => (
          <input
            disabled={locked}
            value={draft.statuses}
            placeholder="（网关默认：不额外重试）"
            onChange={(e) => set("statuses", e.target.value)}
            {...aria}
          />
        )}
      </Field>
      {draft.statuses.trim() !== "" && statuses === null && (
        <p className="hint">状态码要写成数字，用逗号分隔。</p>
      )}
      <label className="f">
        <input
          type="checkbox"
          disabled={locked}
          checked={draft.retryOn429}
          onChange={(e) => set("retryOn429", e.target.checked)}
        />{" "}
        <span>429 也参与重试和切换</span>
      </label>

      <h2>路由目标（{targets.length}）</h2>
      {targets.length === 0 && additions.length === 0 ? (
        <p className="hint">
          还没有账号服务这个模型：请求会以「上游不可用」失败。{!readOnly && "用下面的「添加目标」加一个账号。"}
        </p>
      ) : (
        <ul className="targets">
          {targets.map((t) => {
            const accountId = t.provider_key_id;
            const accountName = t.display_name || "（账号已不存在）";
            return (
              <TargetRow
                key={accountId}
                label={`目标 ${accountName}`}
                accountId={accountId}
                draft={shownOf(t)}
                publicName={publicModel.name}
                locked={locked}
                refusal={refusals[accountId]}
                written={written.includes(accountId)}
                onChange={(patch) => editTarget(t, patch)}
                accountShown={false}
                heading={
                  <>
                    <div>账号：{accountName}</div>
                    <div className="sub">可服务分组：{groupsText(t.groups)}</div>
                  </>
                }
              >
                {!readOnly &&
                  // Two steps held here rather than by `TwoStep`: a removal
                  // reloads the targets behind this form, and the step has to
                  // survive the re-render that causes.
                  (removing === accountId ? (
                    <div>
                      <p role="alert">
                        移除后，账号「{accountName}」不再为「{publicModel.name}」提供服务，它对该模型的映射也会删除。
                      </p>
                      <p>
                        <Button
                          kind="danger"
                          size="sm"
                          busy={removingBusy}
                          busyLabel="移除中…"
                          disabled={busy}
                          onClick={() => void removeTarget(accountId, accountName)}
                        >
                          确认移除该目标
                        </Button>
                        <Button size="sm" disabled={removingBusy} onClick={() => setRemoving(null)}>
                          取消
                        </Button>
                      </p>
                    </div>
                  ) : (
                    <p>
                      <Button size="sm" disabled={removingBusy || busy} onClick={() => setRemoving(accountId)}>
                        移除该目标
                      </Button>
                    </p>
                  ))}
              </TargetRow>
            );
          })}
          {additions.map((a, i) => {
            const refusal = refusals[addedRow(a.key)];
            const account = catalog.accounts.get(a.accountId);
            return (
              <TargetRow
                key={addedRow(a.key)}
                label={`新目标 ${i + 1}`}
                accountId={a.accountId}
                draft={a.draft}
                publicName={publicModel.name}
                locked={locked}
                refusal={refusal}
                written={false}
                onChange={(patch) => editAddition(a.key, { draft: { ...a.draft, ...patch } })}
                accountShown
                heading={
                  <>
                    <Field label="上游账号" error={refusal?.fields.account}>
                      {(aria) => (
                        <select
                          disabled={locked}
                          value={a.accountId}
                          onChange={(e) => chooseAccount(a, e.target.value)}
                          {...aria}
                        >
                          <option value="">（选择上游账号）</option>
                          {accountOptions(a).map((o) => (
                            <option key={o.id} value={o.id}>
                              {o.label}
                            </option>
                          ))}
                        </select>
                      )}
                    </Field>
                    {account && <div className="sub">可服务分组：{groupsText(account.doc.groups)}</div>}
                    {disabledRows.has(a.accountId) && (
                      <div className="sub">该账号对这个模型的映射已停用，保存后重新启用。</div>
                    )}
                  </>
                }
              >
                <p>
                  <Button
                    size="sm"
                    disabled={busy}
                    onClick={() => {
                      setAdditions((list) => list.filter((x) => x.key !== a.key));
                      settle(addedRow(a.key));
                    }}
                  >
                    取消添加
                  </Button>
                </p>
              </TargetRow>
            );
          })}
        </ul>
      )}
      {!readOnly && (
        <p>
          <Button type="button" disabled={busy} onClick={addTarget}>
            添加目标
          </Button>
        </p>
      )}
      <p className="hint">
        优先级高的一组先用，这一组都不可用时才用下一组；同一组内按权重分配。上游模型名称留空即用公共模型的名称。
      </p>

      {(outcome || err) && (
        <div className="note crit" role="alert">
          {outcome && <p>{outcome}</p>}
          {err && (typeof err === "string" ? err : `路由设置：${messageFor(err)}`)}
          {listed.length > 0 && (
            <ul>
              {listed.map((d) => (
                <li key={d.path}>
                  <code>{d.path}</code>：{d.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}

      <p>
        {!readOnly && (
          <>
            <Button
              kind="primary"
              type="submit"
              busy={busy}
              busyLabel="保存中…"
              disabled={statuses === null || removingBusy || !dirty}
              disabledReason={statuses !== null && !removingBusy && !dirty ? "没有要保存的改动" : undefined}
            >
              保存
            </Button>{" "}
          </>
        )}
        <Button type="button" disabled={busy} onClick={onClose}>
          关闭
        </Button>
      </p>
    </form>
  );
}

/**
 * One target of a public model's routing form: who serves it (`heading`),
 * then the upstream name, priority and weight, each with its own refusal,
 * then whatever the row offers to do with it (`children`).
 */
function TargetRow({
  label,
  accountId,
  heading,
  accountShown,
  draft,
  publicName,
  locked,
  refusal,
  written,
  onChange,
  children,
}: {
  /** The row's accessible name. */
  label: string;
  /** The account whose model list the upstream field offers; empty offers none. */
  accountId: string;
  heading: ReactNode;
  /** Whether `heading` has an account field that shows a refusal of the account itself. */
  accountShown: boolean;
  draft: TargetDraft;
  publicName: string;
  locked: boolean;
  refusal: RowRefusal | undefined;
  /** Written by a save that left something else unsaved. */
  written: boolean;
  onChange: (patch: Partial<TargetDraft>) => void;
  children?: ReactNode;
}) {
  const id = useId();
  const upstreamError = refusal?.fields.upstream ?? null;
  // A row without an account field takes a refusal of its account as its own.
  const message = [refusal?.message, accountShown ? null : refusal?.fields.account]
    .filter((m): m is string => typeof m === "string" && m !== "")
    .join("；");
  return (
    <li className="target" aria-label={label}>
      {heading}
      <div className="r">
        <div className="field">
          <div className="f">
            <span className="f-label">上游模型名称</span>
            <UpstreamModelPicker
              accountId={accountId}
              value={draft.upstream}
              publicName={publicName}
              label="上游模型名称"
              onChange={(upstream) => onChange({ upstream })}
              disabled={locked}
              invalid={upstreamError !== null}
              describedBy={upstreamError === null ? undefined : `${id}-upstream`}
            />
          </div>
          {upstreamError !== null && (
            <p className="field-problem" id={`${id}-upstream`}>
              {upstreamError}
            </p>
          )}
        </div>
        <Field label="优先级" error={refusal?.fields.priority}>
          {(aria) => (
            <input
              type="number"
              step={1}
              disabled={locked}
              value={draft.priority}
              onChange={(e) => onChange({ priority: e.target.value })}
              {...aria}
            />
          )}
        </Field>
        <Field label="权重" error={refusal?.fields.weight}>
          {(aria) => (
            <input
              type="number"
              step={1}
              disabled={locked}
              value={draft.weight}
              onChange={(e) => onChange({ weight: e.target.value })}
              {...aria}
            />
          )}
        </Field>
      </div>
      {message !== "" && (
        <div className="note crit" role="alert">
          {message}
        </div>
      )}
      {written && <p className="sub">已保存</p>}
      {children}
    </li>
  );
}
