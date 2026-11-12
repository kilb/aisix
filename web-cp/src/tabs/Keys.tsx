import { parseBeijingInput } from "../lib/time";
import { keySearchText } from "../lib/operationalRecords";
import { useRecordTable } from "../lib/recordTable";
import { RecordTools, RecordPager } from "../components/RecordTools";
/** 布局：[01 租户范围 12]；[02 密钥 6][03 正常 6]；[列表 12 / 创建时表单 5 + 列表 7]。 */
import { Fragment, useCallback, useEffect, useId, useMemo, useState } from "react";
import type { TabProps } from "../App";
import { GatewayTooOld } from "../components/GatewayTooOld";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  groups as groupsApi,
  keys as keysApi,
  resources,
  tenants,
  type AdminKey,
  type Member,
  type MintedKey,
  type NewKey,
  type Tenant,
} from "../lib/api";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { keyModels } from "../lib/keyModels";
import {
  atList,
  Check,
  ChooseGroup,
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
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, ChartSkeleton, Metric, MetricSkeleton, PageHeader, span } from "../components/page";
import { RankBars } from "../components/charts";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";
import { CopyButton } from "../ui/CopyButton";
import { KeySecret } from "../ui/KeySecret";
import { DataTable, Loading } from "../ui/DataTable";
import { EmptyState } from "../ui/EmptyState";
import { Lamp } from "../ui/Lamp";
import { TwoStep } from "../ui/TwoStep";

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

export function Keys({ canEdit }: TabProps) {
  const list = useLoad(() => tenants.list(), []);
  const ordered = useMemo(() => [...(list.data ?? [])].sort(byPicker), [list.data]);
  const [chosen, setChosen] = useState<string | null>(null);
  const tenantId = chosen ?? ordered[0]?.id ?? null;
  // Bumped by 刷新: the tenant shown reads its lists again.
  const [tick, setTick] = useState(0);

  return (
    <>
      {/* 桌面与平板：范围 12；概况 6 + 6；密钥表单与明细保留独立状态。 */}
      <PageHeader
        title="密钥"
        eyebrow="客户管理"
        dataLead={list.data === null ? "读取当前可管理的租户。" : `共有 ${fmtInt(ordered.length)} 个租户可查看；统计只包含当前选择的租户。`}
        description="平台内部租户的密钥是运维自用的，可以按名称模式授权模型；其他租户的密钥只能使用该租户已授权的模型。平台内部租户的密钥明文可随时复制；客户租户的密钥在这里只显示前缀，明文由该租户在自己的门户里复制。"
        actions={
          <Button
            type="button"
            icon="refresh"
            onClick={() => {
              void list.reload();
              setTick((n) => n + 1);
            }}
          >
            刷新
          </Button>
        }
      />
      <BentoGrid className="mb-3">
        <Card className={span.d12}>
          <CardHeader number={1} title="租户范围" description="平台内部密钥可按名称模式授权模型，明文可随时复制；客户密钥只能使用租户已授权的模型，在这里仅显示前缀，明文由客户在自己的门户复制。切换租户会清除当前显示的新密钥明文。" />
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
                <select value={tenantId ?? ""} onChange={(e) => setChosen(e.target.value)}>
                  {ordered.map((t) => (
                    <option key={t.id} value={t.id}>
                      {tenantLabel(t)}
                    </option>
                  ))}
                </select>
              </label>
            </div>
          )}
        </Card>
      </BentoGrid>
      {/* 以租户作为实例键：切换时清除旧租户状态，包括一次性密钥明文。 */}
      {tenantId && <TenantKeys key={tenantId} tenantId={tenantId} canEdit={canEdit} tick={tick} />}
    </>
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
  return parts.length > 0 ? parts.join(" · ") : "未设";
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

