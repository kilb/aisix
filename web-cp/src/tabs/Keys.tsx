import "./operations.css";
import "./Keys.css";
import { beijingInputValue, parseBeijingInput } from "../lib/time";
import { keySearchText } from "../lib/operationalRecords";
import { useRecordTable } from "../lib/recordTable";
import { RecordTools, RecordPager } from "../components/RecordTools";
/** 当前租户的范围、概况与密钥列表按通栏顺序显示。 */
import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import type { TabProps } from "../App";
import { GatewayTooOld } from "../components/GatewayTooOld";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  groups as groupsApi,
  billing,
  createKeyQuota,
  updateOrgQuota,
  deleteOrgQuota,
  keys as keysApi,
  resources,
  tenants,
  type AdminKey,
  type Member,
  type MintedKey,
  type NewKey,
  type NewKeyBudget,
  type Budget,
  type Tenant,
} from "../lib/api";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { keyModels } from "../lib/keyModels";
import { adminKeySettingsPatch } from "../lib/keySettingsAdmin";
import { isKeyBudgetAmount, keyBudgetAmountInput } from "../lib/keyBudget";
import { fmtMicroCny } from "../lib/money";
import {
  atList,
  Check,
  Failure,
  follow,
  groupChoices,
  groupsText,
  idOf,
  KeyGroupCell,
  LimitInput,
  refusal,
  ROLE_LABEL,
  STATUS_LABEL,
  StrayNote,
  unreachableWarning,
  useLifetime,
  useLoad,
  whyOff,
  type GroupChoice,
  type Refusal,
} from "./Tenants";
import { Card, CardHeader, Metric, PageHeader } from "../components/page";
import { Button } from "../ui/Button";
import { FormDialog } from "../ui/FormDialog";
import { Field, useRefusalFocus } from "../ui/Field";
import { CopyButton } from "../ui/CopyButton";
import { KeySecret } from "../ui/KeySecret";
import { OperationalTable as DataTable, Loading } from "../components/OperationalTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";
import { Select } from "../ui/Select";
import { ArrowLeft, Pencil, Power, PowerOff, Trash2 } from "lucide-react";

/** The roles that manage every key of their tenant, and so may mint one for someone else. */
const MANAGERS: Member["role"][] = ["owner", "admin"];

function tenantLabel(t: Tenant): string {
  if (t.is_internal) return `${t.name}（平台内部）`;
  return t.status === "active" ? t.name : `${t.name}（${STATUS_LABEL[t.status] ?? t.status}）`;
}

/** The platform's own tenant first, then the others by name. */
function byPicker(a: Tenant, b: Tenant): number {
  return Number(b.is_internal) - Number(a.is_internal) || a.name.localeCompare(b.name);
}

export function Keys({ canEdit, routeQuery }: TabProps) {
  const editingId = routeQuery?.get("key") ?? null;
  const routedTenant = routeQuery?.get("tenant") ?? null;
  const list = useLoad(() => editingId && routedTenant ? Promise.resolve<Tenant[]>([]) : tenants.list(), [editingId, routedTenant]);
  const ordered = useMemo(() => [...(list.data ?? [])].sort(byPicker), [list.data]);
  const [chosen, setChosen] = useState<string | null>(() => routeQuery?.get("tenant") ?? null);
  const tenantId = chosen ?? ordered[0]?.id ?? null;
  useEffect(() => setChosen(routeQuery?.get("tenant") ?? null), [routeQuery]);
  // Bumped by 刷新: the tenant shown reads its lists again.
  const [tick, setTick] = useState(0);

  return (
    <div className="cp-operations cp-keys">
      {/* 租户切换保留独立的数据边界，密钥列表占满工作区。 */}
      <PageHeader
        title={editingId ? "编辑密钥" : "密钥"}
        eyebrow="客户管理"
        dataLead={editingId ? "管理该密钥的模型范围、调用限制与费用额度。" : list.data === null ? "读取当前可管理的租户。" : `共有 ${fmtInt(ordered.length)} 个租户可查看；统计只包含当前选择的租户。`}
        description="平台内部租户的密钥是运维自用的，可以按名称模式授权模型；其他租户的密钥只能使用该租户已授权的模型。平台内部租户的密钥明文可随时复制；客户租户的密钥在这里只显示前缀，明文由该租户在自己的门户里复制。"
        actions={
          <Button
            type="button"
            icon="refresh"
            aria-label="刷新密钥"
            title="刷新密钥"
            onClick={() => {
              void list.reload();
              setTick((n) => n + 1);
            }}
          />
        }
      />
      {!editingId && <div className="keys-scope">
        <Card className="keys-scope-card">
          <div className="keys-scope-heading"><h2 className="text-title text-fg">租户范围</h2><p className="text-caption text-fg-3">切换租户会清除当前新建结果。</p></div>
          {list.error && (
            <div className="note crit" role="alert">
              {list.error}{" "}
              <Button type="button" size="sm" onClick={() => void list.reload()}>
                重试
              </Button>
            </div>
          )}
          {list.data === null ? (
            !list.error && <Loading />
          ) : ordered.length === 0 ? (
            <EmptyState text="还没有租户。" />
          ) : (
            <div className="flex flex-wrap items-end gap-3">
              <label className="f filter filter-wide">
                <span>租户</span>
                <Select size="compact" value={tenantId ?? ""} onValueChange={(value) => { setChosen(value); window.location.hash = `#keys?tenant=${encodeURIComponent(value)}`; }}>
                  {ordered.map((t) => (
                    <option key={t.id} value={t.id}>
                      {tenantLabel(t)}
                    </option>
                  ))}
                </Select>
              </label>
            </div>
          )}
        </Card>
      </div>}
      {/* 以租户作为实例键：切换时清除旧租户状态，包括一次性密钥明文。 */}
      {tenantId && <TenantKeys key={tenantId} tenantId={tenantId} editingId={editingId} canEdit={canEdit} tick={tick} />}
    </div>
  );
}

