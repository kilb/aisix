import { useState } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { FailModeView, failModeOf } from "../components/FailModeNote";
import { JsonEditor } from "../components/JsonEditor";
import { RolloutBadge } from "../components/RolloutBadge";
import { ApiError, resources, type Detail, type ResourceView } from "../lib/api";
import { messageFor } from "../lib/errors";
import { useResource } from "../lib/useResource";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { CardHeader, PageHeader, Split } from "../components/page";
import { Plus, RefreshCw } from "lucide-react";
import { InventorySummary, countGroups } from "./providers/OperationsBento";

/**
 * Every gateway resource kind the control plane stores, by the segment
 * `/admin/resources/{kind}` names it with. The writable ones are what
 * operators write directly; the others are derived from tenants, members
 * and keys and are only shown.
 */
const KINDS = [
  { kind: "models", label: "模型", writable: true },
  { kind: "provider_keys", label: "上游账号", writable: true },
  { kind: "cache_policies", label: "缓存策略", writable: true },
  { kind: "rate_limit_policies", label: "限流策略", writable: true },
  { kind: "guardrails", label: "护栏", writable: true },
  { kind: "guardrail_attachments", label: "护栏挂载", writable: true },
  { kind: "observability_exporters", label: "可观测性导出", writable: true },
  { kind: "budget_policy", label: "预算策略", writable: true },
  { kind: "teams", label: "团队", writable: false },
  { kind: "users", label: "用户", writable: false },
  { kind: "api_keys", label: "API 密钥", writable: false },
  { kind: "budgets", label: "预算", writable: false },
] as const;

type KindInfo = (typeof KINDS)[number];

/** Shown wherever a document is derived from tenants, members or keys. */
const MANAGED_NOTE = "由租户与密钥管理生成，在对应页面修改";

/** The one row a kind may hold at most (its id is the environment's). */
const SINGLETONS: readonly string[] = ["budget_policy"];

type Doc = Record<string, unknown>;

/** A row's name: its `name`, else its `display_name`, else nothing. */
function nameOf(doc: Doc): string | null {
  for (const key of ["name", "display_name"]) {
    const v = doc[key];
    if (typeof v === "string" && v) return v;
  }
  return null;
}

function pretty(doc: unknown): string {
  return JSON.stringify(doc, null, 2);
}

/** "全部资源": a JSON editor over every kind, validated by the server. */
export function Resources({ canEdit, initialSection }: TabProps) {
  // A link from another tab may name the kind to open first.
  const [kind, setKind] = useState<string>(
    KINDS.some((k) => k.kind === initialSection) ? (initialSection as string) : KINDS[0].kind,
  );
  const info = KINDS.find((k) => k.kind === kind) ?? KINDS[0];
  return (
    <>
      <PageHeader
        eyebrow="RESOURCES · 完整配置"
        title="全部资源"
        dataLead={<>当前查看「{info.label}」资源。保留完整 JSON、服务端校验与版本冲突恢复。</>}
        actions={
          <label className="f filter filter-wide max-md:w-full">
            <span>资源类型</span>
            <select className="max-md:w-full" value={kind} onChange={(e) => setKind(e.target.value)}>
              {KINDS.map((k) => (
                <option key={k.kind} value={k.kind}>
                  {k.label} · {k.kind}
                </option>
              ))}
            </select>
          </label>
        }
      />
      {/* Keyed by kind: a watch still running for the previous kind stops
          with the page it belongs to, instead of writing its rows here. */}
      <KindPage key={info.kind} info={info} canEdit={canEdit} />
    </>
  );
}

type Editing =
  | { mode: "new" }
  | { mode: "edit"; id: string; version: number; managedBy: string | null };

