import "./operations.css";
import "./Cache.css";
import { useRecordTable } from "../lib/recordTable";
import { RecordTools, RecordPager } from "../components/RecordTools";
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
import { FormDialog } from "../ui/FormDialog";
import { useRefusalFocus } from "../ui/Field";
import { OperationalTable as DataTable, Loading } from "../components/OperationalTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";
import { Card, CardHeader, PageHeader, Stat, StatGrid } from "../components/page";
import { Plus, RefreshCw } from "lucide-react";
import { Select } from "../ui/Select";

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
  const [enabledFilter, setEnabledFilter] = useState("");
  const [scopeFilter, setScopeFilter] = useState("");
  const table = useRecordTable({ items: (policies.items ?? []).filter((p) => (!enabledFilter || ((p.doc.enabled ?? true) ? "enabled" : "disabled") === enabledFilter) && (!scopeFilter || p.doc.scope === scopeFilter)), searchText: (p) => [p.id, p.doc.name ?? "", p.doc.scope ?? "", p.doc.applies_to_model_id ?? "", p.doc.applies_to ?? ""], resetKey: `${enabledFilter}:${scopeFilter}` });
  const models = useResource<{ display_name?: string }>("models");
  const [options, setOptions] = useState<Options | null>(null);
  const [optionsError, setOptionsError] = useState<string | null>(null);
  const [form, setForm] = useState<Form | null>(null);
  const [failure, setFailure] = useState<{ text: string; details: Detail[] } | null>(null);
  const [conflict, setConflict] = useState(false);
  const [busy, setBusy] = useState(false);
  // The policy whose delete is in flight.
  const [removing, setRemoving] = useState<string | null>(null);
  // A refused delete, shown on the row it was asked on.
  const [deleteFailure, setDeleteFailure] = useState<{ id: string; error: unknown } | null>(null);
  const scopeProblemId = useId();
  const ttlProblemId = useId();
  const [panel, focusRefusal] = useRefusalFocus<HTMLElement>();

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
    setRemoving(p.id);
    setDeleteFailure(null);
    try {
      await policies.remove(p.id, p.version);
      if (form?.id === p.id) setForm(null);
    } catch (e) {
      setDeleteFailure({ id: p.id, error: e });
    } finally {
      setBusy(false);
      setRemoving(null);
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

  const list = (
    <Card className="cache-list" aria-label="全部缓存策略">
      <CardHeader
        description="命中缓存的请求直接由网关应答，不再转发给上游。平台部署中缓存条目只在同一租户的 API 密钥之间共享（team 范围）。"
        as="h2"
        title={
          <span className="inline-flex items-center gap-2">
            全部策略
            {policies.items && policies.items.length > 0 && <span className="tag">{policies.items.length}</span>}
          </span>
        }
      />
      {policies.error && (
        <div className="note crit" role="alert">
          {messageFor(policies.error)}
        </div>
      )}
      {policies.items === null && !policies.error && <Loading />}
      {policies.items !== null && policies.items.length === 0 && <EmptyState text="还没有缓存策略。" />}
      {policies.items !== null && policies.items.length > 0 && (
        <>
          <RecordTools table={table} label="缓存策略"><label className="f filter"><span>启用状态</span><Select size="compact" aria-label="启用状态" value={enabledFilter} onValueChange={(value) => setEnabledFilter(value)}><option value="">全部</option><option value="enabled">启用</option><option value="disabled">停用</option></Select></label><label className="f filter"><span>作用范围</span><Select size="compact" aria-label="作用范围" value={scopeFilter} onValueChange={(value) => setScopeFilter(value)}><option value="">全部</option><option value="team">租户</option><option value="env">环境</option></Select></label></RecordTools>
          <DataTable label="缓存策略">
            <thead>
              <tr>
                <th>名称</th>
                <th>范围</th>
                <th>作用于</th>
                <th className="right">缓存时长（秒）</th>
                <th>启用</th>
                <th>下发状态</th>
                <th><span className="sr-only">操作</span></th>
              </tr>
            </thead>
            <tbody>
              {table.rows.map((p) => (
                <tr key={p.id} className={form?.id === p.id ? "bg-sunken" : undefined}>
                  <td className="font-medium">{p.doc.name}</td>
                  <td>
                    <code className="tag font-mono">{p.doc.scope ?? "—"}</code>
                  </td>
                  <td>{appliesToText(p.doc)}</td>
                  <td className="right num">{p.doc.ttl_seconds ?? "—"}</td>
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
                        busy={removing === p.id}
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
          <RecordPager table={table} label="缓存策略" />
        </>
      )}
    </Card>
  );

  const editor = form && (
    <section ref={panel} className="panel cache-editor" data-tone="surface" aria-label="缓存策略设置">
      <CardHeader
        as="h2"
        title={
          <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
            {form.id === null ? "新建缓存策略" : `缓存策略 ${form.base.name ?? ""}`}
            {current && <RolloutBadge rollout={current.rollout} />}
          </span>
        }
      />
      {form.envScoped && (
        <div className="note warn mt-0">
          {ENV_NOTE}{" "}
          {canEdit && (
            <Button type="button" size="sm" onClick={() => onGoto("resources")}>
              到「全部资源」修改
            </Button>
          )}
        </div>
      )}
      <section className="cache-settings" aria-labelledby="cache-settings-title">
        <h3 id="cache-settings-title" className="text-title text-fg">策略设置</h3>
        <div className="cache-fields">
          <label className="f">
            <span>名称</span>
            <input value={form.name} disabled={readOnly} onChange={(e) => set({ name: e.target.value })} />
          </label>
          <div className="min-w-0">
            <label className="f">
              <span>范围</span>
              {form.envScoped ? (
                <input value={ENV_SCOPE} disabled />
              ) : (
                <Select
                  value={form.scope}
                  disabled={readOnly}
                  aria-invalid={scopeProblem ? true : undefined}
                  aria-describedby={scopeProblem ? scopeProblemId : undefined}
                  onValueChange={(value) => set({ scope: value })}
                >
                  {(options?.scopes ?? []).map((s) => (
                    <option key={s} value={s}>
                      {s}
                    </option>
                  ))}
                </Select>
              )}
            </label>
            {scopeProblem && (
              <p id={scopeProblemId} className="field-problem">
                {scopeProblem}
              </p>
            )}
            {options?.descriptions[form.scope] && (
              <p className="hint -mt-2 mb-4 text-caption">{options.descriptions[form.scope]}</p>
            )}
          </div>
          <div className="min-w-0">
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
          </div>
          <div className="min-w-0">
            <label className="f">
              <span>作用于</span>
              {form.otherTarget !== null ? (
                <input value={form.otherTarget} disabled />
              ) : (
                <Select value={form.target} disabled={readOnly} onValueChange={(value) => set({ target: value })}>
                  <option value="">全部请求</option>
                  {form.target && !models.items?.some((m) => m.id === form.target) && (
                    <option value={form.target}>{form.target}</option>
                  )}
                  {(models.items ?? []).map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.doc.display_name ?? m.id}
                    </option>
                  ))}
                </Select>
              )}
            </label>
            {form.otherTarget !== null && (
              <p className="hint -mt-2 mb-4 text-caption">这种写法的作用范围只能在「全部资源」页修改，保存时原样保留。</p>
            )}
          </div>
          <label className="f">
            <input
              type="checkbox"
              checked={form.enabled}
              disabled={readOnly}
              onChange={(e) => set({ enabled: e.target.checked })}
            />
            <span>启用</span>
          </label>
        </div>
      </section>
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
      <div className="cache-editor-footer">
        <Button type="button" onClick={() => setForm(null)}>
          {readOnly ? "关闭" : "取消"}
        </Button>
        {!readOnly && (
          <Button
            kind="primary"
            type="button"
            disabled={busy || ttlMissing}
            busy={busy && removing === null}
            busyLabel="保存"
            onClick={() => void submit()}
          >
            保存
          </Button>
        )}
      </div>
    </section>
  );

  return (
    <div className="cp-operations cp-cache">
      <PageHeader
        title={form?.id != null ? "编辑缓存策略" : "缓存策略"}
        dataLead={form?.id != null ? "按作用范围、缓存时长与模型调整策略，保存后查看网关下发状态。" : policies.items ? <>已配置 {policies.items.length} 条缓存策略，其中 {policies.items.filter((p) => p.doc.enabled !== false).length} 条启用。命中后由网关直接应答。</> : "按租户范围与模型配置应答缓存。"}
        actions={
          <>
            <Button
              type="button"
              onClick={() => {
                void policies.reload();
                void models.reload();
              }}
            >
              <RefreshCw aria-hidden />
              刷新
            </Button>
            {form?.id != null && <Button type="button" onClick={() => setForm(null)}>返回策略列表</Button>}
            {canEdit && form?.id == null && (
              <Button kind="primary" type="button" disabled={!options} onClick={startNew}>
                <Plus aria-hidden />
                新建缓存策略
              </Button>
            )}
          </>
        }
      />
      {form?.id == null && (
        <StatGrid className="cache-summary" aria-label="缓存策略概况">
          <Stat label="已保存策略" value={policies.items?.length ?? "—"} foot="命中后由网关直接应答" />
          <Stat label="已启用" value={policies.items ? policies.items.filter((p) => p.doc.enabled !== false).length : "—"} foot="各网关是否已应用请查看下发状态" />
          <Stat label="已停用" value={policies.items ? policies.items.filter((p) => p.doc.enabled === false).length : "—"} foot="停用策略不参与缓存应答" />
          <Stat label="env 范围" value={policies.items ? policies.items.filter((p) => p.doc.scope === ENV_SCOPE).length : "—"} foot="此表单只读；平台部署使用 team 范围" />
        </StatGrid>
      )}
      {optionsError && (
        <div className="note crit" role="alert">
          {optionsError}
        </div>
      )}
      {form?.id != null ? editor : list}
      {form?.id === null && <FormDialog open onOpenChange={(next) => { if (!next) setForm(null); }} busy={busy} title="新建缓存策略" description="按作用范围与模型设置缓存；校验失败保留输入。">{editor}</FormDialog>}
    </div>
  );
}
