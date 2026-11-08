import { useEffect, useId, useState } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { RolloutBadge } from "../components/RolloutBadge";
import { ApiError, resources, type Detail, type ResourceView } from "../lib/api";
import { messageFor } from "../lib/errors";
import { errorText } from "../lib/fmt";
import { defaultAt, enumAt, loadSchema, SchemaShapeError, valueDescriptionsAt } from "../lib/schema";
import { useResource } from "../lib/useResource";
import { Failure } from "./Tenants";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";

/** A cache policy document; fields the form does not show are kept as stored. */
interface CachePolicyDoc {
  name?: string;
  scope?: string;
  applies_to?: string;
  applies_to_model_id?: string | null;
  ttl_seconds?: number;
  enabled?: boolean;
  [field: string]: unknown;
}

/** The scope a platform deployment never offers: it shares entries across tenants. */
const ENV_SCOPE = "env";
const ENV_NOTE = "平台部署不提供 env 范围，请改为 team";
/** What a new policy's scope starts at: entries shared within one tenant. */
const PRESELECTED_SCOPE = "team";
/** Refusal details shown beside their own input; the others stay in the list under the form. */
const FIELD_PATHS = ["/scope", "/ttl_seconds"];

interface Options {
  scopes: string[];
  descriptions: Record<string, string>;
  ttlDefault: number | null;
  enabledDefault: boolean;
}

interface Form {
  id: string | null;
  version: number | null;
  /** The stored document, which the save writes back with the form's fields. */
  base: CachePolicyDoc;
  name: string;
  scope: string;
  /** "" for every request, or a model id. */
  target: string;
  /** A stored `applies_to` of another form (an API key): shown, kept, not editable here. */
  otherTarget: string | null;
  ttl: string;
  enabled: boolean;
  /** A stored env policy, which this page does not save. */
  envScoped: boolean;
}

function formOf(view: ResourceView<CachePolicyDoc>): Form {
  const d = view.doc;
  const byId = typeof d.applies_to_model_id === "string" && d.applies_to_model_id !== "";
  const appliesTo = d.applies_to?.trim();
  return {
    id: view.id,
    version: view.version,
    base: d,
    name: d.name ?? "",
    scope: d.scope ?? "",
    target: byId ? (d.applies_to_model_id as string) : "",
    otherTarget: byId || appliesTo === undefined || appliesTo === "all" ? null : (d.applies_to ?? null),
    ttl: d.ttl_seconds === undefined ? "" : String(d.ttl_seconds),
    enabled: d.enabled ?? true,
    envScoped: d.scope === ENV_SCOPE,
  };
}