function ownerOf(k: AdminKey, tenant: Tenant, members: Member[] | null): string {
  if (!k.member_id) return tenant.is_internal ? "—" : "租户共用";
  return members?.find((m) => m.id === k.member_id)?.email ?? "成员";
}

/** The models key `k` may call, by name; a selected model the tenant does not reach now is marked. */
function modelsOf(k: AdminKey, tenant: Tenant, names: Map<string, string> | null): string {
  return keyModels(k, tenant, names);
}

function limitsOf(k: AdminKey): string {
  const parts: string[] = [];
  if (k.rate_limit.concurrency !== undefined) parts.push(`并发 ${k.rate_limit.concurrency}`);
  if (k.rate_limit.rpm !== undefined) parts.push(`每分钟 ${k.rate_limit.rpm}`);
  return parts.length > 0 ? parts.join(" · ") : "不限制";
}

function Status({ k, tenant, members }: { k: AdminKey; tenant: Tenant; members: Member[] | null }) {
  const expired = k.expires_at !== null && Date.parse(k.expires_at) <= Date.now();
  return (
    <>
      {k.effective_disabled ? (
        <>
          <Lamp state="disabled">已停用</Lamp>
          <span className="tn-note">{whyOff(k, tenant, members)}</span>
        </>
      ) : expired ? (
        <Lamp state="disabled">已过期</Lamp>
      ) : (
        <Lamp state="applied">正常</Lamp>
      )}
      {k.expires_at && !expired && <span className="tn-note">到期 {fmtTime(k.expires_at)}</span>}
    </>
  );
}

/**
 * Who minted key `k`. A member mints keys for themselves only, so a key of
 * anyone else that a member minted was minted while they managed every key;
 * once they no longer do, the key is worth rotating.
 */
function Creator({ k, members }: { k: AdminKey; members: Member[] | null }) {
  const by = k.created_by;
  if (by.kind === "operator") return <>运维</>;
  if (by.kind !== "member") return <>{by.kind === "system" ? "系统" : by.kind}</>;
  if (members === null) return <>成员</>;
  const m = members.find((x) => x.id === by.id);
  const rotate = k.member_id !== by.id && (!m || !MANAGERS.includes(m.role));
  return (
    <>
      {m ? `${m.email}（${ROLE_LABEL[m.role] ?? m.role}）` : "已移除的成员"}
      {rotate && <span className="tn-note tn-flag">创建者已不再管理密钥，建议轮换</span>}
    </>
  );
}

