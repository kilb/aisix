import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState, type ReactNode } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { GatewayTooOld } from "../components/GatewayTooOld";
import { Msg, type Note } from "../components/Msg";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  ApiError,
  keys as keysApi,
  providerGroups,
  resources,
  settings,
  tenants,
  type AdminKey,
  type Invite,
  type Member,
  type ProviderGroup,
  type ResourceView,
  type Tenant,
  type TenantChanges,
  type TenantDefaults,
  type TenantLimits,
} from "../lib/api";
import { messageFor } from "../lib/errors";
import "./Tenants.css";

export const STATUS_LABEL: Record<Tenant["status"], string> = {
  pending: "待审核",
  active: "正常",
  suspended: "已停用",
};

const BILLING_LABEL: Record<Tenant["billing_mode"], string> = {
  prepaid: "预付费",
  postpaid: "后付费",
};

export const ROLE_LABEL: Record<Member["role"], string> = {
  owner: "所有者",
  admin: "管理员",
  member: "成员",
  billing: "计费",
};

const ROLES: Member["role"][] = ["owner", "admin", "member", "billing"];

const LIMIT_FIELDS: { field: keyof TenantLimits; label: string }[] = [
  { field: "concurrency", label: "租户并发" },
  { field: "rpm", label: "租户每分钟请求" },
  { field: "tpm", label: "租户每分钟 token" },
  { field: "key_concurrency_max", label: "单 key 并发上限" },
  { field: "key_rpm_max", label: "单 key 每分钟请求上限" },
];

/** Shown beside a model the control plane refused to grant because of its kind. */
const UNGRANTABLE = "通配模型与 ensemble 模型不能授给租户";

const WATCH_INTERVAL_MS = 1_000;
const WATCH_BUDGET_MS = 30_000;

/** The fields of a model document this page reads. */
interface ModelDoc {
  display_name?: string;
  ensemble?: unknown;
}

/** A model a customer tenant cannot be granted: a wildcard row or an ensemble. */
function ungrantable(doc: ModelDoc): boolean {
  return (doc.display_name ?? "").includes("*") || (doc.ensemble !== undefined && doc.ensemble !== null);
}

export function errorText(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/**
 * Why an action was refused, where it was taken: a `gateway_too_old` refusal
 * with what it needs and the gateways that hold it back, a `412` with a
 * reload when `onReload` is given, anything else as its text.
 */
export function Failure({ error, onReload }: { error: unknown; onReload?: () => void }) {
  if (error instanceof ApiError && error.code === "gateway_too_old") return <GatewayTooOld error={error} />;
  if (error instanceof ApiError && error.code === "version_mismatch" && onReload) {
    return <ConflictNote onReload={onReload} />;
  }
  return (
    <span className="tn-error" role="alert">
      {errorText(error)}
    </span>
  );
}

/** A refused write, sorted for the form that made it. */
export interface Refusal {
  /** `412`: the row changed since this edit started. */
  conflict: boolean;
  /** Validation problems by the JSON path of the request body they are at. */
  at: Map<string, string>;
  /** What no field of the form shows. */
  general: string | null;
  /** A `422 gateway_too_old`, which a form may show with the gateways that hold it back. */
  tooOld: ApiError | null;
}

/**
 * `e` sorted for a form: a `412` is a conflict; each detail of a `422` the
 * form has a place for (`placed(path)`) goes to its field and the rest to the
 * general note; a `gateway_too_old` is the general note with its details; any
 * other refusal is the general note.
 */
export function refusal(e: unknown, placed: (path: string) => boolean): Refusal {
  const at = new Map<string, string>();
  if (!(e instanceof ApiError)) return { conflict: false, at, general: String(e), tooOld: null };
  if (e.code === "version_mismatch") return { conflict: true, at, general: null, tooOld: null };
  if (e.code === "gateway_too_old") {
    return { conflict: false, at, general: [messageFor(e), ...e.details.map((d) => d.message)].join("；"), tooOld: e };
  }
  if (e.code !== "validation_failed" || e.details.length === 0) {
    return { conflict: false, at, general: messageFor(e), tooOld: null };
  }
  const rest: string[] = [];
  for (const d of e.details) {
    if (!placed(d.path)) rest.push(d.message);
    else at.set(d.path, at.has(d.path) ? `${at.get(d.path)}；${d.message}` : d.message);
  }
  return { conflict: false, at, general: rest.length > 0 ? rest.join("；") : null, tooOld: null };
}

/**
 * A destructive action in two steps (ruling P1b-R6): the first click turns
 * the button into `确认<label>` and `取消`, under `warning`, and only the
 * confirmation acts. `ask` decides, at each first click, whether to ask at all.
 */
export function TwoStep({
  label,
  onConfirm,
  disabled,
  className = "ghost",
  ask = () => true,
  warning,
}: {
  label: string;
  onConfirm: () => void;
  disabled?: boolean;
  className?: string;
  ask?: () => boolean;
  warning?: ReactNode;
}) {
  const [asking, setAsking] = useState(false);
  if (!asking) {
    return (
      <button
        type="button"
        className={className}
        disabled={disabled}
        onClick={() => (ask() ? setAsking(true) : onConfirm())}
      >
        {label}
      </button>
    );
  }
  return (
    <>
      {warning && (
        <div className="note warn" role="alert">
          {warning}
        </div>
      )}
      <span className="tn-actions">
        <button
          type="button"
          className={className}
          disabled={disabled}
          onClick={() => {
            setAsking(false);
            onConfirm();
          }}
        >
          确认{label}
        </button>
        <button type="button" className="ghost" onClick={() => setAsking(false)}>
          取消
        </button>
      </span>
    </>
  );
}

/** A path at the list `field` or one of its entries. */
export const atList = (field: string) => (path: string) => path === field || path.startsWith(`${field}/`);

function pause(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    signal.addEventListener(
      "abort",
      () => {
        clearTimeout(timer);
        resolve();
      },
      { once: true },
    );
  });
}