function TenantKeys({ tenantId, canEdit, tick }: { tenantId: string; canEdit: boolean; tick: number }) {
  const lifetime = useLifetime();
  const [tenant, setTenant] = useState<Tenant | null>(null);
  const [tenantError, setTenantError] = useState<string | null>(null);
  const keyList = useLoad(() => tenants.keys(tenantId), [tenantId, tick]);
  const members = useLoad(() => tenants.members(tenantId), [tenantId, tick]);
  const catalog = useLoad(() => resources.list<{ display_name?: string }>("models"), [tick]);
  const groups = useLoad(() => groupsApi.list(), [tick]);
  const [creating, setCreating] = useState(false);
  // Held only here: switching tenant or leaving the tab drops it for good.
  const [minted, setMinted] = useState<MintedKey | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
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

  const columns = canEdit ? 9 : 8;
  const keys = keyList.data;
  const snapshotTime = Date.now();
  const live = keys?.filter((k) => !k.effective_disabled && !(k.expires_at !== null && Date.parse(k.expires_at) <= snapshotTime));
  const disabled = keys?.filter((key) => key.effective_disabled);
  const expired = keys?.filter((key) => !key.effective_disabled && key.expires_at !== null && Date.parse(key.expires_at) <= snapshotTime);
  const stateItems = keys === null ? null : [
    { label: "正常", value: live?.length ?? 0, displayValue: fmtInt(live?.length ?? 0) },
    { label: "已停用", value: disabled?.length ?? 0, displayValue: fmtInt(disabled?.length ?? 0) },
    { label: "已过期（未停用）", value: expired?.length ?? 0, displayValue: fmtInt(expired?.length ?? 0) },
  ].sort((a, b) => b.value - a.value);
  const open = creating && canEdit && tenant.status !== "pending";
  return (
    <>
      <BentoGrid className="mb-3" aria-label={`${tenant.name} 的密钥概况`}>
        <Card className={span.d6}>
          <CardHeader number={2} title="密钥" description={keyList.error ? "刷新失败，保留最近成功读取的当前租户密钥" : `${tenant.name} 的完整密钥列表`} />
          {keys === null && !keyList.error ? <MetricSkeleton /> : <Metric size="hero">{keys === null ? "—" : fmtInt(keys.length)}</Metric>}
          <CardFooter>
            {stateItems === null ? (keyList.error ? <CardMessage onRetry={() => void keyList.reload()}>密钥统计读取失败。</CardMessage> : <ChartSkeleton />) : (
              <RankBars items={stateItems} accessibleLabel={`当前租户共有 ${fmtInt(keys?.length ?? 0)} 把密钥；正常 ${fmtInt(live?.length ?? 0)} 把，已停用 ${fmtInt(disabled?.length ?? 0)} 把，未停用但已过期 ${fmtInt(expired?.length ?? 0)} 把。`} emptyLabel="当前租户没有密钥。" />
            )}
          </CardFooter>
        </Card>
        <Card className={span.d6} tone="inverse">
          <CardHeader number={3} title="正常" description={keyList.error ? "读取失败；已有值来自最近成功读取" : "已停用与已过期的密钥均不计入"} />
          {keys === null && !keyList.error ? <MetricSkeleton /> : <Metric size="display">{live ? fmtInt(live.length) : "—"}</Metric>}
          <CardFooter>
            <dl className="grid grid-cols-2 gap-6 border-t border-surface/12 pt-5">
              <div><dt className="font-mono text-meta text-on-inverse-muted">已停用</dt><dd className="mt-2"><Metric size="value">{disabled ? fmtInt(disabled.length) : "—"}</Metric></dd></div>
              <div><dt className="font-mono text-meta text-on-inverse-muted">已过期（未停用）</dt><dd className="mt-2"><Metric size="value">{expired ? fmtInt(expired.length) : "—"}</Metric></dd></div>
            </dl>
          </CardFooter>
        </Card>
      </BentoGrid>
      <BentoGrid className={open ? "is-creating" : undefined}>
        {open && (
          <Card key="key-create" className={span.d5Wide}>
            <CardHeader number={4} title="新建密钥" />
            <CreateForm
              tenant={tenant}
              names={names}
              catalogError={catalog.error}
              choices={choices}
              onMinted={(m) => {
                setMinted(m);
                setCreating(false);
                keyList.setData((list) => (list ? [...list, m.key] : [m.key]));
                void refresh();
              }}
              onCancel={() => setCreating(false)}
            />
          </Card>
        )}
        <Card key="key-list" className={open ? span.d7 : span.d12} aria-label={`${tenant.name} 的密钥`}>
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
                  aria-expanded={creating}
                  onClick={() => setCreating((o) => !o)}
                >
                  新建密钥
                </Button>
              )
            }
          />
          {tenantError && (
            <div className="note crit" role="alert">
              {tenantError}
            </div>
          )}
          {minted && <MintedNote minted={minted} onClose={() => setMinted(null)} />}

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
              <RecordTools table={table} label="密钥列表"><label className="f filter"><span>密钥状态</span><select aria-label="密钥状态" value={keyStatus} onChange={(e) => setKeyStatus(e.target.value)}><option value="">全部</option><option value="enabled">可用</option><option value="disabled">停用</option></select></label><label className="f filter"><span>密钥分组</span><select aria-label="密钥分组" value={keyGroup} onChange={(e) => setKeyGroup(e.target.value)}><option value="">全部</option>{choices.map((g) => <option key={g.name} value={g.name}>{g.display_name}</option>)}</select></label></RecordTools>
              <DataTable label="密钥列表">
                <thead>
                  <tr>
                    <th>名称</th>
                    <th>前缀</th>
                    <th className="side-opt">成员</th>
                    <th>模型</th>
                    <th>分组</th>
                    <th className="side-opt">限额</th>
                    <th>状态</th>
                    <th className="side-opt">创建者</th>
                    {canEdit && <th className="right">操作</th>}
                  </tr>
                </thead>
                <tbody>
                  {table.rows.map((k) => (
                    <Fragment key={k.id}>
                      <tr>
                        <td className="font-medium">{k.label}</td>
                        <td>
                          <KeySecretCell k={k} tenant={tenant} onError={setPlaintextError} />
                        </td>
                        <td className="side-opt text-text-2">{ownerOf(k, tenant, members.data)}</td>
                        <td>
                          <span className="block max-w-[280px] truncate" title={modelsOf(k, tenant, names)}>
                            {modelsOf(k, tenant, names)}
                          </span>
                        </td>
                        <td>
                          <KeyGroupCell k={k} />
                          <StrayNote k={k} tenant={tenant} />
                        </td>
                        <td className="side-opt text-text-2">{limitsOf(k)}</td>
                        <td>
                          <Status k={k} tenant={tenant} members={members.data} />
                        </td>
                        <td className="side-opt text-text-2">
                          <Creator k={k} members={members.data} />
                        </td>
                        {canEdit && (
                          <td className="right">
                            <KeyActions
                              k={k}
                              editing={editing === k.id}
                              onEdit={() => setEditing(editing === k.id ? null : k.id)}
                              onSaved={saved}
                              onRevoked={(gone) => {
                                if (editing === gone.id) setEditing(null);
                                keyList.setData((list) => list && list.filter((x) => x.id !== gone.id));
                                void refresh();
                              }}
                              onReload={() => void keyList.reload()}
                            />
                          </td>
                        )}
                      </tr>
                      {editing === k.id && (
                        <tr className="dt-detail">
                          <td colSpan={columns}>
                            <ChooseGroup
                              k={k}
                              choices={choices}
                              onSaved={(next) => {
                                setEditing(null);
                                saved(next);
                              }}
                              onCancel={() => setEditing(null)}
                              onReload={() => {
                                setEditing(null);
                                void keyList.reload();
                              }}
                            />
                          </td>
                        </tr>
                      )}
                    </Fragment>
                  ))}
                </tbody>
              </DataTable>
              <RecordPager table={table} label="密钥列表" />
            </>
          )}
        </Card>
      </BentoGrid>
    </>
  );
}

