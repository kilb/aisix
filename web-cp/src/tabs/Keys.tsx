import { Fragment, useCallback, useEffect, useId, useMemo, useState } from "react";
import type { TabProps } from "../App";
import { GatewayTooOld } from "../components/GatewayTooOld";
import { RolloutBadge } from "../components/RolloutBadge";
import {
  keys as keysApi,
  providerGroups,
  resources,
  tenants,
  type AdminKey,
  type Member,
  type MintedKey,
  type NewKey,
  type Tenant,
} from "../lib/api";
import { errorText, fmtTime } from "../lib/fmt";
import {
  accountsByGroup,
  atList,
  Check,
  Failure,
  follow,
  GroupPicker,
  groupOptions,
  EffectiveGroups,
  groupsText,
  LimitInput,
  OverrideEditor,
  refusal,
  ROLE_LABEL,
  STATUS_LABEL,
  StrayNote,
  unreachableWarning,
  useLifetime,
  useLoad,
  whyOff,
  type Refusal,
} from "./Tenants";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";
import { CopyButton } from "../ui/CopyButton";
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
      <div className="panel">
        <h2>
          密钥
          <Button
            size="sm"
            type="button"
            icon="refresh"
            onClick={() => {
              void list.reload();
              setTick((n) => n + 1);
            }}
          >
            刷新
          </Button>
        </h2>
        <p className="hint">
          平台内部租户的密钥是运维自用的，可以按名称模式授权模型；其他租户的密钥只能使用该租户已授权的模型。明文只在创建时显示一次，之后只显示前缀。
        </p>
        {list.error && (
          <div className="note crit" role="alert">
            {list.error}{" "}
            <Button type="button" onClick={() => void list.reload()}>
              重试
            </Button>
          </div>
        )}
        {list.data === null ? (
          !list.error && <Loading />
        ) : ordered.length === 0 ? (
          <EmptyState text="还没有租户。" />
        ) : (
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
        )}
      </div>
      {/* Keyed by tenant: switching drops everything shown for the last one, a new key's plaintext included. */}
      {tenantId && <TenantKeys key={tenantId} tenantId={tenantId} canEdit={canEdit} tick={tick} />}
    </>
  );
}

function ownerOf(k: AdminKey, tenant: Tenant, members: Member[] | null): string {
  if (!k.member_id) return tenant.is_internal ? "—" : "租户共用";
  return members?.find((m) => m.id === k.member_id)?.email ?? "成员";
}