const settling = (t: Tenant) => t.rollout.state === "pending" || t.rollout.state === "propagating";

/**
 * Follows tenant `t`'s rollout: re-reads it every second, for up to 30 s,
 * while its documents are on their way to the gateways, handing each view to
 * `onView`. A failed read is skipped; `signal` stops it.
 */
export async function follow(t: Tenant, onView: (t: Tenant) => void, signal: AbortSignal): Promise<void> {
  const deadline = Date.now() + WATCH_BUDGET_MS;
  let last = t;
  while (settling(last) && !signal.aborted && Date.now() + WATCH_INTERVAL_MS <= deadline) {
    await pause(WATCH_INTERVAL_MS, signal);
    if (signal.aborted) return;
    try {
      last = await tenants.get(t.id, signal);
      onView(last);
    } catch (e) {
      // An abort ends the watch; a refused read is tried again next round.
      if (!(e instanceof ApiError)) return;
    }
  }
}

/** A signal that aborts when the component unmounts. */
export function useLifetime(): () => AbortSignal {
  const ref = useRef<AbortController | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    ref.current = controller;
    return () => controller.abort();
  }, []);
  return useCallback(() => ref.current?.signal ?? AbortSignal.abort(), []);
}

/**
 * The result of `load`, read again whenever `deps` change; `data` is null
 * until the first read. A read that a later one overtook is dropped.
 */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[]) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const seq = useRef(0);
  const reload = useCallback(async () => {
    const mine = ++seq.current;
    try {
      const value = await load();
      if (mine !== seq.current) return;
      setData(value);
      setError(null);
    } catch (e) {
      if (mine === seq.current) setError(errorText(e));
    }
    // `load` is a new closure every render; `deps` say when it reads something else.
  }, deps);
  useEffect(() => {
    void reload();
  }, [reload]);
  return { data, error, reload, setData };
}

/**
 * Clears a part's `412` note once the tenant has moved to another version (a
 * reload). The part's draft and any `422` stay: only its own stored value
 * changing resets them, so one part's save never discards another's edit.
 */
function useConflictClears(version: number, setRefused: (update: (r: Refusal | null) => Refusal | null) => void) {
  useEffect(() => {
    setRefused((r) => (r?.conflict ? null : r));
  }, [version, setRefused]);
}

type LimitDraft = Record<keyof TenantLimits, string>;

function limitDraft(limits: TenantLimits): LimitDraft {
  const draft = {} as LimitDraft;
  for (const { field } of LIMIT_FIELDS) draft[field] = limits[field] === undefined ? "" : String(limits[field]);
  return draft;
}

/** The limits a draft sets; a blank field is unset (unbounded). */
function limitsFrom(draft: LimitDraft): TenantLimits {
  const limits: TenantLimits = {};
  for (const { field } of LIMIT_FIELDS) {
    const raw = draft[field].trim();
    if (raw !== "") limits[field] = Number(raw);
  }
  return limits;
}

/** Every group name worth offering: the accounts' groups and those already chosen, sorted. */
export function groupOptions(available: ProviderGroup[] | null, stored: string[]): string[] {
  return [...new Set([...(available ?? []).map((g) => g.name), ...stored])].sort((a, b) => a.localeCompare(b));
}

/** The upstream accounts in each group, by display name. */
export function accountsByGroup(available: ProviderGroup[] | null): Map<string, string[]> {
  return new Map((available ?? []).map((g) => [g.name, g.provider_keys.map((k) => k.display_name ?? k.id)]));
}

/**
 * The groups key `k`'s override names that tenant `t` does not have. A tenant
 * with no groups reaches every upstream, so nothing is outside it.
 */
export function strayGroups(k: AdminKey, t: Tenant): string[] {
  if (!k.allowed_provider_groups || t.allowed_provider_groups.length === 0) return [];
  return k.allowed_provider_groups.filter((g) => !t.allowed_provider_groups.includes(g));
}

/** The note beside a key whose override reaches past its tenant's groups. */
export function StrayNote({ k, tenant }: { k: AdminKey; tenant: Tenant }) {
  const stray = strayGroups(k, tenant);
  if (stray.length === 0) return null;
  return <span className="tn-note tn-flag">覆盖中的 {stray.join("、")} 不在租户的可用分组中</span>;
}

/** The warning before a group choice whose groups hold no upstream account is saved. */
export function unreachableWarning(groups: string[], whose: string): string {
  return `${groups.join("、")}：没有上游账号在此分组。保存后，${whose}找不到任何上游账号，无法调用模型。`;
}

