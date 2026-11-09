import { Fragment, useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import type { TabProps } from "../App";
import { ConflictNote } from "../components/ConflictNote";
import { GatewayTooOld } from "../components/GatewayTooOld";
import { Msg, type Note } from "../components/Msg";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  ApiError,
  groupPrices,
  groups as groupsApi,
  keys as keysApi,
  resources,
  settings,
  tenants,
  type AdminKey,
  type Group,
  type Invite,
  type Member,
  type ResourceView,
  type Tenant,
  type TenantChanges,
  type TenantDefaults,
  type TenantLimits,
} from "../lib/api";
import { messageFor } from "../lib/errors";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";
import { CopyButton } from "../ui/CopyButton";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { GroupName } from "../ui/GroupName";
import { Lamp, type LampState } from "../ui/Lamp";
import { SidePanel } from "../ui/SidePanel";
import { TwoStep } from "../ui/TwoStep";
import "./Tenants.css";

export const STATUS_LABEL: Record<Tenant["status"], string> = {
  pending: "待审核",
  active: "正常",
  suspended: "已停用",
};

/** The lamp each tenant status shows. */
const STATUS_LAMP: Record<Tenant["status"], LampState> = {
  pending: "pending",
  active: "applied",
  suspended: "disabled",
};

/** A tenant's status as a lamp; a status a newer control plane added shows as it is named. */
export function TenantStatus({ status }: { status: Tenant["status"] }) {
  return <Lamp state={STATUS_LAMP[status] ?? "disabled"}>{STATUS_LABEL[status] ?? status}</Lamp>;
}

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

/** Shown beside a granted model the model catalog, as last read, does not list. */
const NOT_IN_CATALOG = "目录中未找到";

/** Shown beside a granted model with no baseline in force (design D9). */
const UNPRICED = "未定价：调用会暂不计费，直到设置基准价";

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

/** Every group worth offering: the live groups and those already named, by routing name. */
function groupOptions(available: Group[] | null, stored: string[]): Group[] {
  const live = available ?? [];
  const listed = new Set(live.map((g) => g.name));
  // A group a stored value names but the table does not list any more (retired
  // since it was chosen) stays listed, so it can be seen and taken away.
  const gone = stored
    .filter((name) => !listed.has(name))
    .map((name) => ({ id: name, name, display_name: name }) as Group);
  return [...live, ...gone].sort((a, b) => a.display_name.localeCompare(b.display_name));
}

/** The upstream accounts in each group, by display name. */
function accountsByGroup(available: Group[] | null): Map<string, string[]> {
  return new Map((available ?? []).map((g) => [g.name, g.provider_keys.map((k) => k.display_name ?? k.id)]));
}

/** The picker's options: each live group by display name, with the accounts in it. */
export function groupChoices(available: Group[] | null, stored: string[] = []): GroupChoice[] {
  const accounts = accountsByGroup(available);
  return groupOptions(available, stored).map((g) => ({
    id: g.id,
    name: g.name,
    display_name: g.display_name,
    accounts: accounts.get(g.name) ?? [],
  }));
}

/**
 * The groups key `k` says it is in that tenant `t` no longer assigns. A key
 * names one group, chosen at creation; losing that group from the tenant
 * closes the key rather than moving it.
 */
export function strayGroups(k: AdminKey, t: Tenant): string[] {
  if (!k.group || t.is_internal) return [];
  return t.allowed_provider_groups.includes(k.group.name) ? [] : [k.group.name];
}

/**
 * A key's effective groups, as listed. None reaches every upstream for the
 * platform's own tenant; a customer tenant's key with none is refused by the
 * gateway.
 */
export function groupsText(groups: string[], tenant: Tenant): string {
  if (groups.length > 0) return groups.join("、");
  return tenant.is_internal ? "全部上游" : "无分组（密钥不可用）";
}