function TenantKeys({ tenantId, editingId, canEdit, tick }: { tenantId: string; editingId: string | null; canEdit: boolean; tick: number }) {
  const lifetime = useLifetime();
  const [tenant, setTenant] = useState<Tenant | null>(null);
  const [tenantError, setTenantError] = useState<string | null>(null);
  const keyList = useLoad(() => editingId ? keysApi.get(editingId).then(key => key.tenant_id === tenantId ? [key] : []) : tenants.keys(tenantId), [tenantId, editingId, tick]);
  const members = useLoad(() => editingId ? Promise.resolve<Member[]>([]) : tenants.members(tenantId), [tenantId, editingId, tick]);
  const catalog = useLoad(() => resources.list<{ display_name?: string }>("models"), [tick]);
  const groups = useLoad(() => groupsApi.list(), [tick]);
  const [creating, setCreating] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);
  // Held only here: switching tenant or leaving the tab drops it for good.
  const [minted, setMinted] = useState<MintedKey | null>(null);
  const createdResult = useRef<HTMLDivElement>(null);
  useEffect(() => { if (minted && creating) createdResult.current?.focus(); }, [minted, creating]);
  const [editBusy, setEditBusy] = useState(false);
  const [editSaved, setEditSaved] = useState<string | null>(null);
  useEffect(() => setEditSaved(null), [editingId]);
  // A reveal that was refused: shown above the table, beside the row's own
  // errors, never instead of it.
  const [plaintextError, setPlaintextError] = useState<string | null>(null);

  // A view replaces the one shown unless it is older: a read that raced a write.
  const applyView = useCallback(
    (view: Tenant) => setTenant((prev) => (prev && view.version < prev.version ? prev : view)),
    [],
  );

  /** Reads the tenant again and follows its rollout, which covers its keys' documents. */
  const refresh = useCallback(async () => {
    try {
      const view = await tenants.get(tenantId);
      setTenantError(null);
      applyView(view);
      void follow(view, applyView, lifetime());
    } catch (e) {
      setTenantError(errorText(e));
    }
    // `tick`: 刷新 reads the tenant again too.
  }, [tenantId, applyView, lifetime, tick]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // `null` until the catalogue is read, so a key's models can tell "the list
  // is not here yet" from "this model was deleted".
  const names = useMemo(
    () => (catalog.data === null ? null : new Map(catalog.data.map((m) => [m.id, m.doc.display_name || m.id]))),
    [catalog.data],
  );
  const choices = useMemo(() => {
    if (tenant === null) return [];
    const all = groupChoices(groups.data);
    // A customer's key takes one of its tenant's assigned groups; the
    // platform's own tenant has no list to take one from, so it takes any.
    if (tenant.is_internal) return all;
    return all.filter((g) => tenant.allowed_provider_groups.includes(g.name));
  }, [groups.data, tenant]);

  const [keyStatus, setKeyStatus] = useState("");
  const [keyGroup, setKeyGroup] = useState("");
  const table = useRecordTable({ items: (keyList.data ?? []).filter((k) => (!keyStatus || (k.effective_disabled ? "disabled" : "enabled") === keyStatus) && (!keyGroup || k.group?.name === keyGroup)), searchText: (k) => [...keySearchText(k), k.member_id ? members.data?.find((m) => m.id === k.member_id)?.email ?? k.member_id : "租户共用"], resetKey: `${tenantId}:${keyStatus}:${keyGroup}` });

  const saved = (k: AdminKey) => {
    keyList.setData((list) => list && list.map((x) => (x.id === k.id ? k : x)));
    void refresh();
  };
  const listHref = `#keys?tenant=${encodeURIComponent(tenantId)}`;
  const editHref = (id: string) => `${listHref}&key=${encodeURIComponent(id)}`;

  if (!tenant) {
    return (
      <Card>
        <CardHeader number={2} title="租户密钥" />
        {tenantError ? (
          <div className="note crit" role="alert">
            {tenantError}{" "}
            <Button type="button" size="sm" onClick={() => void refresh()}>
              重试
            </Button>
          </div>
        ) : (
          <Loading />
        )}
      </Card>
    );
  }

  if (editingId) {
    const current = keyList.data?.find(key => key.id === editingId);
    return <>
      <div><Button kind="quiet" disabled={editBusy} onClick={() => { window.location.hash = listHref; }}><ArrowLeft aria-hidden />返回密钥列表</Button></div>
      {keyList.error && <div className="note crit" role="alert">{keyList.error}<Button onClick={() => void keyList.reload()}>重新读取</Button></div>}
      {keyList.data === null ? !keyList.error && <Loading /> : !current ? <EmptyState text="未找到该租户的密钥，可能已被吊销。" /> : <>
        <Card className="key-editor-page">
          <CardHeader title={current.label} description={`${tenant.name} · ${current.prefix} · 密钥配置与下方费用额度分别保存。`} />
          {editSaved === current.id && <div className="note" role="status">密钥配置已保存</div>}
          {canEdit ? <CreateForm key={`${current.id}:${current.version}`} initial={current} tenant={tenant} names={names} catalogError={catalog.error} choices={choices} onBusyChange={setEditBusy}
            onSaved={next => { saved(next); setEditSaved(next.id); }} onMinted={() => undefined} onCancel={() => { window.location.hash = listHref; }} onReload={() => void keyList.reload()} /> : <p className="hint">当前账号仅可查看密钥。</p>}
        </Card>
        <KeyBudgets key={current.id} tenantId={tenantId} keyId={current.id} canEdit={canEdit} />
      </>}
    </>;
  }

  const keys = keyList.data;
  const snapshotTime = Date.now();
  const live = keys?.filter((k) => !k.effective_disabled && !(k.expires_at !== null && Date.parse(k.expires_at) <= snapshotTime));
  const disabled = keys?.filter((key) => key.effective_disabled);
  const expired = keys?.filter((key) => !key.effective_disabled && key.expires_at !== null && Date.parse(key.expires_at) <= snapshotTime);
  const open = creating && canEdit && tenant.status !== "pending";
  return (
    <>
      <section className="key-stats" aria-label={`${tenant.name} 的密钥概况`}>
        {([
          ["密钥", keys === null ? "—" : fmtInt(keys.length)],
          ["正常", live ? fmtInt(live.length) : "—"],
          ["已停用", disabled ? fmtInt(disabled.length) : "—"],
          ["已过期（未停用）", expired ? fmtInt(expired.length) : "—"],
        ] as const).map(([label, value]) => (
          <Card key={label} className="key-stat"><CardHeader title={label} /><Metric size="value">{value}</Metric></Card>
        ))}
      </section>
      <div className="keys-workspace">
        <FormDialog open={open} onOpenChange={(next) => { setCreating(next); if (!next) setMinted(null); }} busy={createBusy} title={minted ? "密钥已创建" : "新建密钥"} description="选择分组、模型范围与使用限制。未填写的限制沿用租户上限。">
          {minted ? <div ref={createdResult} tabIndex={-1} aria-label="密钥已创建" className="key-created-result"><MintedNote minted={minted} onClose={() => { setCreating(false); setMinted(null); }} /></div> : <CreateForm tenant={tenant} names={names} catalogError={catalog.error} choices={choices}
            onBusyChange={setCreateBusy}
            onMinted={(m) => { setMinted(m); keyList.setData((list) => list ? [...list, m.key] : [m.key]); void refresh(); }}
            onCancel={() => setCreating(false)} />}
        </FormDialog>
        <Card key="key-list" className="keys-list" aria-label={`${tenant.name} 的密钥`}>
          <CardHeader
            number={open ? 5 : 4}
            className="sm:flex-nowrap [&>div:first-child]:flex-1"
            title={
              <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
                {tenant.name}
                <RolloutBadge rollout={tenant.rollout} />
              </span>
            }
            description={
              <>
                {STATUS_LABEL[tenant.status] ?? tenant.status} · 可用分组 {groupsText(tenant.allowed_provider_groups, tenant)}
                。密钥未设的限额按租户的单 key 上限；运维停用的密钥只有运维能重新启用。
              </>
            }
            actions={
              // A pending tenant has no keys until it is approved; the control plane refuses one.
              canEdit &&
              tenant.status !== "pending" && (
                <Button
                  kind="primary"
                  size="sm"
                  type="button"
                  icon="plus"
                  aria-label="新建密钥"
                  title="新建密钥"
                  aria-expanded={creating}
                  onClick={() => { setMinted(null); setCreating(true); }}
                />
              )
            }
          />
          {tenantError && (
            <div className="note crit" role="alert">
              {tenantError}
            </div>
          )}

          {keyList.error && (
            <div className="note crit" role="alert">
              密钥读取失败：{keyList.error}
            </div>
          )}
          {plaintextError && (
            <div className="note crit" role="alert">
              {plaintextError}
            </div>
          )}
          {members.error && (
            <div className="note crit" role="alert">
              成员读取失败：{members.error}
            </div>
          )}
          {keys === null ? (
            !keyList.error && <Loading />
          ) : keys.length === 0 ? (
            <EmptyState mark="key" text="该租户还没有密钥。" />
          ) : (
            <>
              <RecordTools table={table} label="密钥列表"><label className="f filter"><span>密钥状态</span><Select size="compact" aria-label="密钥状态" value={keyStatus} onValueChange={(value) => setKeyStatus(value)}><option value="">全部</option><option value="enabled">可用</option><option value="disabled">停用</option></Select></label><label className="f filter"><span>密钥分组</span><Select size="compact" aria-label="密钥分组" value={keyGroup} onValueChange={(value) => setKeyGroup(value)}><option value="">全部</option>{choices.map((g) => <option key={g.name} value={g.name}>{g.display_name}</option>)}</Select></label></RecordTools>
              <DataTable label="密钥列表">
                <thead>
                  <tr>
                    <th className="key-identity-column">名称 / 密钥</th>
                    <th className="key-owner-column">归属 / 创建者</th>
                    <th className="key-model-column">模型</th>
                    <th>分组</th>
                    <th className="key-limit-column">调用限制</th>
                    <th>状态</th>
                    <th className="key-created-column">创建时间</th>
                    {canEdit && <th className="key-actions-column right">操作</th>}
                  </tr>
                </thead>
                <tbody>
                  {table.rows.map((k) => (
                      <tr key={k.id}>
                        <td className="key-identity-column"><a className="ops-cell-main key-name" href={editHref(k.id)}>{k.label}</a>
                          <div className="key-secret-line"><KeySecretCell k={k} tenant={tenant} onError={setPlaintextError} /></div>
                        </td>
                        <td className="key-owner-column">
                          <span className="ops-cell-main">{ownerOf(k, tenant, members.data)}</span>
                          <span className="ops-cell-secondary">创建者：<Creator k={k} members={members.data} /></span>
                        </td>
                        <td className="key-model-column">
                          <span className="key-models" title={modelsOf(k, tenant, names)}>
                            {modelsOf(k, tenant, names)}
                          </span>
                        </td>
                        <td>
                          <KeyGroupCell k={k} />
                          <StrayNote k={k} tenant={tenant} />
                        </td>
                        <td className="key-limit-column text-fg-2">{limitsOf(k)}</td>
                        <td>
                          <Status k={k} tenant={tenant} members={members.data} />
                        </td>
                        <td className="key-created-column text-fg-3"><time dateTime={k.created_at}>{fmtTime(k.created_at)}</time></td>
                        {canEdit && (
                          <td className="key-actions-column right">
                            <KeyActions
                              k={k}
                              onEdit={() => { setEditSaved(null); window.location.hash = editHref(k.id); }}
                              onSaved={saved}
                              onRevoked={(gone) => {
                                keyList.setData((list) => list && list.filter((x) => x.id !== gone.id));
                                void refresh();
                              }}
                              onReload={() => void keyList.reload()}
                            />
                          </td>
                        )}
                      </tr>
                  ))}
                </tbody>
              </DataTable>
              <RecordPager table={table} label="密钥列表" />
            </>
          )}
        </Card>
      </div>
    </>
  );
}