export function Tenants({ operator, canEdit }: TabProps) {
  const [filter, setFilter] = useState<Tenant["status"] | "">("");
  const [items, setItems] = useState<Tenant[] | null>(null);
  const [listError, setListError] = useState<string | null>(null);
  const [rowError, setRowError] = useState<{ id: string; error: unknown } | null>(null);
  const [acting, setActing] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState<Tenant | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [showDefaults, setShowDefaults] = useState(false);
  const lifetime = useLifetime();
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  const listSeq = useRef(0);

  const loadList = useCallback(async () => {
    // A read for an earlier filter that answers late must not replace this one.
    const mine = ++listSeq.current;
    try {
      const list = await tenants.list(filter || undefined);
      if (mine !== listSeq.current) return;
      setItems(list);
      setListError(null);
    } catch (e) {
      if (mine === listSeq.current) setListError(errorText(e));
    }
  }, [filter]);

  useEffect(() => {
    void loadList();
  }, [loadList]);

  // A view replaces the one shown unless it is older: a read that raced a write.
  const applyView = useCallback((view: Tenant) => {
    const newer = (old: Tenant) => (old.id === view.id && view.version >= old.version ? view : old);
    setItems((prev) => prev && prev.map(newer));
    setDetail((prev) => prev && newer(prev));
  }, []);

  /** Shows `view`, then follows its rollout until it settles. */
  const settle = useCallback(
    (view: Tenant) => {
      applyView(view);
      void follow(view, applyView, lifetime());
    },
    [applyView, lifetime],
  );

  const loadDetail = useCallback(async (id: string) => {
    setDetailError(null);
    try {
      const view = await tenants.get(id);
      setDetail((current) => (selectedRef.current === id ? view : current));
    } catch (e) {
      if (selectedRef.current === id) setDetailError(errorText(e));
    }
  }, []);

  useEffect(() => {
    setDetail(null);
    if (selected) void loadDetail(selected);
  }, [selected, loadDetail]);

  /** Reads the tenant again after a change that moved its documents but not its row. */
  const refresh = useCallback(
    async (id: string) => {
      try {
        settle(await tenants.get(id));
      } catch (e) {
        // The view shown stays; the next read corrects it.
        if (!(e instanceof ApiError)) throw e;
      }
    },
    [settle],
  );

  async function act(t: Tenant, run: (id: string) => Promise<Tenant>) {
    setActing(t.id);
    setRowError(null);
    try {
      settle(await run(t.id));
      // The filter may leave the tenant out now.
      await loadList();
    } catch (e) {
      setRowError({ id: t.id, error: e });
    } finally {
      setActing(null);
    }
  }

  return (
    <>
      <div className="panel">
        <h2>
          租户
          {operator.role === "admin" && (
            <button
              type="button"
              className="ghost"
              style={{ marginLeft: "auto" }}
              aria-expanded={showDefaults}
              onClick={() => setShowDefaults((open) => !open)}
            >
              新租户默认值
            </button>
          )}
        </h2>
        <p className="hint">
          客户经租户门户自助注册后在这里审核；批准前它不能自行创建密钥，名下的密钥也都不可用。停用租户会让它的全部密钥立即失效，恢复后重新可用。
        </p>
        <label className="f" style={{ maxWidth: 220 }}>
          <span>按状态筛选</span>
          <select value={filter} onChange={(e) => setFilter(e.target.value as Tenant["status"] | "")}>
            <option value="">全部</option>
            <option value="pending">待审核</option>
            <option value="active">正常</option>
            <option value="suspended">已停用</option>
          </select>
        </label>
        {listError && (
          <div className="note crit" role="alert">
            {listError}{" "}
            <button type="button" className="ghost" onClick={() => void loadList()}>
              重试
            </button>
          </div>
        )}
        {items === null ? (
          !listError && <p className="hint">载入中…</p>
        ) : items.length === 0 ? (
          <p className="hint">没有符合条件的租户。</p>
        ) : (
          <div className="scroll">
            <table aria-label="租户列表">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>状态</th>
                  <th>计费方式</th>
                  <th className="right">成员</th>
                  <th className="right">密钥</th>
                  <th>下发</th>
                  {canEdit && <th>操作</th>}
                </tr>
              </thead>
              <tbody>
                {items.map((t) => (
                  <tr key={t.id}>
                    <td>
                      <button type="button" className="tn-name" onClick={() => setSelected(t.id)}>
                        {t.name}
                      </button>
                      {t.is_internal && <span className="pill value tn-mark">平台内部</span>}
                    </td>
                    <td>{STATUS_LABEL[t.status] ?? t.status}</td>
                    <td>{BILLING_LABEL[t.billing_mode] ?? t.billing_mode}</td>
                    <td className="right">{t.member_count}</td>
                    <td className="right">{t.key_count}</td>
                    <td>
                      <RolloutBadge rollout={t.rollout} />
                    </td>
                    {canEdit && (
                      <td>
                        <div className="tn-actions">
                          {t.status === "pending" && (
                            <button
                              type="button"
                              className="ghost"
                              disabled={acting === t.id}
                              onClick={() => void act(t, tenants.approve)}
                            >
                              批准
                            </button>
                          )}
                          {/* The platform's own tenant holds the operators' keys; it is never suspended. */}
                          {t.status === "active" && !t.is_internal && (
                            <TwoStep
                              label="停用"
                              disabled={acting === t.id}
                              onConfirm={() => void act(t, tenants.suspend)}
                            />
                          )}
                          {t.status === "suspended" && (
                            <button
                              type="button"
                              className="ghost"
                              disabled={acting === t.id}
                              onClick={() => void act(t, tenants.reactivate)}
                            >
                              恢复
                            </button>
                          )}
                        </div>
                        {rowError?.id === t.id && <Failure error={rowError.error} />}
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {showDefaults && operator.role === "admin" && <DefaultsPanel />}

      {selected && detailError && (
        <div className="note crit" role="alert">
          {detailError}{" "}
          <button type="button" className="ghost" onClick={() => void loadDetail(selected)}>
            重试
          </button>
        </div>
      )}
      {detail && (
        <TenantDetail
          key={detail.id}
          tenant={detail}
          canEdit={canEdit}
          onView={settle}
          onRefresh={() => void refresh(detail.id)}
          onReload={() => void loadDetail(detail.id)}
          onClose={() => setSelected(null)}
        />
      )}
    </>
  );
}

/** What each part of the tenant's page is given. */
interface PartProps {
  tenant: Tenant;
  canEdit: boolean;
  /** Saves `changes` against the version shown; throws the refusal. */
  save: (changes: TenantChanges) => Promise<Tenant>;
  /** Reads the tenant again, after a `412`. */
  onReload: () => void;
}

function TenantDetail({
  tenant,
  canEdit,
  onView,
  onRefresh,
  onReload,
  onClose,
}: {
  tenant: Tenant;
  canEdit: boolean;
  onView: (view: Tenant) => void;
  onRefresh: () => void;
  onReload: () => void;
  onClose: () => void;
}) {
  const catalog = useLoad(() => resources.list<ModelDoc>("models"), []);
  const groups = useLoad(() => providerGroups.list(), []);
  const members = useLoad(() => tenants.members(tenant.id), [tenant.id]);
  // Every change to the tenant's row can change what its keys are given.
  const keyList = useLoad(() => tenants.keys(tenant.id), [tenant.id, tenant.version]);

  const save = useCallback(
    async (changes: TenantChanges) => {
      const view = await tenants.update(tenant.id, changes, tenant.version);
      onView(view);
      return view;
    },
    [tenant.id, tenant.version, onView],
  );

  const options = useMemo(
    () => groupOptions(groups.data, tenant.allowed_provider_groups),
    [groups.data, tenant.allowed_provider_groups],
  );
  const accounts = useMemo(() => accountsByGroup(groups.data), [groups.data]);
  const part = { tenant, canEdit, save, onReload };

  return (
    <section className="panel" aria-label={`租户详情：${tenant.name}`}>
      <h2>
        {tenant.name}
        <RolloutBadge rollout={tenant.rollout} />
        <button type="button" className="ghost" onClick={onClose}>
          关闭
        </button>
      </h2>
      <p className="hint">
        {STATUS_LABEL[tenant.status] ?? tenant.status} · {BILLING_LABEL[tenant.billing_mode] ?? tenant.billing_mode} ·
        成员 {tenant.member_count} · 密钥 {tenant.key_count}
        {tenant.is_internal && " · 平台内部租户：它的密钥是运维密钥，没有成员"}
      </p>

      <GrantsPart {...part} catalog={catalog.data} catalogError={catalog.error} />
      <GroupsPart {...part} options={options} accounts={accounts} groupsError={groups.error} />
      <LimitsPart {...part} />

      {!tenant.is_internal && (
        <MembersPart
          members={members.data}
          error={members.error}
          canEdit={canEdit}
          onChanged={(m) => {
            members.setData((list) => list && list.map((x) => (x.id === m.id ? m : x)));
            void keyList.reload();
            onRefresh();
          }}
        />
      )}
      {!tenant.is_internal && canEdit && <InvitePart tenant={tenant} />}

      <KeysPart
        tenant={tenant}
        keys={keyList.data}
        error={keyList.error}
        members={members.data}
        options={options}
        accounts={accounts}
        canEdit={canEdit}
        onSaved={(k) => {
          keyList.setData((list) => list && list.map((x) => (x.id === k.id ? k : x)));
          onRefresh();
        }}
        onReload={() => void keyList.reload()}
      />
    </section>
  );
}

/** One choice in a list of checkboxes, with a note beside it and its refusal under it. */
export function Check({
  label,
  checked,
  disabled,
  onChange,
  note,
  error,
}: {
  label: string;
  checked: boolean;
  disabled: boolean;
  onChange: (on: boolean) => void;
  note?: string;
  error?: string;
}) {
  const errorId = useId();
  return (
    <li>
      <label>
        <input
          type="checkbox"
          checked={checked}
          disabled={disabled}
          onChange={(e) => onChange(e.target.checked)}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? errorId : undefined}
        />
        {label}
      </label>
      {note && <span className="tn-note">{note}</span>}
      {error && (
        <span id={errorId} className="tn-error" role="alert">
          {error}
        </span>
      )}
    </li>
  );
}

/** A multi-select of provider groups, each with the upstream accounts in it. */
export function GroupPicker({
  options,
  chosen,
  onChange,
  disabled,
  accounts,
  errorFor,
}: {
  options: string[];
  chosen: string[];
  onChange: (next: string[]) => void;
  disabled: boolean;
  accounts: Map<string, string[]>;
  errorFor?: (group: string) => string | undefined;
}) {
  if (options.length === 0) return <p className="hint">还没有上游账号设置分组。</p>;
  return (
    <ul className="tn-checks">
      {options.map((g) => {
        const inGroup = accounts.get(g) ?? [];
        return (
          <Check
            key={g}
            label={g}
            checked={chosen.includes(g)}
            disabled={disabled}
            onChange={(on) => onChange(on ? [...chosen, g] : chosen.filter((x) => x !== g))}
            note={inGroup.length > 0 ? `上游账号：${inGroup.join("、")}` : "没有上游账号在此分组"}
            error={errorFor?.(g)}
          />
        );
      })}
    </ul>
  );
}

function GrantsPart({
  tenant,
  canEdit,
  save,
  onReload,
  catalog,
  catalogError,
}: PartProps & { catalog: ResourceView<ModelDoc>[] | null; catalogError: string | null }) {
  const [chosen, setChosen] = useState(() => new Set(tenant.allowed_model_ids));
  // The ids in the order last sent: a refusal names an entry by its index.
  const [sent, setSent] = useState<string[]>([]);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);

  // The stored grants changed (a save, a reload): the form edits them from now on.
  const stored = JSON.stringify(tenant.allowed_model_ids);
  useEffect(() => {
    setChosen(new Set(tenant.allowed_model_ids));
    setRefused(null);
  }, [stored]);
  useConflictClears(tenant.version, setRefused);

  const rows = useMemo(
    () =>
      (catalog ?? [])
        .map((m) => ({ id: m.id, name: m.doc.display_name || m.id, doc: m.doc }))
        .sort((a, b) => a.name.localeCompare(b.name)),
    [catalog],
  );

  function reasonFor(row: (typeof rows)[number]): string | undefined {
    const i = sent.indexOf(row.id);
    const refusedHere = i >= 0 ? refused?.at.get(`/allowed_model_ids/${i}`) : undefined;
    if (refusedHere === undefined) return undefined;
    return ungrantable(row.doc) ? UNGRANTABLE : refusedHere;
  }

  async function submit() {
    const ids = rows.filter((r) => chosen.has(r.id)).map((r) => r.id);
    setBusy(true);
    setRefused(null);
    setNote(null);
    setSent(ids);
    try {
      await save({ allowed_model_ids: ids });
      setNote({ kind: "ok", text: "已保存授权。" });
    } catch (e) {
      setRefused(refusal(e, (p) => /^\/allowed_model_ids\/\d+$/.test(p)));
    } finally {
      setBusy(false);
    }
  }

  return (
    <fieldset className="tn-section">
      <legend>授权模型</legend>
      <p className="hint">租户只能使用这里授权的模型，按平台名称调用；改名不影响授权。删除模型会把它从所有授权里移除。</p>
      {catalogError && (
        <div className="note crit" role="alert">
          模型目录读取失败：{catalogError}
        </div>
      )}
      {catalog === null ? (
        !catalogError && <p className="hint">载入中…</p>
      ) : rows.length === 0 ? (
        <p className="hint">模型目录还是空的。</p>
      ) : (
        <ul className="tn-checks">
          {rows.map((r) => (
            <Check
              key={r.id}
              label={r.name}
              checked={chosen.has(r.id)}
              disabled={!canEdit}
              onChange={(on) =>
                setChosen((prev) => {
                  const next = new Set(prev);
                  if (on) next.add(r.id);
                  else next.delete(r.id);
                  return next;
                })
              }
              error={reasonFor(r)}
            />
          ))}
        </ul>
      )}
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
      {refused?.conflict && <ConflictNote onReload={onReload} />}
      <Msg note={note} />
      {canEdit && (
        // Without the catalog the form shows no choices, and saving would withdraw every grant.
        <button type="button" className="act" disabled={busy || catalog === null} onClick={() => void submit()}>
          保存授权
        </button>
      )}
    </fieldset>
  );
}

function GroupsPart({
  tenant,
  canEdit,
  save,
  onReload,
  options,
  accounts,
  groupsError,
}: PartProps & { options: string[]; accounts: Map<string, string[]>; groupsError: string | null }) {
  const [chosen, setChosen] = useState<string[]>(tenant.allowed_provider_groups);
  const [sent, setSent] = useState<string[]>([]);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);

  const stored = JSON.stringify(tenant.allowed_provider_groups);
  useEffect(() => {
    setChosen(tenant.allowed_provider_groups);
    setRefused(null);
  }, [stored]);
  useConflictClears(tenant.version, setRefused);

  const picked = options.filter((g) => chosen.includes(g));
  const reachable = (g: string) => (accounts.get(g) ?? []).length > 0;
  const unreachable = picked.filter((g) => !reachable(g));

  async function submit() {
    const groups = picked;
    setBusy(true);
    setRefused(null);
    setNote(null);
    setSent(groups);
    try {
      await save({ allowed_provider_groups: groups });
      setNote({ kind: "ok", text: "已保存分组。" });
    } catch (e) {
      setRefused(refusal(e, atList("/allowed_provider_groups")));
    } finally {
      setBusy(false);
    }
  }

  const whole = refused?.at.get("/allowed_provider_groups");
  return (
    <fieldset className="tn-section">
      <legend>可用分组</legend>
      <p className="hint">
        租户看不到分组名。该租户的密钥只会路由到这些分组里的上游账号；单个密钥设了分组覆盖时以覆盖为准。
        {tenant.is_internal && "平台内部租户不选分组即可使用全部上游账号。"}
      </p>
      {groupsError && (
        <div className="note crit" role="alert">
          分组读取失败：{groupsError}
        </div>
      )}
      <GroupPicker
        options={options}
        chosen={chosen}
        onChange={setChosen}
        disabled={!canEdit}
        accounts={accounts}
        errorFor={(g) => refused?.at.get(`/allowed_provider_groups/${sent.indexOf(g)}`)}
      />
      {whole && (
        <div className="note crit" role="alert">
          {whole}
        </div>
      )}
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
      {refused?.conflict && <ConflictNote onReload={onReload} />}
      <Msg note={note} />
      {canEdit && (
        // A choice with no group that holds an upstream account leaves every key of the
        // tenant with nothing to reach, so it is confirmed first.
        <TwoStep
          label="保存分组"
          className="act"
          disabled={busy}
          ask={() => !picked.some(reachable)}
          warning={
            unreachable.length > 0
              ? unreachableWarning(unreachable, "该租户的密钥")
              : "没有选择任何分组。保存后，该租户的密钥找不到任何上游账号，无法调用模型。"
          }
          onConfirm={() => void submit()}
        />
      )}
    </fieldset>
  );
}

/** The five limits, each under its label, with its refusal under it. */
function LimitInputs({
  draft,
  onChange,
  disabled,
  errorFor,
}: {
  draft: LimitDraft;
  onChange: (next: LimitDraft) => void;
  disabled: boolean;
  errorFor: (field: keyof TenantLimits) => string | undefined;
}) {
  return (
    <div className="tn-limits">
      {LIMIT_FIELDS.map(({ field, label }) => (
        <LimitInput
          key={field}
          label={label}
          value={draft[field]}
          disabled={disabled}
          onChange={(value) => onChange({ ...draft, [field]: value })}
          error={errorFor(field)}
        />
      ))}
    </div>
  );
}

export function LimitInput({
  label,
  value,
  disabled,
  onChange,
  error,
}: {
  label: string;
  value: string;
  disabled: boolean;
  onChange: (value: string) => void;
  error?: string;
}) {
  const errorId = useId();
  return (
    <div>
      <label className="f">
        <span>{label}</span>
        <input
          type="number"
          min={1}
          step={1}
          inputMode="numeric"
          placeholder="不限"
          value={value}
          disabled={disabled}
          onChange={(e) => onChange(e.target.value)}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? errorId : undefined}
        />
      </label>
      {error && (
        <span id={errorId} className="tn-error" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}

function LimitsPart({ tenant, canEdit, save, onReload }: PartProps) {
  const [draft, setDraft] = useState(() => limitDraft(tenant.limits));
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);

  const stored = JSON.stringify(tenant.limits);
  useEffect(() => {
    setDraft(limitDraft(tenant.limits));
    setRefused(null);
  }, [stored]);
  useConflictClears(tenant.version, setRefused);

  async function submit() {
    setBusy(true);
    setRefused(null);
    setNote(null);
    try {
      await save({ limits: limitsFrom(draft) });
      setNote({ kind: "ok", text: "已保存限额。" });
    } catch (e) {
      setRefused(refusal(e, (p) => LIMIT_FIELDS.some(({ field }) => p === `/limits/${field}`)));
    } finally {
      setBusy(false);
    }
  }

  return (
    <fieldset className="tn-section">
      <legend>限额</legend>
      <p className="hint">
        前三项限制整个租户（全部密钥合计）；后两项是单个密钥最多能设的值，密钥自己不设时就按这个上限。留空表示不限。
      </p>
      <LimitInputs
        draft={draft}
        onChange={setDraft}
        disabled={!canEdit}
        errorFor={(field) => refused?.at.get(`/limits/${field}`)}
      />
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
      {refused?.conflict && <ConflictNote onReload={onReload} />}
      <Msg note={note} />
      {canEdit && (
        <button type="button" className="act" disabled={busy} onClick={() => void submit()}>
          保存限额
        </button>
      )}
    </fieldset>
  );
}

function MembersPart({
  members,
  error,
  canEdit,
  onChanged,
}: {
  members: Member[] | null;
  error: string | null;
  canEdit: boolean;
  onChanged: (member: Member) => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [rowError, setRowError] = useState<{ id: string; error: unknown } | null>(null);

  async function toggle(m: Member) {
    setBusy(m.id);
    setRowError(null);
    try {
      onChanged(await tenants.setMemberDisabled(m.id, !m.disabled));
    } catch (e) {
      setRowError({ id: m.id, error: e });
    } finally {
      setBusy(null);
    }
  }

  return (
    <section className="tn-section">
      <h3>成员</h3>
      <p className="hint">
        停用后该成员的所有密钥立即失效，其登录也随之结束；启用后这些密钥恢复（成员自己停用的除外）。计费角色不持有可用的密钥。
      </p>
      {error && (
        <div className="note crit" role="alert">
          成员读取失败：{error}
        </div>
      )}
      {members === null ? (
        !error && <p className="hint">载入中…</p>
      ) : members.length === 0 ? (
        <p className="hint">还没有成员。</p>
      ) : (
        <div className="scroll">
          <table aria-label="成员">
            <thead>
              <tr>
                <th>名称</th>
                <th>邮箱</th>
                <th>角色</th>
                <th>状态</th>
                {canEdit && <th>操作</th>}
              </tr>
            </thead>
            <tbody>
              {members.map((m) => (
                <tr key={m.id}>
                  <td>{m.display_name || "—"}</td>
                  <td>{m.email}</td>
                  <td>{ROLE_LABEL[m.role] ?? m.role}</td>
                  <td>{m.disabled ? "已停用" : "正常"}</td>
                  {canEdit && (
                    <td>
                      <div className="tn-actions">
                        {m.disabled ? (
                          <button
                            type="button"
                            className="ghost"
                            disabled={busy === m.id}
                            onClick={() => void toggle(m)}
                          >
                            启用
                          </button>
                        ) : (
                          <TwoStep label="停用" disabled={busy === m.id} onConfirm={() => void toggle(m)} />
                        )}
                      </div>
                      {rowError?.id === m.id && <Failure error={rowError.error} />}
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function InvitePart({ tenant }: { tenant: Tenant }) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<Member["role"]>("member");
  // Held only here: leaving the tenant's page drops it for good.
  const [issued, setIssued] = useState<Invite | null>(null);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [copied, setCopied] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const emailErrorId = useId();

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setRefused(null);
    setIssued(null);
    setCopied(null);
    try {
      setIssued(await tenants.invite(tenant.id, email.trim(), role));
      setEmail("");
    } catch (err) {
      setRefused(refusal(err, (p) => p === "/email"));
    } finally {
      setBusy(false);
    }
  }

  async function copy(text: string, what: string) {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(`已复制${what}`);
    } catch {
      setCopied("无法写入剪贴板，请手动选中复制");
    }
  }

  const emailError = refused?.at.get("/email");
  return (
    // The control plane judges the address: its refusal is shown at the field.
    <form onSubmit={(e) => void submit(e)} noValidate>
      <fieldset className="tn-section">
        <legend>邀请成员</legend>
        <p className="hint">生成一次性的邀请 token，交给受邀人在租户门户接受邀请、设置口令。</p>
        <div className="r">
          <label className="f">
            <span>邮箱</span>
            <input
              type="email"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              aria-invalid={emailError ? true : undefined}
              aria-describedby={emailError ? emailErrorId : undefined}
            />
          </label>
          <label className="f">
            <span>角色</span>
            <select value={role} onChange={(e) => setRole(e.target.value as Member["role"])}>
              {ROLES.map((r) => (
                <option key={r} value={r}>
                  {ROLE_LABEL[r]}
                </option>
              ))}
            </select>
          </label>
          <button type="submit" className="act" disabled={busy}>
            生成邀请
          </button>
        </div>
        {emailError && (
          <span id={emailErrorId} className="tn-error" role="alert">
            {emailError}
          </span>
        )}
        {refused?.general && (
          <div className="note crit" role="alert">
            {refused.general}
          </div>
        )}
        {issued && (
          <div className="note warn">
            <strong>邀请 token 只显示这一次。</strong>请现在复制并交给受邀人；离开此页后无法再查看。
            <code className="tn-token" aria-label="邀请 token">
              {issued.invite_token}
            </code>
            <button type="button" className="ghost" onClick={() => void copy(issued.invite_token, " token")}>
              复制
            </button>
            {issued.invite_url && (
              <>
                <code className="tn-token" aria-label="邀请链接">
                  {issued.invite_url}
                </code>
                <button type="button" className="ghost" onClick={() => void copy(issued.invite_url ?? "", "链接")}>
                  复制链接
                </button>
              </>
            )}
            <p className="hint">有效期至 {new Date(issued.expires_at).toLocaleString()}。</p>
            {copied && <p className="hint">{copied}</p>}
          </div>
        )}
      </fieldset>
    </form>
  );
}

/** Why the gateway refuses key `k`, as far as this page can tell. */
export function whyOff(k: AdminKey, tenant: Tenant, members: Member[] | null): string {
  const why: string[] = [];
  if (k.disabled) why.push("运维停用");
  if (k.disabled_by_tenant) why.push("租户自行停用");
  if (tenant.status !== "active") why.push(`租户${STATUS_LABEL[tenant.status] ?? tenant.status}`);
  const member = k.member_id ? members?.find((m) => m.id === k.member_id) : undefined;
  if (member?.disabled) why.push("成员已停用");
  if (member?.role === "billing") why.push("计费成员不持有可用的密钥");
  return why.join("、");
}

function KeysPart({
  tenant,
  keys,
  error,
  members,
  options,
  accounts,
  canEdit,
  onSaved,
  onReload,
}: {
  tenant: Tenant;
  keys: AdminKey[] | null;
  error: string | null;
  members: Member[] | null;
  options: string[];
  accounts: Map<string, string[]>;
  canEdit: boolean;
  onSaved: (key: AdminKey) => void;
  onReload: () => void;
}) {
  const [editing, setEditing] = useState<string | null>(null);
  const columns = canEdit ? 7 : 6;
  const ownerOf = (k: AdminKey) =>
    k.member_id ? (members?.find((m) => m.id === k.member_id)?.email ?? "成员") : "租户共用";

  return (
    <section className="tn-section">
      <h3>密钥</h3>
      <p className="hint">
        生效分组：密钥设了分组覆盖时用覆盖，否则用租户的可用分组。密钥的新建、停用与吊销在「密钥」页。
      </p>
      {error && (
        <div className="note crit" role="alert">
          密钥读取失败：{error}
        </div>
      )}
      {keys === null ? (
        !error && <p className="hint">载入中…</p>
      ) : keys.length === 0 ? (
        <p className="hint">该租户还没有密钥。</p>
      ) : (
        <div className="scroll">
          <table aria-label="密钥">
            <thead>
              <tr>
                <th>名称</th>
                <th>前缀</th>
                <th>成员</th>
                <th>生效分组</th>
                <th>分组覆盖</th>
                <th>状态</th>
                {canEdit && <th>操作</th>}
              </tr>
            </thead>
            <tbody>
              {keys.map((k) => (
                <Fragment key={k.id}>
                  <tr>
                    <td>{k.label}</td>
                    <td>
                      <code>{k.prefix}…</code>
                    </td>
                    <td>{ownerOf(k)}</td>
                    <td>
                      {k.effective_provider_groups.length > 0 ? k.effective_provider_groups.join("、") : "全部上游"}
                    </td>
                    <td>
                      {k.allowed_provider_groups?.length ? k.allowed_provider_groups.join("、") : "沿用租户"}
                      <StrayNote k={k} tenant={tenant} />
                    </td>
                    <td>
                      {k.effective_disabled ? (
                        <>
                          已停用<span className="tn-note">{whyOff(k, tenant, members)}</span>
                        </>
                      ) : (
                        "正常"
                      )}
                    </td>
                    {canEdit && (
                      <td>
                        <button
                          type="button"
                          className="ghost"
                          aria-expanded={editing === k.id}
                          onClick={() => setEditing(editing === k.id ? null : k.id)}
                        >
                          分组覆盖
                        </button>
                      </td>
                    )}
                  </tr>
                  {editing === k.id && (
                    <tr>
                      <td colSpan={columns}>
                        <OverrideEditor
                          k={k}
                          options={options}
                          accounts={accounts}
                          onSaved={(saved) => {
                            setEditing(null);
                            onSaved(saved);
                          }}
                          onCancel={() => setEditing(null)}
                          onReload={() => {
                            setEditing(null);
                            onReload();
                          }}
                        />
                      </td>
                    </tr>
                  )}
                </Fragment>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

export function OverrideEditor({
  k,
  options,
  accounts,
  onSaved,
  onCancel,
  onReload,
}: {
  k: AdminKey;
  options: string[];
  accounts: Map<string, string[]>;
  onSaved: (key: AdminKey) => void;
  onCancel: () => void;
  onReload: () => void;
}) {
  const [chosen, setChosen] = useState<string[]>(k.allowed_provider_groups ?? []);
  const [sent, setSent] = useState<string[]>([]);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [busy, setBusy] = useState(false);
  const shown = useMemo(() => groupOptions(null, [...options, ...(k.allowed_provider_groups ?? [])]), [options, k]);

  async function submit(groups: string[] | null) {
    setBusy(true);
    setRefused(null);
    setSent(groups ?? []);
    try {
      onSaved(await keysApi.update(k.id, { allowed_provider_groups: groups }, k.version));
    } catch (e) {
      setRefused(refusal(e, atList("/allowed_provider_groups")));
    } finally {
      setBusy(false);
    }
  }

  const whole = refused?.at.get("/allowed_provider_groups");
  const picked = shown.filter((g) => chosen.includes(g));
  const reachable = (g: string) => (accounts.get(g) ?? []).length > 0;
  return (
    <fieldset className="tn-override">
      <legend>分组覆盖</legend>
      <p className="hint">只对这个密钥生效，取代租户的可用分组；清除覆盖后沿用租户的分组。</p>
      <GroupPicker
        options={shown}
        chosen={chosen}
        onChange={setChosen}
        disabled={busy}
        accounts={accounts}
        errorFor={(g) => refused?.at.get(`/allowed_provider_groups/${sent.indexOf(g)}`)}
      />
      {whole && (
        <div className="note crit" role="alert">
          {whole}
        </div>
      )}
      {refused?.tooOld ? (
        <GatewayTooOld error={refused.tooOld} />
      ) : (
        refused?.general && (
          <div className="note crit" role="alert">
            {refused.general}
          </div>
        )
      )}
      {refused?.conflict && <ConflictNote onReload={onReload} />}
      <div className="tn-actions">
        {/* Groups none of which holds an upstream account leave the key nothing to reach (ruling P1b-R6). */}
        <TwoStep
          label="保存覆盖"
          className="act"
          disabled={busy}
          ask={() => picked.length > 0 && !picked.some(reachable)}
          warning={unreachableWarning(picked, "这个密钥")}
          onConfirm={() => void submit(picked)}
        />
        <button
          type="button"
          className="ghost"
          disabled={busy || k.allowed_provider_groups === null}
          onClick={() => void submit(null)}
        >
          清除覆盖
        </button>
        <button type="button" className="ghost" onClick={onCancel}>
          取消
        </button>
      </div>
    </fieldset>
  );
}

/** The defaults new tenants copy when they are created (admins only). */
function DefaultsPanel() {
  const stored = useLoad(() => settings.tenantDefaults(), []);
  const groups = useLoad(() => providerGroups.list(), []);
  if (stored.data === null) {
    return (
      <section className="panel" aria-label="新租户默认值">
        <h2>新租户默认值</h2>
        {stored.error ? (
          <div className="note crit" role="alert">
            {stored.error}
          </div>
        ) : (
          <p className="hint">载入中…</p>
        )}
      </section>
    );
  }
  return <DefaultsForm stored={stored.data} available={groups.data} onSaved={stored.setData} />;
}

function DefaultsForm({
  stored,
  available,
  onSaved,
}: {
  stored: TenantDefaults;
  available: ProviderGroup[] | null;
  onSaved: (defaults: TenantDefaults) => void;
}) {
  const [draft, setDraft] = useState(() => limitDraft(stored));
  const [chosen, setChosen] = useState<string[]>(stored.default_provider_groups);
  const [sent, setSent] = useState<string[]>([]);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);
  const options = useMemo(
    () => groupOptions(available, stored.default_provider_groups),
    [available, stored.default_provider_groups],
  );
  const accounts = useMemo(() => accountsByGroup(available), [available]);

  async function submit() {
    const groups = options.filter((g) => chosen.includes(g));
    setBusy(true);
    setRefused(null);
    setNote(null);
    setSent(groups);
    try {
      onSaved(await settings.saveTenantDefaults({ ...limitsFrom(draft), default_provider_groups: groups }));
      setNote({ kind: "ok", text: "已保存。之后创建的租户使用这些默认值，已有租户不受影响。" });
    } catch (e) {
      setRefused(
        refusal(
          e,
          (p) => atList("/default_provider_groups")(p) || LIMIT_FIELDS.some(({ field }) => p === `/${field}`),
        ),
      );
    } finally {
      setBusy(false);
    }
  }

  const whole = refused?.at.get("/default_provider_groups");
  return (
    <section className="panel" aria-label="新租户默认值">
      <h2>新租户默认值</h2>
      <p className="hint">新租户创建时复制这里的限额与默认分组；修改不影响已有租户。留空的限额表示不限。</p>
      <LimitInputs draft={draft} onChange={setDraft} disabled={false} errorFor={(field) => refused?.at.get(`/${field}`)} />
      <fieldset className="tn-section">
        <legend>默认分组</legend>
        <p className="hint">新租户的密钥只会路由到这些分组里的上游账号；至少要选一个。</p>
        <GroupPicker
          options={options}
          chosen={chosen}
          onChange={setChosen}
          disabled={false}
          accounts={accounts}
          errorFor={(g) => refused?.at.get(`/default_provider_groups/${sent.indexOf(g)}`)}
        />
        {whole && (
          <div className="note crit" role="alert">
            {whole}
          </div>
        )}
      </fieldset>
      {refused?.general && (
        <div className="note crit" role="alert">
          {refused.general}
        </div>
      )}
      <Msg note={note} />
      <button type="button" className="act" disabled={busy} onClick={() => void submit()}>
        保存默认值
      </button>
    </section>
  );
}
