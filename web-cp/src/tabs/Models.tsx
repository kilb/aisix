import { FORM_FOOTER, RowField, ServedGroups, GENERATED_STRATEGY, type Target, type ModelDoc, type AccountDoc, type Model, type Catalog, type SchemaInfo } from "./models/catalog";
import { RoutingModeField } from "../ui/RoutingModeField";
import { Plus, RefreshCw, Trash2, Waypoints } from "lucide-react";
import { Fragment, useEffect, useRef, useState, type ReactNode } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { Field, useRefusalFocus } from "../ui/Field";
import { Msg, type Note } from "../components/Msg";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  ApiError,
  publicModels,
  tenants,
  type PublicModel,
  type Tenant,
} from "../lib/api";
import { messageFor, problemAt } from "../lib/errors";
import { errorText } from "../lib/fmt";
import { modelClass, modelSource, type ModelClass, type ModelSource } from "../lib/modelSource";
import { draftOf as routingDraftOf, sameRouting } from "../lib/publicRouting";
import { inUseText } from "../lib/retireWarning";
import { defaultAt, enumAt, loadSchema } from "../lib/schema";
import { useResource } from "../lib/useResource";
import { Card, CardHeader, PageHeader } from "../components/page";
import { cn } from "../lib/cn";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { RoutePolicy } from "../ui/RoutePolicy";
import { CooldownEditor } from "../ui/CooldownEditor";
import { useRecordTable } from "../lib/recordTable";
import { RecordTableToolbar, RecordTablePager } from "../ui/RecordTable";
import { cooldownProblem as validateCooldown } from "../lib/cooldown";
import { GroupBadges } from "./providers/GroupBadges";

import { InventorySummary, countGroups } from "./providers/OperationsBento";

/** The model's dispatch shape. Only direct and routing models have a form here. */
type Shape = "direct" | "routing" | "ensemble" | "semantic";


const SHAPE_LABEL: Record<Shape, string> = {
  direct: "直连",
  routing: "路由",
  ensemble: "集成",
  semantic: "语义路由",
};

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

/** The model a target names: by id, or by display name when it carries no id. */
function targetModel(t: Target, catalog: Catalog): Model | undefined {
  if (t.model_id) return catalog.models.get(t.model_id);
  return [...catalog.models.values()].find((m) => m.doc.display_name === t.model);
}

/** The groups of the account a direct model dispatches through, as badges, or why there are none to show. */
function groupsOf(m: Model | undefined, catalog: Catalog): ReactNode {
  if (!m || shapeOf(m.doc) !== "direct") return "—";
  const account = m.doc.provider_key_id ? catalog.accounts.get(m.doc.provider_key_id) : undefined;
  if (!account) return "—（上游账号不存在）";
  return <GroupBadges groups={account.doc.groups ?? []} empty="—（账号未设分组）" />;
}