/** 平台内部密钥保留前缀与复制；客户租户的明文只由其门户提供。 */
function KeySecretCell({
  k,
  tenant,
  onError,
}: {
  k: AdminKey;
  tenant: Tenant;
  onError: (message: string) => void;
}) {
  if (!tenant.is_internal) return <code>{k.prefix}…</code>;
  return (
    <KeySecret
      prefix={k.prefix}
      className="tn-actions"
      reveal={() => keysApi.reveal(k.id)}
      unavailable={
        k.plaintext_available ? null : "该密钥创建于旧版本，无法再次查看；如需明文请新建密钥"
      }
      onError={(e) => onError(errorText(e))}
    />
  );
}

/** The operator's switch, the override editor's toggle and revocation, each sent with the key's version. */
function KeyActions({
  k,
  onEdit,
  onSaved,
  onRevoked,
  onReload,
}: {
  k: AdminKey;
  onEdit: () => void;
  onSaved: (k: AdminKey) => void;
  onRevoked: (k: AdminKey) => void;
  onReload: () => void;
}) {
  const [busy, setBusy] = useState(false);
  // Which of the row's actions is in flight.
  const [acting, setActing] = useState<"switch" | "revoke" | null>(null);
  const [failed, setFailed] = useState<unknown>(null);

  // A new version of the key (a save, a reload) supersedes what was refused.
  useEffect(() => setFailed(null), [k.version]);

  async function run(what: "switch" | "revoke", action: () => Promise<void>) {
    setBusy(true);
    setActing(what);
    setFailed(null);
    try {
      await action();
    } catch (e) {
      setFailed(e);
    } finally {
      setBusy(false);
      setActing(null);
    }
  }

  const setDisabled = (disabled: boolean) =>
    run("switch", async () => onSaved(await keysApi.update(k.id, { disabled }, k.version)));

  return (
    <>
      <div className="tn-actions ops-table-actions key-row-actions">
        {k.disabled ? (
          <Button
            type="button"
            size="sm"
            kind="quiet"
            iconOnly
            aria-label="启用"
            title="启用"
            disabled={busy}
            busy={acting === "switch"}
            busyLabel="启用"
            onClick={() => void setDisabled(false)}
          >
            <Power aria-hidden="true" />
          </Button>
        ) : (
          <TwoStep
            label="停用"
            kind="quiet"
            icon={<PowerOff aria-hidden="true" />}
            size="sm"
            disabled={busy}
            busy={acting === "switch"}
            warning="停用后该密钥立即失效；租户不能自行重新启用。"
            onConfirm={() => void setDisabled(true)}
          />
        )}
        <Button type="button" size="sm" kind="quiet" iconOnly aria-label="编辑密钥" title="编辑密钥" disabled={busy} onClick={onEdit}><Pencil aria-hidden="true" /></Button>
        <TwoStep
          label="吊销"
          kind="quiet"
          icon={<Trash2 aria-hidden="true" />}
          size="sm"
          disabled={busy}
          busy={acting === "revoke"}
          warning="吊销后该密钥永久失效，不能恢复。"
          onConfirm={() =>
            void run("revoke", async () => {
              await keysApi.revoke(k.id, k.version);
              onRevoked(k);
            })
          }
        />
      </div>
      {failed !== null && <Failure error={failed} onReload={onReload} />}
    </>
  );
}