/** The note beside a key whose group its tenant no longer assigns. */
export function StrayNote({ k, tenant }: { k: AdminKey; tenant: Tenant }) {
  const stray = strayGroups(k, tenant);
  if (stray.length === 0) return null;
  return <span className="tn-note tn-flag">所在分组 {stray.join("、")} 不在租户的可用分组中，该密钥已停用</span>;
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
  // Bumped by 刷新: the open tenant's page reads its lists again.
  const [tick, setTick] = useState(0);
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

  function refreshAll() {
    void loadList();
    if (selected) void loadDetail(selected);
    setTick((n) => n + 1);
  }

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
    <div className={detail ? "with-side is-open" : "with-side"}>
      <div className="side-main">
        <div className="panel">
          <h2>
            租户
            <Button size="sm" type="button" icon="refresh" onClick={refreshAll}>
              刷新
            </Button>
            {operator.role === "admin" && (
              <Button
                size="sm"
                type="button"
                aria-expanded={showDefaults}
                onClick={() => setShowDefaults((open) => !open)}
              >
                新租户默认值
              </Button>
            )}
          </h2>
          <p className="hint">
            客户经租户门户自助注册后在这里审核；批准前它不能自行创建密钥，名下的密钥也都不可用。停用租户会让它的全部密钥立即失效，恢复后重新可用。
          </p>
          <label className="f filter">
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
              <Button type="button" onClick={() => void loadList()}>
                重试
              </Button>
            </div>
          )}
          {items === null ? (
            !listError && <Loading />
          ) : items.length === 0 ? (
            <EmptyState text="没有符合条件的租户。" />
          ) : (
            <DataTable label="租户列表">
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
                      <button
                        type="button"
                        className="link-button tn-name"
                        title={t.name}
                        aria-expanded={selected === t.id}
                        onClick={() => setSelected(t.id)}
                      >
                        {t.name}
                      </button>
                      {t.is_internal && <span className="tag tn-mark">平台内部</span>}
                    </td>
                    <td>
                      <TenantStatus status={t.status} />
                    </td>
                    <td>{BILLING_LABEL[t.billing_mode] ?? t.billing_mode}</td>
                    <td className="right">{fmtInt(t.member_count)}</td>
                    <td className="right">{fmtInt(t.key_count)}</td>
                    <td>
                      <RolloutBadge rollout={t.rollout} />
                    </td>
                    {canEdit && (
                      <td>
                        <div className="tn-actions">
                          {t.status === "pending" && (
                            <Button
                              size="sm"
                              type="button"
                              disabled={acting === t.id}
                              busy={acting === t.id}
                              busyLabel="批准"
                              onClick={() => void act(t, tenants.approve)}
                            >
                              批准
                            </Button>
                          )}
                          {/* The platform's own tenant holds the operators' keys; it is never suspended. */}
                          {t.status === "active" && !t.is_internal && (
                            <TwoStep
                              label="停用"
                              size="sm"
                              disabled={acting === t.id}
                              busy={acting === t.id}
                              onConfirm={() => void act(t, tenants.suspend)}
                            />
                          )}
                          {t.status === "suspended" && (
                            <Button
                              size="sm"
                              type="button"
                              disabled={acting === t.id}
                              busy={acting === t.id}
                              busyLabel="恢复"
                              onClick={() => void act(t, tenants.reactivate)}
                            >
                              恢复
                            </Button>
                          )}
                        </div>
                        {rowError?.id === t.id && <Failure error={rowError.error} />}
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </DataTable>
          )}
        </div>

        {showDefaults && operator.role === "admin" && <DefaultsPanel />}

        {selected && detailError && (
          <div className="note crit" role="alert">
            {detailError}{" "}
            <Button type="button" onClick={() => void loadDetail(selected)}>
              重试
            </Button>
          </div>
        )}
      </div>
      {detail && (
        <TenantDetail
          key={detail.id}
          tenant={detail}
          canEdit={canEdit}
          tick={tick}
          onView={settle}
          onRefresh={() => void refresh(detail.id)}
          onReload={() => void loadDetail(detail.id)}
          onClose={() => setSelected(null)}
        />
      )}
    </div>
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
  tick,
  onView,
  onRefresh,
  onReload,
  onClose,
}: {
  tenant: Tenant;
  canEdit: boolean;
  /** Changes when the operator asks for everything to be read again. */
  tick: number;
  onView: (view: Tenant) => void;
  onRefresh: () => void;
  onReload: () => void;
  onClose: () => void;
}) {
  // Read again with every version of the tenant: a grant made elsewhere can
  // name a model created after the last read.
  const catalog = useLoad(() => resources.list<ModelDoc>("models"), [tenant.version, tick]);
  const groups = useLoad(() => groupsApi.list(), [tick]);
  // Every model a group can price, with its baseline: a granted model with
  // none is a call nobody can bill yet.
  const prices = useLoad(() => groupPrices.list(), [tick]);
  const members = useLoad(() => tenants.members(tenant.id), [tenant.id, tick]);
  // Every change to the tenant's row can change what its keys are given.
  const keyList = useLoad(() => tenants.keys(tenant.id), [tenant.id, tenant.version, tick]);

  const save = useCallback(
    async (changes: TenantChanges) => {
      const view = await tenants.update(tenant.id, changes, tenant.version);
      onView(view);
      return view;
    },
    [tenant.id, tenant.version, onView],
  );

  const choices = useMemo(
    () => groupChoices(groups.data, tenant.allowed_provider_groups),
    [groups.data, tenant.allowed_provider_groups],
  );
  const priced = useMemo(
    () =>
      prices.data === null
        ? null
        : new Set((prices.data ?? []).filter((r) => r.baseline.current !== null).map((r) => r.model_id)),
    [prices.data],
  );
  const part = { tenant, canEdit, save, onReload };

  return (
    <SidePanel
      title={tenant.name}
      label={`租户详情：${tenant.name}`}
      extra={<RolloutBadge rollout={tenant.rollout} />}
      onClose={onClose}
    >
      <p className="hint tn-facts">
        {STATUS_LABEL[tenant.status] ?? tenant.status} · {BILLING_LABEL[tenant.billing_mode] ?? tenant.billing_mode} ·
        成员 {tenant.member_count} · 密钥 {tenant.key_count}
        {tenant.is_internal && " · 平台内部租户：它的密钥是运维密钥，没有成员"}
      </p>

      <GrantsPart
        {...part}
        catalog={catalog.data}
        catalogError={catalog.error}
        priced={priced}
      />
      <GroupsPart {...part} choices={choices} groupsError={groups.error} keys={keyList.data} />
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
        choices={choices}
        canEdit={canEdit}
        onSaved={(k) => {
          keyList.setData((list) => list && list.map((x) => (x.id === k.id ? k : x)));
          onRefresh();
        }}
        onReload={() => void keyList.reload()}
      />
    </SidePanel>
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
  route,
}: {
  label: string;
  checked: boolean;
  disabled: boolean;
  onChange: (on: boolean) => void;
  note?: string;
  error?: string;
  /** The routing name beside the label, in small text: the control is still named by the label. */
  route?: string;
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
      {route !== undefined && <code className="grp-route">{route}</code>}
      {note && <span className="tn-note">{note}</span>}
      {error && (
        <span id={errorId} className="tn-error" role="alert">
          {error}
        </span>
      )}
    </li>
  );
}

