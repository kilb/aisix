import { Plus, RefreshCw } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { Field, useRefusalFocus } from "../ui/Field";
import { Msg, type Note } from "../components/Msg";
import { RolloutBadge } from "../components/RolloutBadge";
import { REDACTED, SecretInput } from "../components/SecretInput";
import { ApiError, groups as groupsApi, settings, tenants, type AdminKey, type ResourceView, type Tenant } from "../lib/api";
import { messageFor, problemAt } from "../lib/errors";
import { errorText, maskUrl } from "../lib/fmt";
import { enumAt, loadSchema } from "../lib/schema";
import { useResource } from "../lib/useResource";
import { Card, CardHeader, PageHeader } from "../components/page";
import { cn } from "../lib/cn";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { GroupPicker, groupChoices, useLoad, type GroupChoice } from "./Tenants";
import { ChannelModels } from "./providers/ChannelModels";
import { GroupBadges } from "./providers/GroupBadges";
import { InventorySummary, countGroups } from "./providers/OperationsBento";

/** The fields of a `provider_key` this page edits; every other field is kept as stored. */
interface ProviderKeyDoc {
  display_name?: string;
  adapter?: string | null;
  provider?: string;
  api_base?: string | null;
  api_key?: string;
  /** The former spelling of `api_key`, which the control plane renames on write. */
  secret?: string;
  groups?: string[];
  [field: string]: unknown;
}

type Account = ResourceView<ProviderKeyDoc>;

/** What the form reads from the gateway's `provider_key` schema. */
type SchemaInfo = { adapters: string[] } | { error: string };

/**
 * The 供应商标识 field's help. The Dashboard's own words: the schema's
 * description is the public API reference, in English.
 */
const PROVIDER_HINT =
  "上游供应商的标识，例如 openai、deepseek、anthropic，或模型目录中的 ID。网关据此选择该供应商专属的调用方式，并校验接口地址。";

/** Which form is open: a new account (with its starting groups), or one account. */
type Open = { mode: "new"; groups: string[]; groupsNote: string | null } | { mode: "edit"; id: string };

const GROUPS_HINT = "分组名只有运维可见；普通租户只能用到其分组内的上游账号";

/** A form card's closing bar: its buttons at the bottom right, on a muted strip the card's width. */
const FORM_FOOTER =
  "-mx-7 -mb-7 mt-6 flex max-w-none flex-wrap items-center justify-end gap-2 rounded-b-card border-t border-line bg-well px-7 py-3 [&_.hint]:text-ink-soft";

/** How a referrer's kind is named on this page. */
const KIND_LABEL: Record<string, string> = { models: "模型" };

/**
 * The refusal details each field of the account form shows beside itself;
 * the others stay in the list under the form. A credential's refusal names
 * its slot, `api_key` or its former spelling `secret`.
 */
const ACCOUNT_FIELDS = {
  name: ["/display_name"],
  adapter: ["/adapter"],
  provider: ["/provider"],
  apiBase: ["/api_base"],
  secret: ["api_key", "/api_key", "secret", "/secret"],
  groups: ["/groups"],
};

/** Whether `path` is at `field` or inside it. */
const within = (fields: string[]) => (path: string) =>
  fields.some((f) => path === f || (f.startsWith("/") && path.startsWith(`${f}/`)));

function nameOf(a: Account): string {
  return a.doc.display_name || a.id;
}

/**
 * Upstream accounts (`provider_keys`): the list, the form, a two-step
 * delete, and the platform's provider groups on each account.
 */