/** 新建密钥始终以遮罩呈现，明文仅交给复制回调。 */
function MintedNote({ minted, onClose }: { minted: MintedKey; onClose: () => void }) {
  const [copied, setCopied] = useState<string | null>(null);

  return (
    <div className="note warn" role="region" aria-label="密钥已创建">
      <strong>密钥「{minted.key.label}」已创建。</strong>现在可以复制并交给使用方；之后也能在密钥列表里再次复制，关闭本提示不影响。
      <div className="mt-3 flex flex-wrap items-start gap-2">
        <code className="tn-token" aria-label="密钥前缀">
          {minted.key.prefix}…
        </code>
        <div className="tn-actions">
          <CopyButton text={minted.plaintext} onFailed={() => setCopied("无法写入剪贴板，请允许剪贴板权限后重试。")} />
          <Button type="button" onClick={onClose}>
            关闭
          </Button>
        </div>
      </div>
      {copied && <p className="hint mt-2">{copied}</p>}
    </div>
  );
}

function TextField({
  label,
  value,
  onChange,
  type = "text",
  placeholder,
  hint,
  error,
  disabled = false,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  type?: string;
  placeholder?: string;
  hint?: string;
  error?: string;
  disabled?: boolean;
}) {
  const errorId = useId();
  return (
    <div className="field">
      <label className="f">
        <span>{label}</span>
        <input
          type={type}
          disabled={disabled}
          value={value}
          placeholder={placeholder}
          onChange={(e) => onChange(e.target.value)}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? errorId : undefined}
        />
      </label>
      {hint && <p className="hint">{hint}</p>}
      {error && (
        <span id={errorId} className="tn-error" role="alert">
          {error}
        </span>
      )}
    </div>
  );
}

/** 创建与编辑共用字段的拒绝定位。 */
function placedInForm(path: string): boolean {
  return path === "/label" || atList("/allowed_models")(path) || atList("/model_ids")(path) || path === "/group_id" ||
    path === "/rate_limit/concurrency" || path === "/rate_limit/rpm" || path === "/expires_at" || atList("/budget")(path);
}

type BudgetCycle = "cumulative" | "day" | "week" | "month";
const CYCLE_NAMES: Record<BudgetCycle, string> = { cumulative: "累计", day: "每日", week: "每周", month: "每月" };
function budgetBody(amount: string, cycle: BudgetCycle): NewKeyBudget {
  return cycle === "cumulative" ? { limit_cny: amount, kind: "cumulative" } : { limit_cny: amount, kind: "periodic", period: cycle };
}