/** One group the picker offers: what people choose by, and the routing name the documents store. */
export interface GroupChoice {
  /** The group's id, which a key names. */
  id: string;
  /** The routing name: what a tenant's assignment list, a key's group and an account's document hold. */
  name: string;
  /** What people see and choose by. */
  display_name: string;
  /** The upstream accounts in the group, by display name. */
  accounts: string[];
}

/**
 * The group picker every operator page shares: a checkbox per group, named by
 * its display name, with the routing name in small text beside it, so an
 * operator chooses what a tenant sees and still sees what the document
 * stores. A group with no account in it says so: choosing only those leaves
 * whoever it is for with no upstream to reach.
 */
export function GroupPicker({
  options,
  chosen,
  onChange,
  disabled,
  errorFor,
}: {
  options: GroupChoice[];
  /** The routing names chosen. */
  chosen: string[];
  onChange: (next: string[]) => void;
  disabled: boolean;
  errorFor?: (group: string) => string | undefined;
}) {
  if (options.length === 0) return <p className="hint">还没有可选的分组。</p>;
  return (
    <ul className="tn-checks tn-groups">
      {options.map((g) => (
        <Check
          key={g.name}
          label={g.display_name}
          route={g.name !== g.display_name ? g.name : undefined}
          checked={chosen.includes(g.name)}
          disabled={disabled}
          onChange={(on) => onChange(on ? [...chosen, g.name] : chosen.filter((x) => x !== g.name))}
          note={g.accounts.length > 0 ? `上游账号：${g.accounts.join("、")}` : "没有上游账号在此分组"}
          error={errorFor?.(g.name)}
        />
      ))}
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
  priced,
}: PartProps & {
  catalog: ResourceView<ModelDoc>[] | null;
  catalogError: string | null;
  /** The ids of the models with a baseline in force; `null` until the list is read. */
  priced: Set<string> | null;
}) {
  const [chosen, setChosen] = useState(() => new Set(tenant.allowed_model_ids));
  const [part, focusRefusal] = useRefusalFocus<HTMLFieldSetElement>();
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

  // The catalog's models, then every granted or chosen model it does not
  // list: a save sends the whole list, so a grant made since the catalog was
  // read must stay in it rather than be withdrawn unseen.
  const rows = useMemo(() => {
    const listed: { id: string; name: string; doc: ModelDoc | null }[] = (catalog ?? [])
      .map((m) => ({ id: m.id, name: m.doc.display_name || m.id, doc: m.doc }))
      .sort((a, b) => a.name.localeCompare(b.name));
    const known = new Set(listed.map((r) => r.id));
    const unknown = [...new Set([...tenant.allowed_model_ids, ...chosen])]
      .filter((id) => !known.has(id))
      .sort()
      .map((id) => ({ id, name: id, doc: null }));
    return [...listed, ...unknown];
  }, [catalog, tenant.allowed_model_ids, chosen]);

  function reasonFor(row: (typeof rows)[number]): string | undefined {
    const i = sent.indexOf(row.id);
    const refusedHere = i >= 0 ? refused?.at.get(`/allowed_model_ids/${i}`) : undefined;
    if (refusedHere === undefined) return undefined;
    return row.doc && ungrantable(row.doc) ? UNGRANTABLE : refusedHere;
  }

  /**
   * What an operator should know before saving the row: a model the catalog
   * does not list, and a model with no baseline in force, whose calls are
   * stored unpriced until one is set (D9). The second is read only once the
   * price list has arrived: an unknown price is not an absent one.
   */
  function noteFor(row: (typeof rows)[number]): string | undefined {
    const notes: string[] = [];
    if (!row.doc) notes.push(NOT_IN_CATALOG);
    if (priced !== null && !priced.has(row.id)) notes.push(UNPRICED);
    return notes.length > 0 ? notes.join(" ") : undefined;
  }

  // Grants the save takes away: every key of the tenant loses those models.
  const withdrawn = tenant.allowed_model_ids.filter((id) => !chosen.has(id));
  // Unsaved edits: the save turns Sign yellow only while there are some.
  const granted = [...chosen].some((id) => !tenant.allowed_model_ids.includes(id));
  const nameOf = (id: string) => rows.find((r) => r.id === id)?.name ?? id;

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
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  return (
    <fieldset ref={part} className="tn-section">
      <legend>授权模型</legend>
      <p className="hint">租户只能使用这里授权的模型，按平台名称调用；改名不影响授权。删除模型会把它从所有授权里移除。</p>
      {catalogError && (
        <div className="note crit" role="alert">
          模型目录读取失败：{catalogError}
        </div>
      )}
      {catalog === null ? (
        !catalogError && <Loading />
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
              note={noteFor(r)}
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
        // Until the catalog is read the form shows no choices. A save that takes a
        // grant away cuts every key of the tenant off from it, so it is confirmed
        // first (ruling P1b-R6).
        <TwoStep
          label="保存授权"
          kind={withdrawn.length > 0 || granted ? "primary" : "secondary"}
          disabled={busy || catalog === null}
          busy={busy}
          ask={() => withdrawn.length > 0}
          warning={`将撤销 ${withdrawn.map(nameOf).join("、")} 的授权。保存后，该租户的所有密钥都不能再调用这些模型。`}
          onConfirm={() => void submit()}
        />
      )}
    </fieldset>
  );
}