function KindPage({ info, canEdit }: { info: KindInfo; canEdit: boolean }) {
  const { items, error, reload, create, save, remove, watch } = useResource<Doc>(info.kind);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [text, setText] = useState("");
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [busy, setBusy] = useState(false);
  const [panel, focusRefusal] = useRefusalFocus<HTMLDivElement>();

  const current = editing?.mode === "edit" ? items?.find((i) => i.id === editing.id) : undefined;
  const writable = canEdit && info.writable;
  // A row a tenant, member or key derives stays read-only even in a kind
  // operators write (a tenant's limits policy among the rate-limit policies).
  const derived = !info.writable || (editing?.mode === "edit" && editing.managedBy !== null);
  const editable = writable && !derived;

  function clearOutcome() {
    setDetails([]);
    setFailure(null);
    setConflict(false);
    setConfirmDelete(false);
  }

  function open(view: ResourceView<Doc>) {
    clearOutcome();
    setEditing({ mode: "edit", id: view.id, version: view.version, managedBy: view.managed_by });
    setText(pretty(view.doc));
  }

  function startNew() {
    clearOutcome();
    setEditing({ mode: "new" });
    setText("{}");
  }

  function close() {
    clearOutcome();
    setEditing(null);
  }

  function refused(e: unknown) {
    if (e instanceof ApiError && e.code === "version_mismatch") {
      setConflict(true);
      return;
    }
    if (e instanceof ApiError) {
      const referrers = e.referencedBy.map((r) => `${r.kind} ${r.name ?? r.id}`);
      setFailure(referrers.length > 0 ? `${messageFor(e)}：${referrers.join("、")}` : messageFor(e));
      setDetails(e.details);
    } else {
      setFailure(e instanceof Error ? e.message : String(e));
    }
  }

  /** The editor's text as a document, or null after showing why it is not one. */
  function parsed(): Doc | null {
    try {
      return JSON.parse(text) as Doc;
    } catch (e) {
      setDetails([{ path: "", message: `不是合法的 JSON：${e instanceof Error ? e.message : String(e)}` }]);
      return null;
    }
  }

  async function submit() {
    if (!editing) return;
    clearOutcome();
    const doc = parsed();
    if (doc === null) {
      focusRefusal();
      return;
    }
    setBusy(true);
    try {
      const view =
        editing.mode === "new" ? await create(doc) : await save(editing.id, doc, editing.version);
      // The stored document as the server reads it back: references by id,
      // credentials redacted.
      open(view);
      void watch(view.id).catch(() => undefined);
    } catch (e) {
      refused(e);
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  async function reloadEditing() {
    if (editing?.mode !== "edit") return;
    try {
      open(await resources.get<Doc>(info.kind, editing.id));
      void reload();
    } catch (e) {
      refused(e);
    }
  }

  async function del() {
    if (editing?.mode !== "edit") return;
    clearOutcome();
    setBusy(true);
    try {
      await remove(editing.id, editing.version);
      close();
    } catch (e) {
      refused(e);
    } finally {
      setBusy(false);
    }
  }

  const canCreate = writable && !(SINGLETONS.includes(info.kind) && (items?.length ?? 0) > 0);
  const editingId = editing?.mode === "edit" ? editing.id : null;

  const list = (
    <div className="panel mb-0">
      <CardHeader
        number={3}
        description="按网关的资源定义直接编辑 JSON 文档。保存时由控制面按网关的规则校验，问题列在编辑框下方。"
        as="h2"
        title={
          <span className="inline-flex flex-wrap items-center gap-2">
            {info.label} <code className="lab">{info.kind}</code>
            {items !== null && items.length > 0 && <span className="tag">{items.length}</span>}
          </span>
        }
        actions={
          <>
            <Button type="button" size="sm" onClick={() => void reload()}>
              <RefreshCw aria-hidden />
              刷新
            </Button>
            {canCreate && (
              <Button kind="primary" type="button" size="sm" onClick={startNew}>
                <Plus aria-hidden />
                新建
              </Button>
            )}
          </>
        }
      />
      {!info.writable && <div className="note mt-0">{MANAGED_NOTE}</div>}
      {info.kind === "budget_policy" && items !== null && <FailModeView mode={failModeOf(items.map((i) => i.doc))} />}
      {error && (
        <div className="note crit" role="alert">
          {messageFor(error)}
        </div>
      )}
      {items === null && !error && <Loading />}
      {items !== null && items.length === 0 && <EmptyState text="暂无此类资源。" />}
      {items !== null && items.length > 0 && (
        <DataTable label="资源文档">
          <thead>
            <tr>
              <th>名称</th>
              <th>ID</th>
              <th className="right">版本</th>
              <th>管理方</th>
              <th>下发状态</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {items.map((r) => (
              <tr key={r.id} className={editingId === r.id ? "bg-well" : undefined}>
                <td className="font-medium">{nameOf(r.doc) ?? <span className="hint font-normal">（未命名）</span>}</td>
                <td>
                  <code
                    className={editing ? "inline-block max-w-[24ch] truncate align-middle text-muted" : "text-muted"}
                    title={r.id}
                  >
                    {r.id}
                  </code>
                </td>
                <td className="right num">{r.version}</td>
                <td>{r.managed_by ? <span className="tag">{r.managed_by}</span> : "—"}</td>
                <td>
                  <RolloutBadge rollout={r.rollout} />
                </td>
                <td className="right">
                  <Button size="sm" type="button" onClick={() => open(r)}>
                    {writable && r.managed_by === null ? "编辑" : "查看"}
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </div>
  );

  const editor = editing && (
    <div ref={panel} className="panel mb-0">
      <CardHeader
        number={4}
        as="h2"
        title={
          <span className="inline-flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 break-all">
            {editing.mode === "new" ? `新建${info.label}` : (current && nameOf(current.doc)) || editing.id}
            {current && <RolloutBadge rollout={current.rollout} />}
          </span>
        }
        description={editing.mode === "edit" ? <code className="break-all">{editing.id}</code> : undefined}
      />
      {editing.mode === "edit" && derived && <div className="note mt-0">{MANAGED_NOTE}</div>}
      {editing.mode === "edit" && !derived && !canEdit && <p className="hint mb-3">当前账号只读。</p>}
      <JsonEditor
        value={text}
        onChange={editing.mode === "new" || editable ? setText : undefined}
        details={details}
      />
      {conflict && <ConflictNote onReload={() => void reloadEditing()} />}
      {failure && (
        <div className="note crit" role="alert">
          {failure}
        </div>
      )}
      <div className="-mx-[var(--pad)] -mb-[var(--pad)] mt-6 flex flex-wrap items-center gap-2 rounded-b-card border-t border-line bg-well px-[var(--pad)] py-3 [&_.hint]:text-ink-soft">
        {editing.mode === "edit" && editable && !confirmDelete && (
          <Button type="button" disabled={busy} onClick={() => setConfirmDelete(true)}>
            删除
          </Button>
        )}
        {/* Ruling P1b-R6: a destructive action confirms inline, never in a dialog. */}
        {editing.mode === "edit" && editable && confirmDelete && (
          <>
            <Button kind="danger" type="button" busy={busy} busyLabel="确认删除" onClick={() => void del()}>
              确认删除
            </Button>
            <Button type="button" disabled={busy} onClick={() => setConfirmDelete(false)}>
              取消
            </Button>
          </>
        )}
        <span className="ml-auto flex gap-2">
          {!confirmDelete && (
            <Button type="button" onClick={close}>
              {editing.mode === "new" || editable ? "取消" : "关闭"}
            </Button>
          )}
          {editing.mode === "new" && (
            <Button kind="primary" type="button" busy={busy} busyLabel="创建" onClick={() => void submit()}>
              创建
            </Button>
          )}
          {editing.mode === "edit" && editable && !confirmDelete && (
            <Button kind="primary" type="button" busy={busy} busyLabel="保存" onClick={() => void submit()}>
              保存
            </Button>
          )}
        </span>
      </div>
    </div>
  );

  const rolloutLabels: Record<string, string> = {
    pending: "待下发", propagating: "下发中", applied: "已生效", rejected: "被拒绝", no_gateways: "无在线网关",
  };
  const derivedCount = items?.filter((r) => !info.writable || r.managed_by !== null).length ?? 0;
  return (
    <>
      <InventorySummary
        title={`${info.label} · 当前类型`}
        total={items?.length ?? null}
        totalLabel={`仅统计 ${info.kind}`}
        breakdown={countGroups(items?.map((r) => rolloutLabels[r.rollout.state] ?? r.rollout.state) ?? [])}
        breakdownLabel="当前类型的资源下发状态"
        secondaryTitle="派生或只读的资源"
        secondaryValue={items ? derivedCount : null}
        secondaryFoot="派生文档在租户、密钥或公共模型等来源页维护；完整文档仍可在这里查看。"
        error={error !== null}
        secondaryError={error !== null}
        onRetry={() => void reload()}
      >
        <dl className="ops-facts">
          <div><dt>手工维护的资源</dt><dd>{items ? items.length - derivedCount : "—"}</dd></div>
          <div><dt>当前类型操作</dt><dd>{writable ? "可编辑手工资源" : "只读"}</dd></div>
        </dl>
      </InventorySummary>
      <Split asideWidth="minmax(460px, 44%)" main={list} aside={editor || undefined} />
    </>
  );
}