function BudgetFields({ amount, onAmount, cycle, onCycle, busy, error }: {
  amount: string; onAmount: (value: string) => void; cycle: BudgetCycle; onCycle: (value: BudgetCycle) => void; busy: boolean; error?: string;
}) {
  return <div className="key-budget-create col-span-full grid gap-4 sm:grid-cols-2">
    <TextField label="费用额度（元）" value={amount} onChange={onAmount} disabled={busy} placeholder="不限制" hint="留空不设置费用额度；支持最多两位小数。" error={error} />
    <Field label="额度周期" hint="周期额度按 UTC 日历结算，累计额度从设置时起计算。">{aria => <Select {...aria} value={cycle} disabled={busy} onValueChange={value => onCycle(value as BudgetCycle)}>{Object.entries(CYCLE_NAMES).map(([value, name]) => <option key={value} value={value}>{name}</option>)}</Select>}</Field>
  </div>;
}

function CreateForm({ tenant, names, catalogError, choices, onMinted, onCancel, onBusyChange, initial, onSaved, onReload }: {
  tenant: Tenant; names: Map<string, string> | null; catalogError: string | null; choices: GroupChoice[];
  onMinted: (minted: MintedKey) => void; onCancel: () => void; onBusyChange: (busy: boolean) => void;
  initial?: AdminKey; onSaved?: (key: AdminKey) => void; onReload?: () => void;
}) {
  const [label, setLabel] = useState(initial?.label ?? "");
  const initialPatterns = initial?.allowed_models?.join(", ") ?? "";
  const [patterns, setPatterns] = useState(initialPatterns);
  const [modelScope, setModelScope] = useState<"inherited" | "selected" | "patterns">(initial?.allowed_models !== null && initial?.allowed_models !== undefined ? "patterns" : initial?.model_ids !== null && initial?.model_ids !== undefined ? "selected" : "inherited");
  const [models, setModels] = useState<string[]>(initial?.model_ids ?? []);
  const [modelSearch, setModelSearch] = useState("");
  const [group, setGroup] = useState(() => initial?.group?.name ?? (choices.length === 1 && choices[0] ? choices[0].name : ""));
  const [concurrency, setConcurrency] = useState(initial?.rate_limit.concurrency?.toString() ?? "");
  const [rpm, setRpm] = useState(initial?.rate_limit.rpm?.toString() ?? "");
  const initialExpires = initial?.expires_at ? beijingInputValue(new Date(initial.expires_at)) : "";
  const [expires, setExpires] = useState(initialExpires);
  const [disabled, setDisabled] = useState(initial?.disabled ?? false);
  const [amount, setAmount] = useState("");
  const [cycle, setCycle] = useState<BudgetCycle>("cumulative");
  const [sent, setSent] = useState<NewKey | null>(null);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [busy, setBusy] = useState(false);
  const [part, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const grants = useMemo(() => tenant.effective_model_ids.map(id => ({ id, name: names?.get(id) ?? id })).sort((a, b) => a.name.localeCompare(b.name)), [tenant.effective_model_ids, names]);
  const grantedIds = useMemo(() => new Set(grants.map(model => model.id)), [grants]);
  const selectedIds = useMemo(() => new Set(models), [models]);
  const visible = grants.filter(model => model.name.toLocaleLowerCase().includes(modelSearch.trim().toLocaleLowerCase()));
  const unavailable = names === null ? [] : models.filter(id => !grantedIds.has(id));
  const at = (path: string) => refused?.at.get(path);
  const patternsError = [...(refused?.at ?? [])].filter(([path]) => atList("/allowed_models")(path)).map(([, message]) => message).join("；");

  async function submit() {
    const problems = new Map<string, string>();
    if (!initial && modelScope === "selected" && models.length === 0) problems.set("/model_ids", "请选择至少一个模型，或改为不限制模型");
    if (!initial && modelScope === "patterns" && patterns.trim() === "") problems.set("/allowed_models", "请填写模型名称模式，或改为不限制模型");
    for (const [name, value] of [["concurrency", concurrency], ["rpm", rpm]] as const) {
      if (value.trim() !== "" && (!/^\d+$/.test(value.trim()) || Number(value) < 1 || Number(value) > 4_294_967_295)) problems.set(`/rate_limit/${name}`, "请输入大于零的有效整数，留空沿用租户限制");
    }
    const expiresAt = initial && expires === initialExpires ? initial.expires_at : expires === "" ? null : parseBeijingInput(expires);
    if (expires !== "" && expiresAt === null) problems.set("/expires_at", "请填写有效的时间");
    if (!initial && amount.trim() && !isKeyBudgetAmount(amount.trim())) problems.set("/budget/limit_cny", "请输入大于零的金额，最多两位小数且不超过额度上限");
    if (problems.size) { setRefused({ conflict: false, at: problems, general: null, tooOld: null }); focusRefusal(); return; }
    const limits: NonNullable<NewKey["rate_limit"]> = {};
    if (concurrency.trim()) limits.concurrency = Number(concurrency);
    if (rpm.trim()) limits.rpm = Number(rpm);
    const selectedPatterns = initial?.allowed_models && patterns === initialPatterns ? initial.allowed_models : patterns.split(/[\s,，、]+/).filter(Boolean);
    const body: NewKey = { label };
    if (group) body.group_id = initial?.group?.id ?? idOf(choices, group);
    if (modelScope === "selected") body.model_ids = models;
    if (modelScope === "patterns") body.allowed_models = selectedPatterns;
    if (limits.concurrency !== undefined || limits.rpm !== undefined) body.rate_limit = limits;
    if (expiresAt !== null) body.expires_at = expiresAt;
    if (!initial && amount.trim()) body.budget = budgetBody(amount.trim(), cycle);
    setBusy(true); onBusyChange(true); setRefused(null); setSent(body);
    try {
      if (initial) {
        const patch = adminKeySettingsPatch(initial, { label, scope: modelScope, modelIds: models, patterns: selectedPatterns, groupId: body.group_id ?? "", rateLimit: limits, disabled, expiresAt });
        onSaved?.(Object.keys(patch).length ? await keysApi.update(initial.id, patch, initial.version) : initial);
      } else onMinted(await keysApi.create(tenant.id, body));
    } catch (error) { setRefused(refusal(error, placedInForm)); focusRefusal(); }
    finally { setBusy(false); onBusyChange(false); }
  }

  return <form ref={part} className="tn-create key-create-form" onSubmit={event => { event.preventDefault(); void submit(); }}>
    <TextField label="名称" value={label} onChange={setLabel} disabled={busy} error={at("/label")} />
    <fieldset className="tn-field col-span-full">
      <legend>分组</legend>
      {initial?.group ? <><p>{initial.group.display_name}</p><p className="hint">分组创建后不能更改；需要其他分组时请新建密钥。</p></> : choices.length === 0 ? <p className="hint">该租户没有可用分组；先在租户页设置分组。</p> : <label className="f"><span>分组（必选）</span><Select value={group} disabled={busy} onValueChange={setGroup}><option value="">请选择分组</option>{choices.map(choice => <option key={choice.id} value={choice.name}>{choice.display_name}{choice.display_name === choice.name ? "" : `（${choice.name}）`}</option>)}</Select></label>}
      {at("/group_id") && <p className="tn-error" role="alert">{at("/group_id")}</p>}
    </fieldset>
    <fieldset className="tn-field key-model-picker col-span-full" aria-invalid={at("/model_ids") || patternsError ? true : undefined}>
      <legend>模型</legend>
      <label className="f"><span>模型范围</span><Select value={modelScope} disabled={busy} onValueChange={value => setModelScope(value as typeof modelScope)}><option value="inherited">不限制模型</option><option value="selected">指定模型</option>{tenant.is_internal && <option value="patterns">名称模式（高级）</option>}</Select></label>
      <p className="hint">不限制模型时使用租户当前及之后授权的模型；指定模型时只使用勾选项。</p>
      {catalogError && <p className="note crit" role="alert">模型目录读取失败：{catalogError}</p>}
      {modelScope === "selected" && <>
        <TextField label="搜索模型" value={modelSearch} onChange={setModelSearch} disabled={busy} placeholder="按模型名称搜索" />
        {grants.length === 0 ? <p className="hint">该租户还没有授权模型，请先在租户页授权。</p> : visible.length === 0 ? <p className="hint">没有匹配的模型</p> : <ul className="tn-checks tn-box key-model-options">{visible.map(model => <Check key={model.id} label={model.name} checked={selectedIds.has(model.id)} disabled={busy} onChange={checked => setModels(previous => checked ? [...previous, model.id] : previous.filter(id => id !== model.id))} error={at(`/model_ids/${sent?.model_ids?.indexOf(model.id) ?? -1}`)} />)}</ul>}
        {unavailable.length > 0 && <><p className="hint">以下已选模型已不在当前授权中，可取消勾选后保存：</p><ul className="tn-checks key-model-options">{unavailable.map(id => <Check key={id} label={`${names?.get(id) ?? id}（已不可用）`} checked disabled={busy} onChange={() => setModels(previous => previous.filter(value => value !== id))} />)}</ul></>}
        {models.length === 0 && initial && <p className="hint">未选择任何模型，此密钥将不能调用模型。</p>}
      </>}
      {modelScope === "patterns" && <TextField label="模型名称模式" value={patterns} onChange={setPatterns} disabled={busy} placeholder="例如 gpt-4o*, claude-*" hint="多个模式用逗号或空格分开。留空将不授权任何模型；不限制请选择上方选项。" error={patternsError || undefined} />}
      {at("/model_ids") && <p className="tn-error" role="alert">{at("/model_ids")}</p>}
    </fieldset>
    <div className="tn-limits col-span-full grid gap-x-4 sm:grid-cols-2"><LimitInput label="并发" value={concurrency} disabled={busy} onChange={setConcurrency} error={at("/rate_limit/concurrency")} /><LimitInput label="每分钟请求" value={rpm} disabled={busy} onChange={setRpm} error={at("/rate_limit/rpm")} /></div>
    <TextField label="过期时间" type="datetime-local" value={expires} onChange={setExpires} disabled={busy} hint="留空表示不过期；时间采用 UTC+8。" error={at("/expires_at")} />
    {initial ? <Field label="平台状态" hint={initial.disabled_by_tenant ? "该密钥同时被租户停用，平台启用不会解除租户的停用。" : undefined}>{aria => <Select {...aria} value={disabled ? "disabled" : "enabled"} disabled={busy} onValueChange={value => setDisabled(value === "disabled")}><option value="enabled">启用</option><option value="disabled">停用</option></Select>}</Field> : <BudgetFields amount={amount} onAmount={setAmount} cycle={cycle} onCycle={setCycle} busy={busy} error={at("/budget/limit_cny")} />}
    {refused?.conflict && <div className="note crit col-span-full" role="alert">该密钥已被其他人修改，请重新载入最新配置后再保存。<Button type="button" onClick={onReload}>重新载入配置</Button></div>}
    {refused?.tooOld ? <GatewayTooOld error={refused.tooOld} /> : refused?.general && <div className="note crit col-span-full" role="alert">{refused.general}</div>}
    <div className="tn-foot right col-span-full">
      <Button type="button" disabled={busy} onClick={onCancel}>{initial ? "返回列表" : "取消"}</Button>
      {initial ? <Button kind="primary" type="submit" disabled={busy} busy={busy}>保存配置</Button> : <TwoStep label="创建" kind="primary" disabled={busy || group === ""} busy={busy} ask={() => group !== "" && !(choices.find(choice => choice.name === group)?.accounts.length ?? 0)} warning={unreachableWarning(choices.filter(choice => choice.name === group).map(choice => choice.display_name), "这个密钥")} onConfirm={() => void submit()} />}
    </div>
  </form>;
}

function KeyBudgets({ tenantId, keyId, canEdit }: { tenantId: string; keyId: string; canEdit: boolean }) {
  const loaded = useLoad(() => billing.budgets(tenantId), [tenantId, keyId]);
  const [amount, setAmount] = useState("");
  const [cycle, setCycle] = useState<BudgetCycle>("cumulative");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const rows = (loaded.data ?? []).filter(row => row.scope === "api_key" && row.target.id === keyId);
  async function add(event: React.FormEvent) {
    event.preventDefault(); setError(null); setNotice(null);
    if (!isKeyBudgetAmount(amount.trim())) { setError("请输入大于零的费用额度，最多两位小数"); return; }
    setBusy(true);
    try { await createKeyQuota(tenantId, keyId, budgetBody(amount.trim(), cycle)); setAmount(""); setNotice("费用额度已添加"); await loaded.reload(); }
    catch (failure) { setError(errorText(failure)); }
    finally { setBusy(false); }
  }
  return <Card className="key-budget-editor">
    <CardHeader title="费用额度" description="费用额度在这里单独保存；组织、成员与密钥的各项额度仍同时生效。" />
    {loaded.error && <div className="note crit" role="alert">{loaded.error}<Button onClick={() => void loaded.reload()}>重新读取额度</Button></div>}
    {loaded.data === null ? <Loading /> : <>
      {rows.length === 0 && <p className="hint">该密钥未设置独立费用额度。</p>}
      {rows.map(row => <KeyBudgetRow key={`${row.id}:${row.limit_micro_cny}`} row={row} tenantId={tenantId} canEdit={canEdit} onChanged={() => void loaded.reload()} />)}
      {notice && <p role="status">{notice}</p>}
      {canEdit && <form onSubmit={event => void add(event)} aria-label="添加密钥费用额度" className="grid gap-4">
        <BudgetFields amount={amount} onAmount={setAmount} cycle={cycle} onCycle={setCycle} busy={busy} />
        {error && <p className="note crit" role="alert">{error}</p>}
        <div><Button kind="primary" type="submit" busy={busy} disabled={busy}>添加额度</Button></div>
      </form>}
    </>}
  </Card>;
}

function KeyBudgetRow({ row, tenantId, canEdit, onChanged }: { row: Budget; tenantId: string; canEdit: boolean; onChanged: () => void }) {
  const exact = Number.isSafeInteger(row.limit_micro_cny);
  const [amount, setAmount] = useState(() => exact ? keyBudgetAmountInput(row.limit_micro_cny) : "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cycle = row.kind === "cumulative" ? "cumulative" : row.period!;
  const title = `${CYCLE_NAMES[cycle]}额度`;
  const editable = canEdit && row.set_by === "operator";
  async function change(remove: boolean) {
    setError(null);
    if (!remove && !isKeyBudgetAmount(amount.trim())) { setError("请输入大于零的额度，最多两位小数"); return; }
    setBusy(true);
    try { if (remove) await deleteOrgQuota(tenantId, row.id); else await updateOrgQuota(tenantId, row.id, amount.trim()); onChanged(); }
    catch (failure) { setError(errorText(failure)); }
    finally { setBusy(false); }
  }
  return <form className="key-budget-row grid gap-3 sm:grid-cols-2" aria-label={title} onSubmit={event => { event.preventDefault(); void change(false); }}>
    <TextField label={`${title}（元）`} value={amount} onChange={setAmount} disabled={!editable || busy || !exact} hint={row.set_by === "operator" ? "由平台设置" : "由租户或成员设置，平台不覆盖此额度"} />
    <p className="hint">已使用 {Number.isSafeInteger(row.spent_micro_cny) ? fmtMicroCny(row.spent_micro_cny) : "金额超出浏览器精确范围"}{row.exhausted ? " · 额度已用尽" : ""}</p>
    {!exact && <p className="note" role="status">该额度超出浏览器精确编辑范围，请通过 API 读取与修改原值。</p>}
    {error && <p className="note crit col-span-full" role="alert">{error}</p>}
    {editable && <div className="tn-actions col-span-full"><Button type="submit" disabled={busy || !exact} busy={busy}>保存额度</Button><TwoStep label="移除额度" kind="quiet" disabled={busy} busy={busy} warning="移除后不再使用这一项费用额度，其他额度仍然生效。" onConfirm={() => void change(true)} /></div>}
  </form>;
}