/**
 * A key's prefix, and, for the platform's own tenant, its plaintext: `复制`
 * reveals and copies it, and `显示` puts it in place of the prefix. A
 * customer tenant's key shows the prefix alone: operators mint the platform's
 * own keys and must be able to copy one again, while a customer's key is the
 * customer's to reveal in its own portal.
 */
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
  editing,
  onEdit,
  onSaved,
  onRevoked,
  onReload,
}: {
  k: AdminKey;
  editing: boolean;
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
      <div className="tn-actions justify-end">
        {k.disabled ? (
          <Button
            type="button"
            size="sm"
            disabled={busy}
            busy={acting === "switch"}
            busyLabel="启用"
            onClick={() => void setDisabled(false)}
          >
            启用
          </Button>
        ) : (
          <TwoStep
            label="停用"
            size="sm"
            disabled={busy}
            busy={acting === "switch"}
            warning="停用后该密钥立即失效；租户不能自行重新启用。"
            onConfirm={() => void setDisabled(true)}
          />
        )}
        {/* A legacy key minted without a group takes one once; one that has a group never changes it. */}
        {k.group === null && (
          <Button type="button" size="sm" aria-expanded={editing} onClick={onEdit}>
            选择分组
          </Button>
        )}
        <TwoStep
          label="吊销"
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

/** A newly minted key's plaintext, the one time it exists outside the caller's hands. */
function MintedNote({ minted, onClose }: { minted: MintedKey; onClose: () => void }) {
  const [copied, setCopied] = useState<string | null>(null);

  return (
    <div className="note warn" role="region" aria-label="密钥已创建">
      <strong>密钥「{minted.key.label}」已创建。</strong>现在可以复制并交给使用方；之后也能在密钥列表里随时点「复制」再次获取明文，关闭本提示不影响。
      <div className="mt-3 flex flex-wrap items-start gap-2">
        <code className="tn-token" aria-label="密钥明文">
          {minted.plaintext}
        </code>
        <div className="tn-actions">
          <CopyButton text={minted.plaintext} onFailed={() => setCopied("无法写入剪贴板，请手动选中复制")} />
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
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  type?: string;
  placeholder?: string;
  hint?: string;
  error?: string;
}) {
  const errorId = useId();
  return (
    <div className="field">
      <label className="f">
        <span>{label}</span>
        <input
          type={type}
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

/** Every path of a create body this form shows a refusal at. */
function placedInForm(path: string): boolean {
  return (
    path === "/label" ||
    atList("/allowed_models")(path) ||
    /^\/model_ids\/\d+$/.test(path) ||
    path === "/group_id" ||
    path === "/rate_limit/concurrency" ||
    path === "/rate_limit/rpm" ||
    path === "/expires_at"
  );
}

function CreateForm({
  tenant,
  names,
  catalogError,
  choices,
  onMinted,
  onCancel,
}: {
  tenant: Tenant;
  /** `null` until the catalogue is read: a key's models tell "not loaded" from "deleted" by it. */
  names: Map<string, string> | null;
  catalogError: string | null;
  /** The groups a key of this tenant may take: its own for a customer, every live one for the platform's. */
  choices: GroupChoice[];
  onMinted: (minted: MintedKey) => void;
  onCancel: () => void;
}) {
  const [label, setLabel] = useState("");
  const [patterns, setPatterns] = useState("");
  const [models, setModels] = useState<string[]>([]);
  // One group, required. A tenant with a single group has it chosen already.
  const [group, setGroup] = useState<string>(() => (choices.length === 1 && choices[0] ? choices[0].name : ""));
  const [concurrency, setConcurrency] = useState("");
  const [rpm, setRpm] = useState("");
  const [expires, setExpires] = useState("");
  // The body last sent: a refusal names a list entry by its index in it.
  const [sent, setSent] = useState<NewKey | null>(null);
  const [refused, setRefused] = useState<Refusal | null>(null);
  const [busy, setBusy] = useState(false);
  const [part, focusRefusal] = useRefusalFocus<HTMLFieldSetElement>();

  const grants = useMemo(
    () =>
      // What the tenant reaches now, which is what the control plane checks a
      // key's selection against: every live public model under `all`, never
      // the stored list that scope keeps aside.
      tenant.effective_model_ids
        // A name missing here is a model the catalogue has not loaded yet;
        // the id is the honest fallback.
        .map((id) => ({ id, name: names?.get(id) ?? id }))
        .sort((a, b) => a.name.localeCompare(b.name)),
    [tenant.effective_model_ids, names],
  );

  /** The create body: every field left blank is left out. */
  function body(): NewKey {
    const b: NewKey = { label };
    if (tenant.is_internal) {
      const list = patterns.split(/[\s,，、]+/).filter(Boolean);
      if (list.length > 0) b.allowed_models = list;
    } else {
      const ids = grants.filter((m) => models.includes(m.id)).map((m) => m.id);
      if (ids.length > 0) b.model_ids = ids;
    }
    if (group !== "") b.group_id = idOf(choices, group);
    const limit: NonNullable<NewKey["rate_limit"]> = {};
    if (concurrency.trim() !== "") limit.concurrency = Number(concurrency);
    if (rpm.trim() !== "") limit.rpm = Number(rpm);
    if (limit.concurrency !== undefined || limit.rpm !== undefined) b.rate_limit = limit;
    if (expires !== "") b.expires_at = parseBeijingInput(expires) ?? undefined;
    return b;
  }

  async function submit() {
    if (expires !== "" && parseBeijingInput(expires) === null) {
      setRefused(refusal(new Error("请填写有效的时间"), placedInForm));
      focusRefusal();
      return;
    }
    const b = body();
    setBusy(true);
    setRefused(null);
    setSent(b);
    try {
      onMinted(await keysApi.create(tenant.id, b));
    } catch (e) {
      setRefused(refusal(e, placedInForm));
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const at = (path: string) => refused?.at.get(path);
  const patternsError = [...(refused?.at ?? [])]
    .filter(([path]) => atList("/allowed_models")(path))
    .map(([, message]) => message)
    .join("；");

  return (
    <fieldset ref={part} className="tn-sub tn-create">
      <legend className="sr-only">新建密钥</legend>
      <TextField label="名称" value={label} onChange={setLabel} error={at("/label")} />
      {tenant.is_internal ? (
        <TextField
          label="模型（可写通配）"
          value={patterns}
          onChange={setPatterns}
          placeholder="例如 gpt-4o*, claude-*"
          hint="按平台的模型名称匹配，* 匹配任意字符，多个用逗号或空格分开。留空则按平台内部租户的授权模型。"
          error={patternsError || undefined}
        />
      ) : (
        <fieldset className="tn-sub tn-field">
          <legend>模型</legend>
          <p className="hint">只能从该租户已授权的模型里选。不选则可用租户的全部授权模型，包括之后新授权的。</p>
          {catalogError && (
            <div className="note crit" role="alert">
              模型目录读取失败：{catalogError}
            </div>
          )}
          {grants.length === 0 ? (
            <p className="hint">该租户还没有授权模型；先在租户页授权。</p>
          ) : (
            <ul className="tn-checks tn-box">
              {grants.map((m) => (
                <Check
                  key={m.id}
                  label={m.name}
                  checked={models.includes(m.id)}
                  disabled={busy}
                  onChange={(on) => setModels((prev) => (on ? [...prev, m.id] : prev.filter((x) => x !== m.id)))}
                  error={at(`/model_ids/${sent?.model_ids?.indexOf(m.id) ?? -1}`)}
                />
              ))}
            </ul>
          )}
        </fieldset>
      )}
      <fieldset className="tn-sub tn-field">
        <legend>分组</legend>
        <p className="hint">每个密钥只路由到这一个分组里的上游账号。分组创建后不能更改。</p>
        {choices.length === 0 ? (
          <p className="hint">该租户没有可用的分组；先在租户页给它一个。</p>
        ) : (
          <label className="f">
            <span>分组（必选）</span>
            <select value={group} disabled={busy} onChange={(e) => setGroup(e.target.value)}>
              <option value="">请选择分组</option>
              {choices.map((g) => (
                <option key={g.id} value={g.name}>
                  {g.display_name}
                  {g.display_name === g.name ? "" : `（${g.name}）`}
                </option>
              ))}
            </select>
          </label>
        )}
        {at("/group_id") && (
          <div className="note crit" role="alert">
            {at("/group_id")}
          </div>
        )}
      </fieldset>
      <div className="tn-limits grid gap-x-4 sm:grid-cols-2">
        <LimitInput label="并发" value={concurrency} disabled={busy} onChange={setConcurrency} error={at("/rate_limit/concurrency")} />
        <LimitInput label="每分钟请求" value={rpm} disabled={busy} onChange={setRpm} error={at("/rate_limit/rpm")} />
      </div>
      <TextField
        label="过期时间"
        type="datetime-local"
        value={expires}
        onChange={setExpires}
        hint="留空表示不过期；时间采用 UTC+8。"
        error={at("/expires_at")}
      />
      {refused?.tooOld ? (
        <GatewayTooOld error={refused.tooOld} />
      ) : (
        refused?.general && (
          <div className="note crit" role="alert">
            {refused.general}
          </div>
        )
      )}
      <div className="tn-foot right">
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
        {/* A group with no upstream account leaves the key nothing to reach (ruling P1b-R6). */}
        <TwoStep
          label="创建"
          kind="primary"
          disabled={busy || group === ""}
          busy={busy}
          ask={() => group !== "" && !(choices.find((g) => g.name === group)?.accounts.length ?? 0)}
          warning={unreachableWarning(choices.filter((g) => g.name === group).map((g) => g.display_name), "这个密钥")}
          onConfirm={() => void submit()}
        />
      </div>
    </fieldset>
  );
}