/** The models key `k` may call, by name; a selected model the tenant no longer grants is marked. */
function modelsOf(k: AdminKey, tenant: Tenant, names: Map<string, string>): string {
  if (k.allowed_models?.length) return k.allowed_models.join("、");
  if (k.model_ids === null) return "全部授权模型";
  if (k.model_ids.length === 0) return "无";
  return k.model_ids
    .map((id) => `${names.get(id) ?? id}${tenant.allowed_model_ids.includes(id) ? "" : "（租户未授权）"}`)
    .join("、");
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
  const groups = useLoad(() => providerGroups.list(), [tick]);
  const [creating, setCreating] = useState(false);
  // Held only here: switching tenant or leaving the tab drops it for good.
  const [minted, setMinted] = useState<MintedKey | null>(null);
  const [editing, setEditing] = useState<string | null>(null);

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

  const names = useMemo(
    () => new Map((catalog.data ?? []).map((m) => [m.id, m.doc.display_name || m.id])),
    [catalog.data],
  );
  const options = useMemo(
    () => groupOptions(groups.data, tenant?.allowed_provider_groups ?? []),
    [groups.data, tenant?.allowed_provider_groups],
  );
  const accounts = useMemo(() => accountsByGroup(groups.data), [groups.data]);

  const saved = (k: AdminKey) => {
    keyList.setData((list) => list && list.map((x) => (x.id === k.id ? k : x)));
    void refresh();
  };

  if (!tenant) {
    return (
      <div className="panel">
        {tenantError ? (
          <div className="note crit" role="alert">
            {tenantError}{" "}
            <Button type="button" onClick={() => void refresh()}>
              重试
            </Button>
          </div>
        ) : (
          <Loading />
        )}
      </div>
    );
  }

  const columns = canEdit ? 9 : 8;
  return (
    <section className="panel" aria-label={`${tenant.name} 的密钥`}>
      <h2>
        {tenant.name}
        <RolloutBadge rollout={tenant.rollout} />
        {/* A pending tenant has no keys until it is approved; the control plane refuses one. */}
        {canEdit && tenant.status !== "pending" && (
          <Button
            kind="primary"
            size="sm"
            type="button"
            aria-expanded={creating}
            onClick={() => setCreating((open) => !open)}
          >
            新建密钥
          </Button>
        )}
      </h2>
      <p className="hint">
        {STATUS_LABEL[tenant.status] ?? tenant.status} · 可用分组 {groupsText(tenant.allowed_provider_groups, tenant)}
        。密钥未设的限额按租户的单 key 上限；运维停用的密钥只有运维能重新启用。
      </p>
      {tenantError && (
        <div className="note crit" role="alert">
          {tenantError}
        </div>
      )}

      {creating && canEdit && tenant.status !== "pending" && (
        <CreateForm
          tenant={tenant}
          names={names}
          catalogError={catalog.error}
          options={options}
          accounts={accounts}
          onMinted={(m) => {
            setMinted(m);
            setCreating(false);
            keyList.setData((list) => (list ? [...list, m.key] : [m.key]));
            void refresh();
          }}
          onCancel={() => setCreating(false)}
        />
      )}
      {minted && <MintedNote minted={minted} onClose={() => setMinted(null)} />}

      {keyList.error && (
        <div className="note crit" role="alert">
          密钥读取失败：{keyList.error}
        </div>
      )}
      {members.error && (
        <div className="note crit" role="alert">
          成员读取失败：{members.error}
        </div>
      )}
      {keyList.data === null ? (
        !keyList.error && <Loading />
      ) : keyList.data.length === 0 ? (
        <EmptyState mark="key" text="该租户还没有密钥。" />
      ) : (
        <DataTable label="密钥列表">
          <thead>
            <tr>
              <th>名称</th>
              <th>前缀</th>
              <th>成员</th>
              <th>模型</th>
              <th>生效分组</th>
              <th>限额</th>
              <th>状态</th>
              <th>创建者</th>
              {canEdit && <th>操作</th>}
            </tr>
          </thead>
          <tbody>
            {keyList.data.map((k) => (
              <Fragment key={k.id}>
                <tr>
                  <td>{k.label}</td>
                  <td>
                    <code>{k.prefix}…</code>
                  </td>
                  <td>{ownerOf(k, tenant, members.data)}</td>
                  <td>{modelsOf(k, tenant, names)}</td>
                  <td>
                    <EffectiveGroups groups={k.effective_provider_groups} tenant={tenant} />
                    {k.allowed_provider_groups?.length ? <span className="tn-note">覆盖</span> : null}
                    <StrayNote k={k} tenant={tenant} />
                  </td>
                  <td>{limitsOf(k)}</td>
                  <td>
                    <Status k={k} tenant={tenant} members={members.data} />
                  </td>
                  <td>
                    <Creator k={k} members={members.data} />
                  </td>
                  {canEdit && (
                    <td>
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
                      <OverrideEditor
                        k={k}
                        options={options}
                        accounts={accounts}
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
      )}
    </section>
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
      <div className="tn-actions">
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
        <Button type="button" size="sm" aria-expanded={editing} onClick={onEdit}>
          分组覆盖
        </Button>
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
    <div className="note warn">
      <strong>明文只显示这一次。</strong>密钥「{minted.key.label}」已创建。请现在复制并交给使用方；关闭或离开此页后无法再查看，列表只显示前缀。
      <code className="tn-token" aria-label="密钥明文">
        {minted.plaintext}
      </code>
      <div className="tn-actions">
        <CopyButton text={minted.plaintext} onFailed={() => setCopied("无法写入剪贴板，请手动选中复制")} />
        <Button type="button" onClick={onClose}>
          关闭
        </Button>
      </div>
      {copied && <p className="hint">{copied}</p>}
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
    <div>
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
    atList("/allowed_provider_groups")(path) ||
    path === "/rate_limit/concurrency" ||
    path === "/rate_limit/rpm" ||
    path === "/expires_at"
  );
}

function CreateForm({
  tenant,
  names,
  catalogError,
  options,
  accounts,
  onMinted,
  onCancel,
}: {
  tenant: Tenant;
  names: Map<string, string>;
  catalogError: string | null;
  options: string[];
  accounts: Map<string, string[]>;
  onMinted: (minted: MintedKey) => void;
  onCancel: () => void;
}) {
  const [label, setLabel] = useState("");
  const [patterns, setPatterns] = useState("");
  const [models, setModels] = useState<string[]>([]);
  const [groups, setGroups] = useState<string[]>([]);
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
      tenant.allowed_model_ids
        .map((id) => ({ id, name: names.get(id) ?? id }))
        .sort((a, b) => a.name.localeCompare(b.name)),
    [tenant.allowed_model_ids, names],
  );
  const picked = options.filter((g) => groups.includes(g));
  const reachable = (g: string) => (accounts.get(g) ?? []).length > 0;

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
    if (picked.length > 0) b.allowed_provider_groups = picked;
    const limit: NonNullable<NewKey["rate_limit"]> = {};
    if (concurrency.trim() !== "") limit.concurrency = Number(concurrency);
    if (rpm.trim() !== "") limit.rpm = Number(rpm);
    if (limit.concurrency !== undefined || limit.rpm !== undefined) b.rate_limit = limit;
    if (expires !== "") b.expires_at = new Date(expires).toISOString();
    return b;
  }

  async function submit() {
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
    <fieldset ref={part} className="tn-section">
      <legend>新建密钥</legend>
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
        <fieldset className="tn-section">
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
            <ul className="tn-checks">
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
      <fieldset className="tn-section">
        <legend>分组覆盖</legend>
        <p className="hint">只对这个密钥生效，取代租户的可用分组。不选则沿用租户的分组。</p>
        <GroupPicker
          options={options}
          chosen={groups}
          onChange={setGroups}
          disabled={busy}
          accounts={accounts}
          errorFor={(g) => at(`/allowed_provider_groups/${sent?.allowed_provider_groups?.indexOf(g) ?? -1}`)}
        />
        {at("/allowed_provider_groups") && (
          <div className="note crit" role="alert">
            {at("/allowed_provider_groups")}
          </div>
        )}
      </fieldset>
      <div className="tn-limits">
        <LimitInput label="并发" value={concurrency} disabled={busy} onChange={setConcurrency} error={at("/rate_limit/concurrency")} />
        <LimitInput label="每分钟请求" value={rpm} disabled={busy} onChange={setRpm} error={at("/rate_limit/rpm")} />
        <TextField
          label="过期时间"
          type="datetime-local"
          value={expires}
          onChange={setExpires}
          hint="留空表示不过期。"
          error={at("/expires_at")}
        />
      </div>
      {refused?.tooOld ? (
        <GatewayTooOld error={refused.tooOld} />
      ) : (
        refused?.general && (
          <div className="note crit" role="alert">
            {refused.general}
          </div>
        )
      )}
      <div className="tn-actions">
        {/* Groups none of which holds an upstream account leave the key nothing to reach (ruling P1b-R6). */}
        <TwoStep
          label="创建"
          kind="primary"
          disabled={busy}
          busy={busy}
          ask={() => picked.length > 0 && !picked.some(reachable)}
          warning={unreachableWarning(picked, "这个密钥")}
          onConfirm={() => void submit()}
        />
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
      </div>
    </fieldset>
  );
}
