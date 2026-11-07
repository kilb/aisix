import { useState } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { JsonEditor } from "../components/JsonEditor";
import { RolloutBadge } from "../components/RolloutBadge";
import { ApiError, resources, type Detail, type ResourceView } from "../lib/api";
import { messageFor } from "../lib/errors";
import { useResource } from "../lib/useResource";

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
export function Resources({ canEdit }: TabProps) {
  const [kind, setKind] = useState<string>(KINDS[0].kind);
  const info = KINDS.find((k) => k.kind === kind) ?? KINDS[0];
  return (
    <>
      <div className="panel">
        <h2>全部资源</h2>
        <p className="hint">
          按网关的资源定义直接编辑 JSON 文档。保存时由控制面按网关的规则校验，问题列在编辑框下方。
        </p>
        <label className="f">
          <span>资源类型</span>
          <select value={kind} onChange={(e) => setKind(e.target.value)}>
            {KINDS.map((k) => (
              <option key={k.kind} value={k.kind}>
                {k.label} · {k.kind}
              </option>
            ))}
          </select>
        </label>
      </div>
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
    if (doc === null) return;
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

  return (
    <>
      <div className="panel">
        <h2>
          {info.label} <code className="lab">{info.kind}</code>
        </h2>
        {!info.writable && <div className="note">{MANAGED_NOTE}</div>}
        <div className="r">
          {canCreate && (
            <button type="button" className="act" onClick={startNew}>
              新建
            </button>
          )}
          <button type="button" className="ghost" onClick={() => void reload()}>
            刷新
          </button>
        </div>
        {error && (
          <div className="note crit" role="alert">
            {messageFor(error)}
          </div>
        )}
        {items === null && !error && <p className="hint">载入中…</p>}
        {items !== null && items.length === 0 && <p className="hint">暂无此类资源。</p>}
        {items !== null && items.length > 0 && (
          <div className="scroll">
            <table>
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
                  <tr key={r.id}>
                    <td>{nameOf(r.doc) ?? <span className="hint">（未命名）</span>}</td>
                    <td>
                      <code>{r.id}</code>
                    </td>
                    <td className="right">{r.version}</td>
                    <td>{r.managed_by ?? "—"}</td>
                    <td>
                      <RolloutBadge rollout={r.rollout} />
                    </td>
                    <td className="right">
                      <button type="button" className="ghost" onClick={() => open(r)}>
                        {writable && r.managed_by === null ? "编辑" : "查看"}
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {editing && (
        <div className="panel">
          <h2>
            {editing.mode === "new" ? `新建${info.label}` : (current && nameOf(current.doc)) || editing.id}
            {current && <RolloutBadge rollout={current.rollout} />}
          </h2>
          {editing.mode === "edit" && derived && <div className="note">{MANAGED_NOTE}</div>}
          {editing.mode === "edit" && !derived && !canEdit && <p className="hint">当前账号只读。</p>}
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
          <div className="r">
            {editing.mode === "new" && (
              <button type="button" className="act" disabled={busy} onClick={() => void submit()}>
                创建
              </button>
            )}
            {editing.mode === "edit" && editable && !confirmDelete && (
              <>
                <button type="button" className="act" disabled={busy} onClick={() => void submit()}>
                  保存
                </button>
                <button type="button" className="ghost" disabled={busy} onClick={() => setConfirmDelete(true)}>
                  删除
                </button>
              </>
            )}
            {/* Ruling P1b-R6: a destructive action confirms inline, never in a dialog. */}
            {editing.mode === "edit" && editable && confirmDelete && (
              <>
                <button type="button" className="act" disabled={busy} onClick={() => void del()}>
                  确认删除
                </button>
                <button type="button" className="ghost" disabled={busy} onClick={() => setConfirmDelete(false)}>
                  取消
                </button>
              </>
            )}
            {!confirmDelete && (
              <button type="button" className="ghost" onClick={close}>
                {editing.mode === "new" || editable ? "取消" : "关闭"}
              </button>
            )}
          </div>
        </div>
      )}
    </>
  );
}