/** The id of the group a routing name names, from a picker's own list; the name when none does. */
export function idOf(choices: GroupChoice[], name: string): string {
  return choices.find((g) => g.name === name)?.id ?? name;
}

function GroupsPart({
  tenant,
  canEdit,
  save,
  onReload,
  choices,
  groupsError,
  keys,
}: PartProps & { choices: GroupChoice[]; groupsError: string | null; keys: AdminKey[] | null }) {
  const [chosen, setChosen] = useState<string[]>(tenant.allowed_provider_groups);
  const [part, focusRefusal] = useRefusalFocus<HTMLFieldSetElement>();
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

  const picked = choices.filter((g) => chosen.includes(g.name)).map((g) => g.name);
  const reachable = (name: string) => (choices.find((g) => g.name === name)?.accounts.length ?? 0) > 0;
  const unreachable = picked.filter((g) => !reachable(g));
  const edited = picked.join() !== tenant.allowed_provider_groups.join();

  // The groups this save takes away, and how many of the tenant's live keys
  // each one stops: a key names one group, so losing it closes exactly the
  // keys in it. Counting every key of the tenant would overstate the damage.
  const removed = tenant.allowed_provider_groups.filter((name) => !chosen.includes(name));
  const stopped = removed
    .map((name) => ({ name, count: (keys ?? []).filter((k) => k.group?.name === name).length }))
    .filter((s) => s.count > 0);
  const labelOf = (name: string) => choices.find((g) => g.name === name)?.display_name ?? name;
  const stopLines = stopped.map(
    (s) => `移除分组「${labelOf(s.name)}」：将停用该租户在此分组下的 ${s.count} 个密钥。`,
  );

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
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const whole = refused?.at.get("/allowed_provider_groups");
  // The save is confirmed when it stops keys, and when it would leave the
  // tenant with no group that holds an upstream account.
  const blocked = !picked.some(reachable);
  return (
    <fieldset ref={part} className="tn-section">
      <legend>可用分组</legend>
      <p className="hint">
        租户按显示名看到分组。该租户的每个密钥只路由到它自己那一个分组里的上游账号；移除一个分组会停用该租户在这个分组下的密钥，再加回来时它们重新可用。
        {tenant.is_internal && "平台内部租户的密钥可以选任意分组。"}
      </p>
      {groupsError && (
        <div className="note crit" role="alert">
          分组读取失败：{groupsError}
        </div>
      )}
      <GroupPicker
        options={choices}
        chosen={chosen}
        onChange={setChosen}
        disabled={!canEdit}
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
        <TwoStep
          label="保存分组"
          kind={edited ? "primary" : "secondary"}
          disabled={busy}
          busy={busy}
          ask={() => stopped.length > 0 || blocked}
          warning={
            <>
              {stopLines.map((line) => (
                <span key={line}>{line}</span>
              ))}
              {blocked &&
                (unreachable.length > 0
                  ? unreachableWarning(unreachable.map(labelOf), "该租户的密钥")
                  : "没有选择任何分组。保存后，该租户的密钥找不到任何上游账号，无法调用模型。")}
            </>
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
  const [part, focusRefusal] = useRefusalFocus<HTMLFieldSetElement>();
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
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  return (
    <fieldset ref={part} className="tn-section">
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
        <Button
          kind={JSON.stringify(draft) !== JSON.stringify(limitDraft(tenant.limits)) ? "primary" : "secondary"}
          type="button"
          busy={busy}
          busyLabel="保存限额"
          onClick={() => void submit()}
        >
          保存限额
        </Button>
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
    <section className="tn-section tn-sep">
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
        !error && <Loading />
      ) : members.length === 0 ? (
        <p className="status-line">还没有成员。</p>
      ) : (
        <DataTable label="成员">
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
                <td>
                  <Lamp state={m.disabled ? "disabled" : "applied"}>{m.disabled ? "已停用" : "正常"}</Lamp>
                </td>
                {canEdit && (
                  <td>
                    <div className="tn-actions">
                      {m.disabled ? (
                        <Button
                          size="sm"
                          type="button"
                          disabled={busy === m.id}
                          busy={busy === m.id}
                          busyLabel="启用"
                          onClick={() => void toggle(m)}
                        >
                          启用
                        </Button>
                      ) : (
                        <TwoStep
                          label="停用"
                          size="sm"
                          disabled={busy === m.id}
                          busy={busy === m.id}
                          onConfirm={() => void toggle(m)}
                        />
                      )}
                    </div>
                    {rowError?.id === m.id && <Failure error={rowError.error} />}
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </DataTable>
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
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

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
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const copyFailed = () => setCopied("无法写入剪贴板，请手动选中复制");

  const emailError = refused?.at.get("/email");
  return (
    // The control plane judges the address: its refusal is shown at the field.
    <form ref={form} onSubmit={(e) => void submit(e)} noValidate>
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
          <Button kind="primary" type="submit" busy={busy} busyLabel="生成邀请">
            生成邀请
          </Button>
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
            <CopyButton text={issued.invite_token} onFailed={copyFailed} />
            {issued.invite_url && (
              <>
                <code className="tn-token" aria-label="邀请链接">
                  {issued.invite_url}
                </code>
                <CopyButton text={issued.invite_url} label="复制链接" onFailed={copyFailed} />
              </>
            )}
            <p className="hint">有效期至 {fmtTime(issued.expires_at)}。</p>
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
  if (!tenant.is_internal) {
    if (k.group === null) why.push("未选分组");
    else if (!tenant.allowed_provider_groups.includes(k.group.name)) why.push("所在分组已不在租户的可用分组中");
  }
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
  choices,
  canEdit,
  onSaved,
  onReload,
}: {
  tenant: Tenant;
  keys: AdminKey[] | null;
  error: string | null;
  members: Member[] | null;
  choices: GroupChoice[];
  canEdit: boolean;
  onSaved: (key: AdminKey) => void;
  onReload: () => void;
}) {
  const [editing, setEditing] = useState<string | null>(null);
  const columns = canEdit ? 6 : 5;
  const ownerOf = (k: AdminKey) =>
    k.member_id ? (members?.find((m) => m.id === k.member_id)?.email ?? "成员") : "租户共用";

  return (
    <section className="tn-section">
      <h3>密钥</h3>
      <p className="hint">
        每个密钥在创建时选定一个分组，之后不能更改；新建、停用与吊销在「密钥」页。分组被从租户移除时，该分组下的密钥停用，分组加回来时重新可用。
      </p>
      {error && (
        <div className="note crit" role="alert">
          密钥读取失败：{error}
        </div>
      )}
      {keys === null ? (
        !error && <Loading />
      ) : keys.length === 0 ? (
        <p className="status-line">该租户还没有密钥。</p>
      ) : (
        <DataTable label="密钥">
          <thead>
            <tr>
              <th>名称</th>
              <th>前缀</th>
              <th>成员</th>
              <th>分组</th>
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
                    <KeyGroupCell k={k} />
                    <StrayNote k={k} tenant={tenant} />
                  </td>
                  <td>
                    {k.effective_disabled ? (
                      <>
                        <Lamp state="disabled">已停用</Lamp>
                        <span className="tn-note">{whyOff(k, tenant, members)}</span>
                      </>
                    ) : (
                      <Lamp state="applied">正常</Lamp>
                    )}
                  </td>
                  {canEdit && (
                    <td>
                      {k.group === null && (
                        <Button
                          size="sm"
                          type="button"
                          aria-expanded={editing === k.id}
                          onClick={() => setEditing(editing === k.id ? null : k.id)}
                        >
                          选择分组
                        </Button>
                      )}
                    </td>
                  )}
                </tr>
                {editing === k.id && (
                  <tr className="dt-detail">
                    <td colSpan={columns}>
                      <ChooseGroup
                        k={k}
                        choices={choices}
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
        </DataTable>
      )}
    </section>
  );
}

/** A key's group in a table cell: its display name, with the routing name in small text when they differ. */
export function KeyGroupCell({ k }: { k: AdminKey }) {
  if (k.group === null) return <span className="tn-note">未选分组</span>;
  const same = k.group.display_name === k.group.name;
  return (
    <span className="grps grps-one">
      <GroupName name={same ? k.group.name : k.group.display_name} />
      {!same && <span className="tn-note">{k.group.name}</span>}
    </span>
  );
}

/**
 * The one-time group a legacy key takes, for a key an older control-plane
 * replica minted without one. A key that has a group never takes another, so
 * the page offers this only where `k.group` is null.
 */
export function ChooseGroup({
  k,
  choices,
  onSaved,
  onCancel,
  onReload,
}: {
  k: AdminKey;
  choices: GroupChoice[];
  onSaved: (key: AdminKey) => void;
  onCancel: () => void;
  onReload: () => void;
}) {
  const [chosen, setChosen] = useState<string[]>([]);
  const [part, focusRefusal] = useRefusalFocus<HTMLFieldSetElement>();
  // The version the edit started from: a newer one must refuse this save (412).
  const [version] = useState(k.version);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [busy, setBusy] = useState(false);
  const picked = choices.filter((g) => chosen.includes(g.name));
  const reachable = (name: string) => (choices.find((g) => g.name === name)?.accounts.length ?? 0) > 0;

  async function submit(name: string) {
    setBusy(true);
    setRefused(null);
    try {
      onSaved(await keysApi.update(k.id, { group_id: idOf(choices, name) }, version));
    } catch (e) {
      setRefused(refusal(e, atList("/group_id")));
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const whole = refused?.at.get("/group_id");
  return (
    <fieldset ref={part} className="tn-override">
      <legend>选择分组</legend>
      <p className="hint">这个密钥还没有分组（由旧版本创建）。分组只能设置一次，设置后不能更改。</p>
      <GroupPicker
        options={choices}
        chosen={chosen}
        onChange={(next) => setChosen(next.slice(-1))}
        disabled={busy}
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
        <TwoStep
          label="保存分组"
          kind="primary"
          disabled={busy || picked.length === 0}
          busy={busy}
          ask={() => picked.length > 0 && !picked.some((g) => reachable(g.name))}
          warning={unreachableWarning(picked.map((g) => g.display_name), "这个密钥")}
          onConfirm={() => void submit(picked[0]?.name ?? "")}
        />
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
      </div>
    </fieldset>
  );
}

/** The defaults new tenants copy when they are created (admins only). */
function DefaultsPanel() {
  const stored = useLoad(() => settings.tenantDefaults(), []);
  const groups = useLoad(() => groupsApi.list(), []);
  if (stored.data === null) {
    return (
      <section className="panel" aria-label="新租户默认值">
        <h2>新租户默认值</h2>
        {stored.error ? (
          <div className="note crit" role="alert">
            {stored.error}
          </div>
        ) : (
          <Loading />
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
  available: Group[] | null;
  onSaved: (defaults: TenantDefaults) => void;
}) {
  const [draft, setDraft] = useState(() => limitDraft(stored));
  const [panel, focusRefusal] = useRefusalFocus<HTMLElement>();
  const [chosen, setChosen] = useState<string[]>(stored.default_provider_groups);
  const [sent, setSent] = useState<string[]>([]);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);
  const choices = useMemo(
    () => groupChoices(available, stored.default_provider_groups),
    [available, stored.default_provider_groups],
  );

  async function submit() {
    const groups = choices.filter((g) => chosen.includes(g.name)).map((g) => g.name);
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
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const whole = refused?.at.get("/default_provider_groups");
  return (
    <section ref={panel} className="panel" aria-label="新租户默认值">
      <h2>新租户默认值</h2>
      <p className="hint">新租户创建时复制这里的限额与默认分组；修改不影响已有租户。留空的限额表示不限。</p>
      <LimitInputs draft={draft} onChange={setDraft} disabled={false} errorFor={(field) => refused?.at.get(`/${field}`)} />
      <fieldset className="tn-section">
        <legend>默认分组</legend>
        <p className="hint">新租户的密钥只会路由到这些分组里的上游账号；至少要选一个。</p>
        <GroupPicker
          options={choices}
          chosen={chosen}
          onChange={setChosen}
          disabled={false}
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
      <Button kind="primary" type="button" busy={busy} busyLabel="保存默认值" onClick={() => void submit()}>
        保存默认值
      </Button>
    </section>
  );
}