/** The cache policies, team-scoped by default, with a hand-laid form. */
export function Cache({ canEdit, onGoto }: TabProps) {
  const policies = useResource<CachePolicyDoc>("cache_policies");
  const models = useResource<{ display_name?: string }>("models");
  const [options, setOptions] = useState<Options | null>(null);
  const [optionsError, setOptionsError] = useState<string | null>(null);
  const [form, setForm] = useState<Form | null>(null);
  const [failure, setFailure] = useState<{ text: string; details: Detail[] } | null>(null);
  const [conflict, setConflict] = useState(false);
  const [busy, setBusy] = useState(false);
  // A refused delete, shown on the row it was asked on.
  const [deleteFailure, setDeleteFailure] = useState<{ id: string; error: unknown } | null>(null);
  const scopeProblemId = useId();
  const ttlProblemId = useId();
  const [panel, focusRefusal] = useRefusalFocus<HTMLDivElement>();

  useEffect(() => {
    let live = true;
    loadSchema("cache_policy").then(
      (schema) => {
        if (!live) return;
        try {
          const enabledDefault = defaultAt(schema, "/properties/enabled");
          const ttlDefault = defaultAt(schema, "/properties/ttl_seconds");
          setOptions({
            scopes: enumAt(schema, "/properties/scope").filter((s) => s !== ENV_SCOPE),
            descriptions: valueDescriptionsAt(schema, "/properties/scope"),
            ttlDefault: typeof ttlDefault === "number" ? ttlDefault : null,
            enabledDefault: enabledDefault !== false,
          });
        } catch (e) {
          setOptionsError(e instanceof SchemaShapeError ? e.message : String(e));
        }
      },
      (e) => {
        if (live) setOptionsError(errorText(e));
      },
    );
    return () => {
      live = false;
    };
  }, []);

  const modelName = (id: string) => models.items?.find((m) => m.id === id)?.doc.display_name ?? id;

  function appliesToText(d: CachePolicyDoc): string {
    if (typeof d.applies_to_model_id === "string" && d.applies_to_model_id) {
      return `模型 ${modelName(d.applies_to_model_id)}`;
    }
    const raw = d.applies_to?.trim();
    return raw === undefined || raw === "all" ? "全部请求" : raw;
  }

  function clearOutcome() {
    setFailure(null);
    setConflict(false);
  }

  function startNew() {
    if (!options) return;
    clearOutcome();
    // Models created elsewhere since this page read them belong in 作用于.
    void models.reload();
    setForm({
      id: null,
      version: null,
      base: {},
      name: "",
      scope: options.scopes.includes(PRESELECTED_SCOPE) ? PRESELECTED_SCOPE : (options.scopes[0] ?? ""),
      target: "",
      otherTarget: null,
      ttl: options.ttlDefault === null ? "" : String(options.ttlDefault),
      enabled: options.enabledDefault,
      envScoped: false,
    });
  }

  function open(view: ResourceView<CachePolicyDoc>) {
    clearOutcome();
    setForm(formOf(view));
  }

  function refused(e: unknown) {
    if (e instanceof ApiError && e.code === "version_mismatch") {
      setConflict(true);
    } else if (e instanceof ApiError) {
      // `gateway_too_old` names, at the field, the version the gateways need.
      setFailure({ text: messageFor(e), details: e.details });
    } else {
      setFailure({ text: e instanceof Error ? e.message : String(e), details: [] });
    }
  }

  async function submit() {
    // A cleared TTL is never sent as "no TTL": 保存 waits for one.
    if (!form || form.ttl.trim() === "") return;
    clearOutcome();
    const doc: CachePolicyDoc = {
      ...form.base,
      name: form.name,
      scope: form.scope,
      ttl_seconds: Number(form.ttl),
      enabled: form.enabled,
    };
    if (form.otherTarget === null) {
      delete doc.applies_to;
      delete doc.applies_to_model_id;
      if (form.target) doc.applies_to_model_id = form.target;
    }
    setBusy(true);
    try {
      const view =
        form.id === null || form.version === null
          ? await policies.create(doc)
          : await policies.save(form.id, doc, form.version);
      open(view);
      void policies.watch(view.id).catch(() => undefined);
    } catch (e) {
      refused(e);
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  async function remove(p: ResourceView<CachePolicyDoc>) {
    setBusy(true);
    setDeleteFailure(null);
    try {
      await policies.remove(p.id, p.version);
      if (form?.id === p.id) setForm(null);
    } catch (e) {
      setDeleteFailure({ id: p.id, error: e });
    } finally {
      setBusy(false);
    }
  }

  async function reloadForm() {
    if (!form?.id) return;
    try {
      open(await resources.get<CachePolicyDoc>("cache_policies", form.id));
      void policies.reload();
    } catch (e) {
      refused(e);
    }
  }

  const readOnly = !canEdit || (form?.envScoped ?? false);
  const current = form?.id ? policies.items?.find((p) => p.id === form.id) : undefined;
  const set = (patch: Partial<Form>) => setForm((f) => (f ? { ...f, ...patch } : f));
  const problemAt = (path: string) =>
    failure?.details
      .filter((d) => d.path === path)
      .map((d) => d.message)
      .join("；") || null;
  const scopeProblem = problemAt("/scope");
  const ttlProblem = problemAt("/ttl_seconds");
  const listedDetails = failure?.details.filter((d) => !FIELD_PATHS.includes(d.path)) ?? [];
  const ttlMissing = form !== null && !readOnly && form.ttl.trim() === "";

  return (
    <>
      <div className="panel">
        <h2>
          缓存策略
          <Button
            size="sm"
            type="button"
            icon="refresh"
            onClick={() => {
              void policies.reload();
              void models.reload();
            }}
          >
            刷新
          </Button>
        </h2>
        <p className="hint">
          命中缓存的请求直接由网关应答，不再转发给上游。平台部署中缓存条目只在同一租户的 API 密钥之间共享（team 范围）。
        </p>
        {optionsError && (
          <div className="note crit" role="alert">
            {optionsError}
          </div>
        )}
        {canEdit && (
          <div className="r">
            <Button kind="primary" type="button" disabled={!options} onClick={startNew}>
              新建缓存策略
            </Button>
          </div>
        )}
        {policies.error && (
          <div className="note crit" role="alert">
            {messageFor(policies.error)}
          </div>
        )}
        {policies.items === null && !policies.error && <Loading />}
        {policies.items !== null && policies.items.length === 0 && <EmptyState text="还没有缓存策略。" />}
        {policies.items !== null && policies.items.length > 0 && (
          <DataTable>
            <thead>
              <tr>
                <th>名称</th>
                <th>范围</th>
                <th>作用于</th>
                <th className="right">缓存时长（秒）</th>
                <th>启用</th>
                <th>下发状态</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {policies.items.map((p) => (
                <tr key={p.id}>
                  <td>{p.doc.name}</td>
                  <td>
                    <code>{p.doc.scope ?? "—"}</code>
                  </td>
                  <td>{appliesToText(p.doc)}</td>
                  <td className="right">{p.doc.ttl_seconds ?? "—"}</td>
                  <td>
                    <Lamp state={p.doc.enabled === false ? "disabled" : "applied"}>{p.doc.enabled === false ? "否" : "是"}</Lamp>
                  </td>
                  <td>
                    <RolloutBadge rollout={p.rollout} />
                  </td>
                  <td className="right">
                    <Button size="sm" type="button" onClick={() => open(p)}>
                      {canEdit && p.doc.scope !== ENV_SCOPE ? "编辑" : "查看"}
                    </Button>
                    {canEdit && (
                      <TwoStep
                        label="删除"
                        size="sm"
                        disabled={busy}
                        warning={`删除「${p.doc.name ?? p.id}」后，它覆盖的请求不再由网关缓存应答。`}
                        onConfirm={() => void remove(p)}
                      />
                    )}
                    {deleteFailure?.id === p.id && (
                      <Failure
                        error={deleteFailure.error}
                        onReload={() => {
                          setDeleteFailure(null);
                          void policies.reload();
                        }}
                      />
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </DataTable>
        )}
      </div>

      {form && (
        <div ref={panel} className="panel">
          <h2>
            {form.id === null ? "新建缓存策略" : `缓存策略 ${form.base.name ?? ""}`}
            {current && <RolloutBadge rollout={current.rollout} />}
          </h2>
          {form.envScoped && (
            <div className="note warn">
              {ENV_NOTE}{" "}
              {canEdit && (
                <Button type="button" onClick={() => onGoto("resources")}>
                  到「全部资源」修改
                </Button>
              )}
            </div>
          )}
          <label className="f">
            <span>名称</span>
            <input value={form.name} disabled={readOnly} onChange={(e) => set({ name: e.target.value })} />
          </label>
          <label className="f">
            <span>范围</span>
            {form.envScoped ? (
              <input value={ENV_SCOPE} disabled />
            ) : (
              <select
                value={form.scope}
                disabled={readOnly}
                aria-invalid={scopeProblem ? true : undefined}
                aria-describedby={scopeProblem ? scopeProblemId : undefined}
                onChange={(e) => set({ scope: e.target.value })}
              >
                {(options?.scopes ?? []).map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </select>
            )}
          </label>
          {scopeProblem && (
            <p id={scopeProblemId} className="field-problem">
              {scopeProblem}
            </p>
          )}
          {options?.descriptions[form.scope] && <p className="hint">{options.descriptions[form.scope]}</p>}
          <label className="f">
            <span>作用于</span>
            {form.otherTarget !== null ? (
              <input value={form.otherTarget} disabled />
            ) : (
              <select value={form.target} disabled={readOnly} onChange={(e) => set({ target: e.target.value })}>
                <option value="">全部请求</option>
                {form.target && !models.items?.some((m) => m.id === form.target) && (
                  <option value={form.target}>{form.target}</option>
                )}
                {(models.items ?? []).map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.doc.display_name ?? m.id}
                  </option>
                ))}
              </select>
            )}
          </label>
          {form.otherTarget !== null && <p className="hint">这种写法的作用范围只能在「全部资源」页修改，保存时原样保留。</p>}
          <label className="f">
            <span>缓存时长（秒）</span>
            <input
              type="number"
              min={1}
              step={1}
              required
              value={form.ttl}
              disabled={readOnly}
              aria-invalid={ttlMissing || ttlProblem ? true : undefined}
              aria-describedby={ttlMissing || ttlProblem ? ttlProblemId : undefined}
              onChange={(e) => set({ ttl: e.target.value })}
            />
          </label>
          {(ttlMissing || ttlProblem) && (
            <p id={ttlProblemId} className="field-problem">
              {ttlMissing ? "必填" : ttlProblem}
            </p>
          )}
          <label className="f">
            <span>启用</span>
            <input
              type="checkbox"
              checked={form.enabled}
              disabled={readOnly}
              onChange={(e) => set({ enabled: e.target.checked })}
            />
          </label>
          {conflict && <ConflictNote onReload={() => void reloadForm()} />}
          {failure && (
            <div className="note crit" role="alert">
              {failure.text}
              {listedDetails.length > 0 && (
                <ul className="json-problems">
                  {listedDetails.map((d, i) => (
                    <li key={i} data-path={d.path}>
                      <mark className="json-path">{d.path || "（文档根）"}</mark> {d.message}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}
          <div className="r">
            {!readOnly && (
              <Button kind="primary" type="button" disabled={busy || ttlMissing} onClick={() => void submit()}>
                保存
              </Button>
            )}
            <Button type="button" onClick={() => setForm(null)}>
              {readOnly ? "关闭" : "取消"}
            </Button>
          </div>
        </div>
      )}
    </>
  );
}