/** 模型目录保留清单和手工维护；公共模型的生命周期与映射在公共路由页维护。 */
export function Models({ canEdit, onGoto }: TabProps) {
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
  const [shapeFilter, setShapeFilter] = useState("");
  // The row whose `删除` was clicked once, with the tenants granted it (null while read).
  const [deleting, setDeleting] = useState<{ id: string; holders: Tenant[] | string | null } | null>(null);
  // 手工模型删除或保存的拒绝原因。
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

  // The rows listed, and how many each class holds: the count beside the
  // toggle is what says the page is hiding rows rather than the operator
  // seeing a short list for no stated reason.
  const all = models.items ?? [];
  const counts: Record<ModelClass, number> = { facing: 0, internal: 0, manual: 0 };
  for (const m of all) counts[modelClass(m)] += 1;
  const shown = showInternal ? all : all.filter((m) => modelClass(m) !== "internal");
  const table = useRecordTable({ items: shown.filter((m) => shapeFilter === "" || shapeOf(m.doc) === shapeFilter), resetKey: `${showInternal}:${shapeFilter}`, searchText: (m) => [nameOf(m), m.id, m.doc.model_name ?? "", m.doc.routing?.strategy ?? ""] });

  const editing = open?.mode === "edit" ? models.items?.find((m) => m.id === open.id) : undefined;
  // 公共生成的直连行只读，手工模型保留原编辑表单。
  const editingSource = editing ? modelSource(editing) : null;
  const editable = editing !== undefined && (editingSource?.kind === "manual" || editingSource?.kind === "public");
  const formProps = {
    catalog,
    schema,
    create: models.create,
    save: models.save,
    onSaved: saved,
    onClose: () => setOpen(null),
  };

  // 编辑表单与列表并排，目标行始终保留完整字段。
  let editor: ReactNode = null;
  if (open?.mode === "new") {
    editor = <ModelForm key={`new-${open.shape}`} model={null} shape={open.shape} readOnly={false} onReload={() => undefined} {...formProps} />;
  } else if (editing && editable && (shapeOf(editing.doc) === "direct" || shapeOf(editing.doc) === "routing")) {
    const shape = shapeOf(editing.doc) as "direct" | "routing";
    editor = (
      // Keyed by version: a reload that finds a newer version starts the edit over.
      <ModelForm
        key={`${editing.id}:${editing.version}`}
        model={editing}
        shape={shape}
        readOnly={!canEdit || modelSource(editing).kind === "public"}
        onReload={() => void models.reload()}
        {...formProps}
      />
    );
  }
  // 编辑区打开时仍保留全部列，表格在卡片内部滚动。
  const split = editor !== null;

  return (
    <>
      <PageHeader
        eyebrow="MODELS · 模型目录"
        title="模型目录"
        dataLead={models.items ? <>当前有 {counts.facing} 个对外公共模型、{counts.manual} 个手工模型和 {counts.internal} 个内部直连目标。</> : "查看模型清单，维护手工模型与路由。"}
        actions={
          <>
            <Button type="button" onClick={refresh}>
              <RefreshCw aria-hidden />
              刷新
            </Button>
            {canEdit && !open && (
              <>
                <Button onClick={() => openForm({ mode: "new", shape: "routing" })}>
                  <Waypoints aria-hidden />
                  新增路由模型
                </Button>
                <Button kind="primary" onClick={() => openForm({ mode: "new", shape: "direct" })}>
                  <Plus aria-hidden />
                  新增直连模型
                </Button>
              </>
            )}
          </>
        }
      />
      <InventorySummary
        title="模型文档 · 当前配置"
        total={models.items?.length ?? null}
        totalLabel="含内部直连目标"
        breakdown={countGroups(models.items?.map((m) => SHAPE_LABEL[shapeOf(m.doc)]) ?? [])}
        breakdownLabel="完整模型集合的派发类型"
        secondaryTitle="对外公共模型"
        secondaryValue={models.items ? counts.facing : null}
        secondaryFoot="由公共模型生成的对外路由文档。内部直连目标默认在下方列表中折叠。"
        error={models.error !== null}
        secondaryError={models.error !== null}
        onRetry={refresh}
      >
        <dl className="ops-facts">
          <div><dt>手工创建</dt><dd>{models.items ? counts.manual : "—"}</dd></div>
          <div><dt>内部直连目标</dt><dd>{models.items ? counts.internal : "—"}</dd></div>
        </dl>
      </InventorySummary>
      <div className="mb-6 grid gap-3 empty:hidden [&>.note]:m-0">
        <Msg note={note} />
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
      </div>

      {/* 编辑表单先于列表，视觉顺序与编号、键盘顺序一致。 */}
      <div
        className={cn(
          "ops-editor-grid",
          split && "ops-editor-open",
        )}
      >
        {editor && <div className="ops-editor">{editor}</div>}
        <Card className="ops-list mb-0">
          <CardHeader
            number={editor ? 4 : 3}
            title="全部模型"
            description={
              <>
                租户按对外名称调用模型。直连模型经一个上游账号调用上游模型；路由模型按选路策略在若干直连模型之间选择。每个模型可服务的分组来自它的上游账号：普通租户只能用到其分组内的账号。
                <span className="mt-1.5 block">
                  要让租户看到和购买的模型，请在「上游账号 → 模型」中建为公共模型，它生成的直连模型只在内部使用；在这里新建的模型会出现在分组定价和租户授权中。
                </span>
              </>
            }
            className="mb-0 items-center"
            actions={
              <label className="inline-flex cursor-pointer items-center gap-2 text-caption text-ink-soft">
                <input type="checkbox" checked={showInternal} onChange={(e) => setShowInternal(e.target.checked)} />
                <span>显示内部直连模型（{counts.internal}）</span>
              </label>
            }
          />
          <RecordTableToolbar search={{ value: table.query, onChange: table.setQuery, label: "搜索模型", placeholder: "模型名称、标识、上游模型或策略" }}><label className="text-caption">模型类型<select value={shapeFilter} onChange={(e) => setShapeFilter(e.target.value)}><option value="">全部类型</option>{Object.entries(SHAPE_LABEL).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label></RecordTableToolbar>
          {models.items === null ? (
            !models.error && <Loading />
          ) : models.items.length === 0 ? (
            <EmptyState text="还没有模型。" />
          ) : table.filteredCount === 0 ? (
            <EmptyState text="没有模型。" />
          ) : (
            <DataTable label="全部模型" scroll={split}>
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
                {table.rows.map((m) => {
                  const shape = shapeOf(m.doc);
                  const hasForm = shape === "direct" || shape === "routing";
                  const src = modelSource(m);
                  const from = src.kind === "public" ? publicById?.get(src.id) : undefined;
                  return (
                    <Fragment key={m.id}>
                      <tr className={editing?.id === m.id ? "bg-well" : undefined}>
                        <td className="font-medium">
                          <span className={cn("block truncate", split ? "max-w-[12rem]" : "max-w-[20rem]")} title={nameOf(m)}>
                            {nameOf(m)}
                          </span>
                        </td>
                        <td className="whitespace-normal">
                          <Source model={m} source={src} from={from} publicRead={publicById !== null} catalog={catalog} />
                        </td>
                        <td className="text-ink-soft">{SHAPE_LABEL[shape]}{shape === "routing" && <RoutePolicy routing={m.doc.routing} defaultStrategy={schema && "preset" in schema ? schema.preset : null} compact />}</td>
                        <td className="whitespace-normal">
                          <div className="min-w-56">
                            <Upstream model={m} catalog={catalog} />
                          </div>
                        </td>
                        <td>
                          <RolloutBadge rollout={m.rollout} />
                        </td>
                        <td className="right">
                          {src.kind === "public" ? (
                            <>
                              <Button size="sm" onClick={() => onGoto("public-routes", src.id)}>公共路由</Button>
                              {src.generated && <Button size="sm" onClick={() => openForm({ mode: "edit", id: m.id })}>查看</Button>}
                              <span className="cell-note ml-auto">由公共模型生成，在公共模型与路由中维护</span>
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
                                <Button size="sm" kind="quiet" disabled={busy} onClick={() => void startDelete(m)}>
                                  删除
                                </Button>
                              )}
                            </>
                          )}
                        </td>
                      </tr>
                      {deleting?.id === m.id && (
                        <tr className="dt-detail">
                          <td colSpan={6} className="whitespace-normal">
                            <DeleteWarning holders={deleting.holders} />
                          </td>
                        </tr>
                      )}
                      {src.kind === "public" && !from && publicById !== null && (
                        <tr className="dt-detail">
                          <td colSpan={6} className="whitespace-normal">
                            <p className="hint text-caption">
                              它的公共模型「{src.id}」已不在列表里，请刷新或进入公共模型与路由核对。
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
          <RecordTablePager {...table} total={table.filteredCount} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} label="模型分页" />
        </Card>
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
          <div className="flex flex-wrap items-baseline gap-x-1.5">
            <span>{account?.doc.display_name || doc.provider_key_id || "—"}</span>
            <span className="text-muted">·</span>
            <code className="text-caption">{doc.model_name ?? "—"}</code>
          </div>
          <ServedGroups>{groupsOf(model, catalog)}</ServedGroups>
        </>
      );
    }
    case "routing":
      return (
        <ul className="grid gap-2">
          {(doc.routing?.targets ?? []).map((t, i) => {
            const target = targetModel(t, catalog);
            return (
              <li key={i} className="min-w-0">
                <span>{target ? nameOf(target) : `${t.model_id ?? t.model ?? "—"}（模型不存在）`}</span>
                <ServedGroups className="mt-0.5">{groupsOf(target, catalog)}</ServedGroups>
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
  if (source.kind === "manual") return <span className="tag">手工创建</span>;
  const name = from?.name;
  const account = model.doc.provider_key_id ? catalog.accounts.get(model.doc.provider_key_id) : undefined;
  // The row a caller names is 对外; the per-account documents behind it are
  // 内部直连 — reachable only as that row's targets, never granted or priced.
  const shared = (
    <div className="sub mt-1 max-w-[16rem] min-w-28 font-mono text-meta [overflow-wrap:anywhere]">
      {publicRead ? (name ?? `${source.id}（公共模型已不在列表）`) : "…"}
    </div>
  );
  if (!source.generated)
    return (
      <>
        <span className="tag border-ink/20 bg-surface text-ink">对外</span>
        {shared}
      </>
    );
  return (
    <>
      <span className="tag border-dashed">内部直连</span>
      {shared}
      <div className="sub font-mono text-meta">{account ? `账号：${account.doc.display_name || account.id}` : "账号：已不存在"}</div>
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
  const [affinity, setAffinity] = useState(routingDraftOf(routing, "").affinity);
  const [cooldown, setCooldown] = useState(doc.cooldown ?? null);
  const [cooldownProblem, setCooldownProblem] = useState<string | null>(null);
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
      if (!sameRouting({ cooldown }, { cooldown: doc.cooldown ?? null })) {
        if (cooldown === null) delete next.cooldown;
        else next.cooldown = cooldown;
      }
      return next;
    }
    const nextRouting = isNew ? {} : { ...routing };
    if (isNew ? shownStrategy !== "" : strategy !== initial.strategy) nextRouting.strategy = shownStrategy;
    if (shownStrategy === "consistent_hash") {
      if (affinity === "api_key") nextRouting.hash_on = [{ type: "api_key" }];
      else if (affinity === "default") delete nextRouting.hash_on;
    } else if (strategy !== initial.strategy) delete nextRouting.hash_on;
    const rowsChanged = rows.map(rowFields).join() !== initial.rows.map(rowFields).join();
    if (isNew || rowsChanged) nextRouting.targets = rows.map(targetOf);
    next.routing = nextRouting;
    return next;
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (shape === "direct") {
      const problem = cooldownProblem ?? validateCooldown(cooldown);
      if (problem) { setErr(problem); return; }
    }
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
    <form ref={form} className="panel @container mb-0" aria-label={title} onSubmit={submit}>
      <CardHeader number={3} title={title} className="mb-5" />
      <div className="grid gap-x-4 @md:grid-cols-2">
        <div className="min-w-0 @md:col-span-2">
          <Field label="对外名称" error={at("/display_name")}>
            {(aria) => <input required disabled={locked} value={name} onChange={(e) => setName(e.target.value)} {...aria} />}
          </Field>
          {!isNew && !readOnly && (
            <p className="hint -mt-2 mb-4 text-caption">
              {RENAME_HINT}
              {RENAME_BY_NAME}
            </p>
          )}
        </div>

        {shape === "direct" ? (
          <>
            <Field label="上游账号" error={at("/provider_key_id")}>
              {(aria) => (
                <select required disabled={locked} value={accountId} onChange={(e) => chooseAccount(e.target.value)} {...aria}>
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
            {account && (
              <p className="hint -mt-2 mb-4 flex flex-wrap items-center gap-x-1.5 gap-y-1 text-caption @md:col-span-2">
                <span>该账号可服务的分组：</span>
                <GroupBadges groups={account.doc.groups ?? []} empty="—（账号未设分组）" />
              </p>
            )}
            <div className="min-w-0 @md:col-span-2">
              <Field label="上游模型" error={at("/model_name")}>
                {(aria) => (
                  <input
                    required
                    disabled={locked}
                    className="font-mono text-caption"
                    value={upstream}
                    onChange={(e) => setUpstream(e.target.value)}
                    {...aria}
                  />
                )}
              </Field>
            </div>
          </>
        ) : schema && "error" in schema ? (
          <div className="f">
            <span>选路策略</span>
            <div className="note crit m-0" role="alert">
              {schema.error}
            </div>
          </div>
        ) : (
          <RoutingModeField
            value={{ strategy: shownStrategy, affinity }}
            onChange={(next) => { setStrategy(next.strategy); setAffinity(next.affinity); }}
            disabled={locked || strategies === null}
            strategies={strategies}
            storedHashOn={routing.hash_on}
            error={at("/routing/strategy")}
            hashError={at("/routing/hash_on")}
          />
        )}
      </div>

      {shape === "direct" && <CooldownEditor value={cooldown} disabled={locked} onChange={setCooldown} onValidityChange={setCooldownProblem} error={at("/cooldown")} />}


      {shape === "routing" && (
        <div className="@container mt-2 border-t border-line pt-5">
          <h4 className="mb-3 text-body font-semibold">路由目标（{rows.length}）</h4>
          <div className="overflow-hidden rounded-cell border border-line">
            <div aria-hidden="true" className={cn(MANUAL_ROW, "hidden border-b border-line bg-well px-3 py-2 text-caption font-medium text-ink-soft @lg:grid")}>
              <span>目标模型</span>
              <span>权重</span>
              <span>优先级</span>
              <span />
            </div>
            {rows.map((r, i) => {
              const target = catalog.models.get(r.modelId);
              return (
                <div key={r.key} className={cn(MANUAL_ROW, "border-line p-3 not-last:border-b")} role="group" aria-label={`目标 ${i + 1}`}>
                  <div className="col-span-2 min-w-0 @lg:col-span-1">
                    <RowField label="目标模型" error={targetAt(r, "model_id")}>
                      {(aria) => (
                        <select required disabled={locked} value={r.modelId} onChange={(e) => setRow(r.key, { modelId: e.target.value })} {...aria}>
                          <option value="">（选择直连模型）</option>
                          {targetOptions(r).map((o) => (
                            <option key={o.id} value={o.id}>
                              {o.label}
                            </option>
                          ))}
                        </select>
                      )}
                    </RowField>
                    {r.modelId && <ServedGroups className="mt-1.5">{groupsOf(target, catalog)}</ServedGroups>}
                  </div>
                  <RowField label="权重" error={targetAt(r, "weight")}>
                    {(aria) => (
                      <input
                        type="number"
                        min={0}
                        step={1}
                        className="text-right tabular-nums"
                        disabled={locked}
                        value={r.weight}
                        onChange={(e) => setRow(r.key, { weight: e.target.value })}
                        {...aria}
                      />
                    )}
                  </RowField>
                  <RowField label="优先级" error={targetAt(r, "priority")}>
                    {(aria) => (
                      <input
                        type="number"
                        step={1}
                        className="text-right tabular-nums"
                        disabled={locked}
                        value={r.priority}
                        onChange={(e) => setRow(r.key, { priority: e.target.value })}
                        {...aria}
                      />
                    )}
                  </RowField>
                  <div className="col-span-2 flex min-h-11 items-center justify-end gap-2 @lg:col-span-1">
                    {!readOnly &&
                      rows.length > 1 &&
                      (removing === r.key ? (
                        <>
                          <Button
                            kind="danger"
                            size="sm"
                            type="button"
                            disabled={busy}
                            onClick={() => {
                              setRows((list) => list.filter((x) => x.key !== r.key));
                              setRemoving(null);
                            }}
                          >
                            确认移除
                          </Button>
                          <Button size="sm" type="button" disabled={busy} onClick={() => setRemoving(null)}>
                            取消
                          </Button>
                        </>
                      ) : (
                        <Button size="sm" kind="quiet" type="button" disabled={busy} onClick={() => setRemoving(r.key)}>
                          <Trash2 aria-hidden />
                          移除
                        </Button>
                      ))}
                  </div>
                </div>
              );
            })}
          </div>
          <div className="mt-3 flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
            {!readOnly && (
              <Button type="button" size="sm" disabled={busy} onClick={addRow}>
                <Plus aria-hidden />
                添加目标
              </Button>
            )}
            <p className="hint max-w-[44ch] text-caption">权重、优先级留空时用网关的默认值。优先级高的一组先用，这一组都不可用时才用下一组。</p>
          </div>
        </div>
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

      <div className={FORM_FOOTER}>
        {readOnly ? (
          <Button type="button" onClick={onClose}>
            关闭
          </Button>
        ) : (
          <>
            <Button type="button" disabled={busy} onClick={onClose}>
              取消
            </Button>
            <Button kind="primary" type="submit" busy={busy} busyLabel="保存中…" disabled={cooldownProblem !== null}>
              保存
            </Button>
          </>
        )}
      </div>
    </form>
  );
}

/** A manual routing model's target row: the model, its weight and priority, its removal. */
const MANUAL_ROW = "grid grid-cols-2 items-start gap-x-3 gap-y-2 @lg:grid-cols-[minmax(0,1fr)_6.5rem_6.5rem_minmax(6rem,auto)]";