export function Providers({ canEdit }: TabProps) {
  const pk = useResource<ProviderKeyDoc>("provider_keys");
  // The live groups an account may list: chosen by display name, stored by routing name.
  const groups = useLoad(() => groupsApi.list(), []);
  const [schema, setSchema] = useState<SchemaInfo | null>(null);
  const [open, setOpen] = useState<Open | null>(null);
  const [opening, setOpening] = useState(false);
  const [note, setNote] = useState<Note | null>(null);
  // The row whose `删除` was clicked once, awaiting `确认删除`.
  const [deleting, setDeleting] = useState<string | null>(null);
  const [deleteFailure, setDeleteFailure] = useState<{ name: string; error: ApiError } | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    loadSchema("provider_key").then(
      (s) => {
        let info: SchemaInfo;
        try {
          info = { adapters: enumAt(s, "/properties/adapter") };
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

  // A new account starts in the platform's default groups.
  async function openNew() {
    setNote(null);
    setOpening(true);
    // A group created since the page was opened must be offered here, so the
    // list is read again with the form.
    void groups.reload();
    try {
      const defaults = await settings.tenantDefaults();
      setOpen({ mode: "new", groups: defaults.default_provider_groups, groupsNote: null });
    } catch (e) {
      setOpen({
        mode: "new",
        groups: [],
        groupsNote: `无法读取平台默认分组（${errorText(e)}），请手动填写分组。`,
      });
    } finally {
      setOpening(false);
    }
  }

  function saved(view: Account) {
    setOpen(null);
    setNote({ kind: "ok", text: `已保存「${nameOf(view)}」。` });
    // The row's badge follows the rollout; a failed read leaves the last view.
    pk.watch(view.id).catch(() => undefined);
  }

  async function remove(a: Account) {
    setBusy(true);
    setNote(null);
    setDeleteFailure(null);
    try {
      await pk.remove(a.id, a.version);
      setNote({ kind: "ok", text: `已删除「${nameOf(a)}」。` });
      if (open?.mode === "edit" && open.id === a.id) setOpen(null);
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(String(e), 0, "client", [], [], null);
      setDeleteFailure({ name: nameOf(a), error });
    } finally {
      setBusy(false);
      setDeleting(null);
    }
  }

  const editing = open?.mode === "edit" ? pk.items?.find((a) => a.id === open.id) : undefined;
  // The stored names are offered too: a group retired since the account was
  // saved must still be listed, so it can be taken away.
  const newChoices = groupChoices(groups.data, open?.mode === "new" ? open.groups : []);
  const editChoices = groupChoices(groups.data, editing?.doc.groups ?? []);

  // 编辑表单旁保留完整账号列，表格在自己的卡片中滚动。
  const split = open?.mode === "new" || editing !== undefined;

  const assignedGroups = new Set(pk.items?.flatMap((a) => a.doc.groups ?? []) ?? []);
  const ungrouped = pk.items?.filter((a) => (a.doc.groups?.length ?? 0) === 0).length ?? 0;

  return (
    <>
      <PageHeader
        eyebrow="PROVIDERS · 上游连接"
        title="上游账号"
        dataLead={pk.items ? <>已配置 {pk.items.length} 个上游账号，覆盖 {assignedGroups.size} 个分组。账号的协议、模型映射和密钥在此维护。</> : "维护上游连接、协议族、分组与模型映射。"}
        actions={
          <>
            <Button type="button" onClick={() => void pk.reload()}>
              <RefreshCw aria-hidden />
              刷新
            </Button>
            {canEdit && !open && (
              <Button kind="primary" disabled={opening} onClick={() => void openNew()}>
                <Plus aria-hidden />
                新增上游账号
              </Button>
            )}
          </>
        }
      />
      <InventorySummary
        title="上游账号 · 当前配置"
        total={pk.items?.length ?? null}
        totalLabel="已配置账号"
        breakdown={countGroups(pk.items?.map((a) => a.doc.adapter || "未声明协议族") ?? [])}
        breakdownLabel="按已保存的协议族统计账号"
        secondaryTitle="账号覆盖的分组"
        secondaryValue={pk.items ? assignedGroups.size : null}
        secondaryFoot="按账号中保存的分组名去重；分组只对运维可见。"
        error={pk.error !== null}
        secondaryError={pk.error !== null}
        onRetry={() => void pk.reload()}
      >
        <dl className="ops-facts">
          <div><dt>已分配分组的账号</dt><dd>{pk.items ? pk.items.length - ungrouped : "—"}</dd></div>
          <div><dt>未分配分组的账号</dt><dd>{pk.items ? ungrouped : "—"}</dd></div>
        </dl>
      </InventorySummary>
      <div className="mb-6 grid gap-3 empty:hidden [&>.note]:m-0">
        <Msg note={note} />
        {deleteFailure &&
          (deleteFailure.error.code === "version_mismatch" ? (
            <ConflictNote
              onReload={() => {
                setDeleteFailure(null);
                void pk.reload();
              }}
            />
          ) : (
            <div className="note crit" role="alert">
              无法删除「{deleteFailure.name}」：{messageFor(deleteFailure.error)}
              {(deleteFailure.error.counts?.public_models ?? 0) > 0 && (
                <p>
                  该账号仍为 {deleteFailure.error.counts?.public_models} 个公共模型提供服务，请先在该账号的「模型」里取消勾选，再删除。
                </p>
              )}
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
        {pk.error && (
          <div className="note crit" role="alert">
            读取失败：{messageFor(pk.error)}
          </div>
        )}
      </div>

      {/* 编辑表单先于列表朗读，账号模型映射在两者下方铺满一行。 */}
      <div className={cn("ops-editor-grid", split && "ops-editor-open")}>
        {open?.mode === "new" && (
          <div className="ops-editor">
            <AccountForm
              key="new"
              account={null}
              initialGroups={open.groups}
              groupsNote={open.groupsNote}
              choices={newChoices}
              groupsError={groups.error}
              schema={schema}
              readOnly={false}
              create={pk.create}
              save={pk.save}
              onSaved={saved}
              onClose={() => setOpen(null)}
              onReload={() => undefined}
            />
          </div>
        )}
        {editing && (
          <div className="ops-editor">
            {/* Keyed by version: a reload that finds a newer version starts the edit over. */}
            <AccountForm
              key={`${editing.id}:${editing.version}`}
              account={editing}
              initialGroups={[]}
              groupsNote={null}
              choices={editChoices}
              groupsError={groups.error}
              schema={schema}
              readOnly={!canEdit}
              create={pk.create}
              save={pk.save}
              onSaved={saved}
              onClose={() => setOpen(null)}
              onReload={() => void pk.reload()}
            />
          </div>
        )}
        <Card className="ops-list mb-0">
          <CardHeader
            number={split ? 4 : 3}
            title="全部账号"
            description={<>网关调用上游供应商所用的账号。密钥保存后不再显示。{GROUPS_HINT}。</>}
            className="mb-0"
            actions={pk.items && <span className="text-caption text-muted tabular-nums">{pk.items.length} 个</span>}
          />
          {pk.items === null ? (
            !pk.error && <Loading />
          ) : pk.items.length === 0 ? (
            <EmptyState text="还没有上游账号。" />
          ) : (
            <DataTable label="上游账号" scroll={split}>
              <thead>
                <tr>
                  <th>名称</th>
                  <th>协议族</th>
                  <th>供应商标识</th>
                  <th>接口地址</th>
                  <th>分组</th>
                  <th>状态</th>
                  <th className="right">操作</th>
                </tr>
              </thead>
              <tbody>
                {pk.items.map((a) => (
                  <tr key={a.id} className={editing?.id === a.id ? "bg-well" : undefined}>
                    <td className="font-medium">
                      <span className={cn("block truncate", split ? "max-w-[12rem]" : "max-w-[18rem]")} title={nameOf(a)}>
                        {nameOf(a)}
                      </span>
                    </td>
                    <td className="font-mono text-caption text-ink-soft">{a.doc.adapter ?? "—"}</td>
                    <td className="font-mono text-caption text-ink-soft">{a.doc.provider || "—"}</td>
                    <td>
                      {a.doc.api_base ? (
                        <code className="block max-w-[18rem] truncate text-caption text-ink-soft" title={maskUrl(a.doc.api_base)}>
                          {maskUrl(a.doc.api_base)}
                        </code>
                      ) : (
                        "—"
                      )}
                    </td>
                    <td className="whitespace-normal">
                      <GroupBadges groups={a.doc.groups ?? []} />
                    </td>
                    <td>
                      <RolloutBadge rollout={a.rollout} />
                    </td>
                    <td className="right">
                      {deleting === a.id ? (
                        <>
                          <Button kind="danger" size="sm" busy={busy} busyLabel="确认删除" onClick={() => void remove(a)}>
                            确认删除
                          </Button>
                          <Button size="sm" disabled={busy} onClick={() => setDeleting(null)}>
                            取消
                          </Button>
                        </>
                      ) : (
                        <>
                          <Button
                            size="sm"
                            onClick={() => {
                              setNote(null);
                              // A group created since the page was opened must be
                              // offered by this form, so the list is read again with it.
                              void groups.reload();
                              setOpen({ mode: "edit", id: a.id });
                            }}
                          >
                            {canEdit ? "编辑" : "查看"}
                          </Button>
                          {canEdit && (
                            <Button
                              size="sm"
                              kind="quiet"
                              disabled={busy}
                              onClick={() => {
                                setDeleteFailure(null);
                                setDeleting(a.id);
                              }}
                            >
                              删除
                            </Button>
                          )}
                        </>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
          )}
        </Card>
        {editing && (
          // 模型映射独立于账号表单，操作按钮不会提交账号文档。
          <div className="ops-channel-models">
            <ChannelModels
              key={`models:${editing.id}`}
              accountId={editing.id}
              accountName={nameOf(editing)}
              groups={editing.doc.groups ?? []}
              canEdit={canEdit}
            />
          </div>
        )}
      </div>
    </>
  );
}

/** Tenants and keys an account's group change affects. */
interface Impact {
  /** Tenants whose default groups no longer include the account. */
  tenants: Tenant[];
  /** Keys whose own groups no longer include the account. */
  keys: { key: AdminKey; tenant: Tenant }[];
}

/** The most tenants whose keys are read at once. */
const KEY_READS_IN_FLIGHT = 6;

/**
 * Who loses an account whose groups go from `before` to `after`. Groups
 * "lose" it when they meet `before` but not `after`; a group list that is
 * empty reaches every account, which is what a legacy groupless key still
 * does for the platform's own tenant.
 *
 * - Active and suspended tenants only: a suspended tenant's keys come back
 *   on reactivation. Pending sign-ups are not read.
 * - A key is named when its own group loses the account. A key is never in
 *   two groups, chosen once at creation.
 * - A tenant is named when its groups lose the account, unless every key it
 *   has names a group that still reaches it. A tenant with no key yet is
 *   named: its next key takes one of those groups.
 *
 * Every tenant's keys are read, at most `KEY_READS_IN_FLIGHT` at a time.
 * This per-tenant cost stands until a backend endpoint reports the impact.
 */
async function whoLoses(before: string[], after: string[]): Promise<Impact> {
  const loses = (groups: string[]) =>
    groups.length > 0 && groups.some((g) => before.includes(g)) && !groups.some((g) => after.includes(g));
  /** The groups a key reaches by: its one group, or the old rule for a key without one. */
  const ownGroups = (k: AdminKey) =>
    k.group !== null ? [k.group.name] : (k.allowed_provider_groups ?? []);
  const live = (await tenants.list()).filter((t) => t.status === "active" || t.status === "suspended");
  const keysOf = new Map<string, AdminKey[]>();
  let next = 0;
  const worker = async () => {
    for (let t = live[next++]; t !== undefined; t = live[next++]) keysOf.set(t.id, await tenants.keys(t.id));
  };
  await Promise.all(Array.from({ length: Math.min(KEY_READS_IN_FLIGHT, live.length) }, worker));

  const impact: Impact = { tenants: [], keys: [] };
  for (const tenant of live) {
    const keys = keysOf.get(tenant.id) ?? [];
    for (const key of keys) {
      if (ownGroups(key).length > 0 && loses(ownGroups(key))) impact.keys.push({ key, tenant });
    }
    const covered =
      keys.length > 0 &&
      keys.every((k) => {
        const own = ownGroups(k);
        return own.length > 0 && own.some((g) => after.includes(g));
      });
    if (loses(tenant.allowed_provider_groups) && !covered) impact.tenants.push(tenant);
  }
  return impact;
}

function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

/**
 * One account's form: new (`account` null), an edit, or a viewer's
 * read-only view. An edit writes back the stored document with only the
 * fields changed here replaced, so a field this form does not show, and a
 * field left as it was, is sent exactly as read.
 */
function AccountForm({
  account,
  initialGroups,
  groupsNote,
  choices,
  groupsError,
  schema,
  readOnly,
  create,
  save,
  onSaved,
  onClose,
  onReload,
}: {
  account: Account | null;
  initialGroups: string[];
  groupsNote: string | null;
  /** The live groups, by display name; an account lists only groups that exist. */
  choices: GroupChoice[];
  groupsError: string | null;
  schema: SchemaInfo | null;
  readOnly: boolean;
  create: (doc: ProviderKeyDoc) => Promise<Account>;
  save: (id: string, doc: ProviderKeyDoc, version: number) => Promise<Account>;
  onSaved: (view: Account) => void;
  onClose: () => void;
  onReload: () => void;
}) {
  const isNew = account === null;
  const doc = account?.doc ?? {};
  const stored = typeof (doc.api_key ?? doc.secret) === "string";
  const adapters = schema && "adapters" in schema ? schema.adapters : null;
  // As the form starts, to tell what the operator changed. The address is
  // shown with its userinfo password masked.
  const [initial] = useState(() => ({
    name: doc.display_name ?? "",
    adapter: doc.adapter ?? (isNew ? (adapters?.[0] ?? "") : ""),
    provider: doc.provider ?? "",
    apiBase: maskUrl(doc.api_base ?? ""),
    groups: isNew ? initialGroups : (doc.groups ?? []),
  }));
  const [name, setName] = useState(initial.name);
  const [adapter, setAdapter] = useState(initial.adapter);
  const [provider, setProvider] = useState(initial.provider);
  const [apiBase, setApiBase] = useState(initial.apiBase);
  const [secret, setSecret] = useState<string | undefined>(stored ? REDACTED : "");
  const [groups, setGroups] = useState(initial.groups);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<ApiError | string | null>(null);
  const [conflict, setConflict] = useState(false);
  // Set while a save that drops groups awaits `确认移除分组`; the impact is
  // null while it is read.
  const [removal, setRemoval] = useState<{ dropped: string[]; impact: Impact | string | null } | null>(null);
  // Numbers each impact lookup; a result is applied only while its number is the latest.
  const lookup = useRef(0);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  // A new account takes the first protocol family once the schema arrives.
  useEffect(() => {
    const first = adapters?.[0];
    if (isNew && adapter === "" && first) setAdapter(first);
  }, [isNew, adapter, adapters]);

  const title = isNew ? "新增上游账号" : readOnly ? `上游账号「${initial.name}」` : `编辑上游账号「${initial.name}」`;
  const locked = readOnly || busy || removal !== null;
  const details = err instanceof ApiError ? err.details : [];
  const at = (field: keyof typeof ACCOUNT_FIELDS) => problemAt(details, within(ACCOUNT_FIELDS[field]));
  /** A refusal this form shows beside the field of a list entry it names by index. */
  const entryAt = (field: keyof typeof ACCOUNT_FIELDS, index: number) => problemAt(details, (path) => path === `${field}/${index}`);
  // Without its schema the adapter has no field to show a refusal at.
  const shown = Object.entries(ACCOUNT_FIELDS).flatMap(([field, paths]) =>
    field === "adapter" && schema && "error" in schema ? [] : paths,
  );
  const listed = details.filter((d) => !within(shown)(d.path));

  // The adapter's options: the schema's, plus what is stored when the
  // schema lacks it, plus "unset" for an existing account without one, so
  // opening an account and saving it never writes a family nobody chose.
  const adapterOptions = [
    ...(!isNew && initial.adapter === "" ? [""] : []),
    ...(adapters ?? []),
    ...(initial.adapter !== "" && !adapters?.includes(initial.adapter) ? [initial.adapter] : []),
  ];

  function changedDoc(): ProviderKeyDoc {
    const next: ProviderKeyDoc = isNew ? {} : { ...doc };
    // Compared as typed, so a stored value left alone is never rewritten.
    const put = (field: string, typed: string, before: string) => {
      if (!isNew && typed === before) return;
      const value = typed.trim();
      if (value === "") delete next[field];
      else next[field] = value;
    };
    put("display_name", name, initial.name);
    put("adapter", adapter, initial.adapter);
    put("provider", provider, initial.provider);
    put("api_base", apiBase, initial.apiBase);
    if (secret === undefined) {
      delete next.api_key;
      delete next.secret;
    } else if (secret !== REDACTED) {
      // One spelling: a replaced secret is written as `api_key`.
      delete next.secret;
      next.api_key = secret;
    }
    if (isNew ? groups.length > 0 : !sameList(groups, initial.groups)) next.groups = groups;
    return next;
  }

  async function write() {
    setBusy(true);
    setErr(null);
    setConflict(false);
    try {
      const body = changedDoc();
      const view = account ? await save(account.id, body, account.version) : await create(body);
      onSaved(view);
    } catch (e) {
      setRemoval(null);
      if (e instanceof ApiError && e.code === "version_mismatch") setConflict(true);
      else setErr(e instanceof ApiError ? e : String(e));
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    if (apiBase !== initial.apiBase && /\/\/[^/@]*:\*\*\*@/.test(apiBase)) {
      setErr("接口地址中的口令显示为 ***：修改地址时请填写完整的口令。");
      return;
    }
    const dropped = isNew ? [] : initial.groups.filter((g) => !groups.includes(g));
    if (dropped.length === 0) {
      await write();
      return;
    }
    // Removing a group can close keys that reach this account only through
    // it: name them first, and save only on confirmation. Only the latest
    // lookup may fill the confirmation: one started for a removal that was
    // cancelled can finish after the next removal opened.
    const ticket = ++lookup.current;
    setRemoval({ dropped, impact: null });
    let impact: Impact | string;
    try {
      impact = await whoLoses(initial.groups, groups);
    } catch (e) {
      impact = errorText(e);
    }
    if (lookup.current === ticket) setRemoval((r) => r && { ...r, impact });
  }

  return (
    <form ref={form} className="panel @container mb-0" aria-label={title} onSubmit={submit}>
      <CardHeader number={3} title={title} className="mb-5" />
      {/* Two columns in the aside beside the list; four where the form has the page's width. */}
      <div className="grid gap-x-4 @sm:grid-cols-2 @3xl:grid-cols-4">
        <Field label="名称" error={at("name")}>
          {(aria) => <input required disabled={locked} value={name} onChange={(e) => setName(e.target.value)} {...aria} />}
        </Field>
        {schema && "error" in schema ? (
          <div className="f">
            <span>协议族</span>
            <div className="note crit m-0" role="alert">
              {schema.error}
            </div>
          </div>
        ) : (
          <Field label="协议族" error={at("adapter")}>
            {(aria) => (
              <select
                disabled={locked || adapters === null}
                value={adapter}
                onChange={(e) => setAdapter(e.target.value)}
                {...aria}
              >
                {adapterOptions.map((v) => (
                  <option key={v} value={v}>
                    {v === "" ? "（未设置）" : v}
                  </option>
                ))}
              </select>
            )}
          </Field>
        )}
        <div className="min-w-0 @sm:col-span-2">
          <Field label="供应商标识" hint={PROVIDER_HINT} error={at("provider")}>
            {(aria) => <input disabled={locked} value={provider} onChange={(e) => setProvider(e.target.value)} {...aria} />}
          </Field>
        </div>
        <div className="min-w-0 @sm:col-span-2">
          <Field label="接口地址" error={at("apiBase")}>
            {(aria) => (
              <input
                disabled={locked}
                className="font-mono text-caption"
                placeholder="https://…"
                value={apiBase}
                onChange={(e) => setApiBase(e.target.value)}
                {...aria}
              />
            )}
          </Field>
        </div>
        <div className="min-w-0 @sm:col-span-2">
          <SecretInput
            label="密钥"
            stored={stored}
            required
            readOnly={locked}
            hint={stored && !readOnly ? "不替换则保留已保存的密钥。改动协议族、供应商标识或接口地址时，需要重新填写密钥。" : undefined}
            problem={at("secret")}
            onChange={setSecret}
          />
        </div>
      </div>
      <fieldset className="tn-section mt-2 border-t border-line pt-5 [&_.tn-groups]:grid-cols-[repeat(auto-fill,minmax(13rem,1fr))] [&_.tn-groups]:gap-x-6">
        <legend className="text-body font-semibold">分组</legend>
        <p className="hint text-caption">{GROUPS_HINT}；只能从已有的分组里选。</p>
        {groupsError && (
          <div className="note crit" role="alert">
            分组读取失败：{groupsError}
          </div>
        )}
        <GroupPicker
          options={choices}
          chosen={groups}
          onChange={setGroups}
          disabled={locked}
          errorFor={(g) => entryAt("groups", groups.indexOf(g)) ?? undefined}
        />
        {at("groups") && (
          <div className="note crit" role="alert">
            {at("groups")}
          </div>
        )}
      </fieldset>
      {groupsNote && <div className="note warn">{groupsNote}</div>}

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

      {removal ? (
        <section className="note warn mb-0" aria-label="移除分组">
          <p>
            <strong>将从该账号移除分组：{removal.dropped.join("、")}</strong>
          </p>
          {removal.impact === null ? (
            <p className="hint">正在查找按这些分组使用该账号的租户与密钥…</p>
          ) : typeof removal.impact === "string" ? (
            <p>无法确认受影响的租户与密钥：{removal.impact}</p>
          ) : removal.impact.tenants.length + removal.impact.keys.length === 0 ? (
            <p>没有租户或密钥会因此用不到该账号。</p>
          ) : (
            <>
              <p>以下租户与密钥会受影响：</p>
              <ul>
                {removal.impact.tenants.map((t) => (
                  <li key={t.id}>租户「{t.name}」：租户默认分组不再包含该账号</li>
                ))}
                {removal.impact.keys.map(({ key, tenant }) => (
                  <li key={key.id}>
                    密钥「{key.label}」（租户「{tenant.name}」）：密钥自己的分组不再包含该账号
                  </li>
                ))}
              </ul>
            </>
          )}
          <div className="mt-3 flex flex-wrap justify-end gap-2">
            <Button type="button" disabled={busy} onClick={() => setRemoval(null)}>
              取消
            </Button>
            <Button
              kind="danger"
              type="button"
              disabled={busy || removal.impact === null}
              busy={busy}
              busyLabel="确认移除分组"
              onClick={() => void write()}
            >
              确认移除分组
            </Button>
          </div>
        </section>
      ) : (
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
              <Button kind="primary" type="submit" busy={busy} busyLabel="保存中…">
                保存
              </Button>
            </>
          )}
        </div>
      )}
    </form>
  );
}
