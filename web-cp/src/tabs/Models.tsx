import { Fragment, useEffect, useRef, useState } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { Field } from "../ui/Field";
import { Msg, type Note } from "../components/Msg";
import { RolloutBadge } from "../components/RolloutBadge";
import { ApiError, tenants, type ResourceView, type Tenant } from "../lib/api";
import { messageFor, problemAt } from "../lib/errors";
import { errorText } from "../lib/fmt";
import { defaultAt, enumAt, loadSchema } from "../lib/schema";
import { useResource } from "../lib/useResource";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";

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
 */
export function Models({ canEdit }: TabProps) {
  const models = useResource<ModelDoc>("models");
  const accounts = useResource<AccountDoc>("provider_keys");
  const [schema, setSchema] = useState<SchemaInfo | null>(null);
  const [open, setOpen] = useState<Open | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  // The row whose `删除` was clicked once, with the tenants granted it (null while read).
  const [deleting, setDeleting] = useState<{ id: string; holders: Tenant[] | string | null } | null>(null);
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

  const editing = open?.mode === "edit" ? models.items?.find((m) => m.id === open.id) : undefined;
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
      {editing && (shapeOf(editing.doc) === "direct" || shapeOf(editing.doc) === "routing") && (
        // Keyed by version: a reload that finds a newer version starts the edit over.
        <ModelForm
          key={`${editing.id}:${editing.version}`}
          model={editing}
          shape={shapeOf(editing.doc) as "direct" | "routing"}
          readOnly={!canEdit}
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
            onClick={() => {
              void models.reload();
              void accounts.reload();
            }}
          >
            刷新
          </Button>
        </h2>
        <p className="hint">
          租户按对外名称调用模型。直连模型经一个上游账号调用上游模型；路由模型按选路策略在若干直连模型之间选择。每个模型可服务的分组来自它的上游账号：普通租户只能用到其分组内的账号。
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
              无法删除「{deleteFailure.name}」：{messageFor(deleteFailure.error)}
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
        {models.items === null ? (
          !models.error && <Loading />
        ) : models.items.length === 0 ? (
          <EmptyState text="还没有模型。" />
        ) : (
          <DataTable>
            <thead>
              <tr>
                <th>对外名称</th>
                <th>类型</th>
                <th>上游账号 / 路由目标</th>
                <th>状态</th>
                <th className="right">操作</th>
              </tr>
            </thead>
            <tbody>
              {models.items.map((m) => {
                const shape = shapeOf(m.doc);
                const hasForm = shape === "direct" || shape === "routing";
                return (
                  <Fragment key={m.id}>
                    <tr>
                      <td>{nameOf(m)}</td>
                      <td>{SHAPE_LABEL[shape]}</td>
                      <td>
                        <Upstream model={m} catalog={catalog} />
                      </td>
                      <td>
                        <RolloutBadge rollout={m.rollout} />
                      </td>
                      <td className="right">
                        {deleting?.id === m.id ? (
                          <>
                            <Button
                              kind="danger"
                              size="sm"
                              disabled={busy || deleting.holders === null}
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
                        <td colSpan={5}>
                          <DeleteWarning holders={deleting.holders} />
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
  const strategies = schema && "strategies" in schema ? schema.strategies : null;
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
    <form className="panel" aria-label={title} onSubmit={submit}>
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
