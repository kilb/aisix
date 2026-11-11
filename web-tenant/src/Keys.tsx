import { useCallback, useEffect, useId, useState, type CSSProperties } from "react";
import {
  ApiError,
  groups as groupApi,
  keys as keyApi,
  members as memberApi,
  models as modelApi,
  org as readOrg,
  type Member,
  type NewKey,
  type Org,
  type PortalGroup,
  type PortalModel,
  type SessionMember,
  type TenantKey,
  type TenantStatus,
} from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { ShownOnce } from "./controls";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field, useRefusalFocus } from "./ui/Field";
import { KeySecret } from "./ui/KeySecret";
import { Lamp, type LampState } from "./ui/Lamp";
import { TwoStep } from "./ui/TwoStep";
import { Card, CardHeader, PageHeader, Split } from "./components/page";

type Caps = Org["limits"];

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** A time from the API, in the viewer's own zone. */
export function when(at: string): string {
  return new Date(at).toLocaleString("zh-CN", { hour12: false });
}

/**
 * The organisation's API keys. An owner or an admin sees every key of the
 * organisation and a member their own: the control plane decides which. A
 * billing member never reaches this page and holds no key.
 *
 * While the organisation is suspended its keys do not work, and the page
 * offers revocation only: nothing that makes a key or switches one on.
 * `onSelfChanged` re-reads the session when a refusal says the organisation
 * is no longer active, so the page follows the state it is really in.
 */
export function Keys({
  member,
  status,
  onSelfChanged,
}: {
  member: SessionMember;
  status: TenantStatus;
  onSelfChanged: () => void;
}) {
  const active = status === "active";
  const manager = member.role === "owner" || member.role === "admin";
  const [items, setItems] = useState<TenantKey[] | null>(null);
  const [listErr, setListErr] = useState<string | null>(null);
  const [catalog, setCatalog] = useState<PortalModel[] | null>(null);
  const [groups, setGroups] = useState<PortalGroup[] | null>(null);
  const [caps, setCaps] = useState<Caps | null>(null);
  const [people, setPeople] = useState<Member[] | null>(null);
  const [readErr, setReadErr] = useState<string | null>(null);
  // The plaintext, in this page's state alone: it goes with a reload or a
  // change of page. The list's own 复制 reveals it again.
  const [minted, setMinted] = useState<string | null>(null);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // Which action on that key is in flight.
  const [acting, setActing] = useState<"switch" | "revoke" | "group" | null>(null);

  const load = useCallback(async () => {
    try {
      setItems(await keyApi.list());
      setListErr(null);
    } catch (e) {
      setListErr(failure(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The caps hold in any state; the models, the groups and the members feed
  // the form, which only an active organisation has. A member still reads
  // the groups, since every member holds keys and sees the page.
  useEffect(() => {
    const failed = (e: unknown) => setReadErr(failure(e));
    readOrg().then((o) => setCaps(o.limits), failed);
    if (!active) return;
    modelApi.list().then(setCatalog, failed);
    groupApi.list().then(setGroups, failed);
    if (manager) memberApi.list().then(setPeople, failed);
  }, [active, manager]);

  /** One change of key `k`; the list is read again whatever the answer. */
  async function act(k: TenantKey, what: "switch" | "revoke" | "group", change: () => Promise<unknown>) {
    setBusy(k.id);
    setActing(what);
    setActionErr(null);
    try {
      await change();
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onSelfChanged();
      // A refused body's own details are the only text that says why: the
      // group refusal ("密钥的分组创建后不能更改…", "分组不存在或不可用") is a
      // 422 whose message alone is the canned 提交的内容未通过校验.
      setActionErr(
        e instanceof ApiError && e.code === "version_mismatch"
          ? "该密钥已被他人修改，列表已重新载入，请再操作一次"
          : e instanceof ApiError
            ? formMessageFor(e)
            : failure(e),
      );
    } finally {
      setBusy(null);
      setActing(null);
    }
    await load();
  }

  function ownerOf(k: TenantKey): string {
    if (!k.owner) return "组织";
    if (k.owner.id === member.id) return "我";
    const id = k.owner.id;
    return k.owner.display_name ?? people?.find((p) => p.id === id)?.email ?? "成员";
  }

  function modelsOf(k: TenantKey): string {
    if (k.model_ids === null) return "全部授权模型";
    if (k.model_ids.length === 0) return "无";
    if (!catalog) return `${k.model_ids.length} 个模型`;
    const names = k.model_ids.flatMap((id) => catalog.find((m) => m.id === id)?.name ?? []);
    const gone = k.model_ids.length - names.length;
    return [...names, ...(gone > 0 ? [`${gone} 个已不在授权内`] : [])].join("、");
  }

  function limitsOf(k: TenantKey): string {
    // A limit the key leaves unset follows the organisation's cap.
    const one = (label: string, own?: number, cap?: number) =>
      own !== undefined ? `${label} ${count(own)}` : cap !== undefined ? `${label} ≤${count(cap)}` : null;
    const parts = [
      one("并发", k.rate_limit.concurrency, caps?.key_concurrency_max),
      one("每分钟", k.rate_limit.rpm, caps?.key_rpm_max),
    ].filter((p) => p !== null);
    return parts.length > 0 ? parts.join(" · ") : "不限";
  }

  return (
    <>
      <PageHeader title="密钥" />
      <Split
        asideWidth="clamp(288px, 22.5vw, 380px)"
        main={
          <Card className="keys-list">
            {!active && <div className="note crit">组织已被停用，所有密钥暂时无法调用；这里只能吊销密钥。</div>}
            {minted && (
              <ShownOnce title="密钥明文（可再次复制）" value={minted} onDone={() => setMinted(null)}>
                <span className="hint">密钥创建成功。之后可在下面的密钥列表里随时点「复制」再次获取明文。</span>
              </ShownOnce>
            )}
            {listErr && (
              <div className="note crit" role="alert">
                {listErr}
              </div>
            )}
            {actionErr && (
              <div className="note crit" role="alert">
                {actionErr}
              </div>
            )}
            {items === null ? (
              !listErr && <Loading />
            ) : items.length === 0 ? (
              <EmptyState mark="key" text="还没有密钥。" />
            ) : (
              <DataTable>
                <thead>
                  <tr>
                    <th>名称</th>
                    <th>前缀</th>
                    <th>归属</th>
                    <th>模型</th>
                    <th>分组</th>
                    <th>限制</th>
                    <th>状态</th>
                    <th>
                      <span className="vh">操作</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {items.map((k) => {
                    const state = stateOf(k);
                    const owner = ownerOf(k);
                    const models = modelsOf(k);
                    return (
                      <tr key={k.id}>
                        <td>
                          <span className="trunc [--trunc:144px] min-[1800px]:[--trunc:160px]" title={k.label}>
                            {k.label}
                          </span>
                        </td>
                        <td>
                          <KeySecret
                            prefix={k.prefix}
                            className="acts mt-1.5 flex w-max justify-start min-[1800px]:mt-0 min-[1800px]:ml-2 min-[1800px]:inline-flex"
                            reveal={() => keyApi.reveal(k.id)}
                            unavailable={
                              k.plaintext_available ? null : "该密钥创建于旧版本，无法再次查看；如需明文请新建密钥"
                            }
                            onError={(e) =>
                              setActionErr(
                                e instanceof ApiError && e.code === "plaintext_unavailable"
                                  ? messageFor(e)
                                  : e instanceof ApiError
                                    ? formMessageFor(e)
                                    : failure(e),
                              )
                            }
                          />
                        </td>
                        <td>
                          <span className="trunc" style={{ "--trunc": "128px" } as CSSProperties} title={owner}>
                            {owner}
                          </span>
                        </td>
                        <td>
                          <span className="trunc [--trunc:120px] min-[1800px]:[--trunc:216px]" title={models}>
                            {models}
                          </span>
                        </td>
                        <td>
                          <GroupCell
                            k={k}
                            groups={groups}
                            active={active}
                            busy={busy === k.id}
                            acting={acting}
                            onChoose={(groupId) => void act(k, "group", () => keyApi.setGroup(k.id, groupId, k.version))}
                          />
                        </td>
                        <td className="tabular [word-break:keep-all] max-[1799px]:whitespace-normal">
                          <span className="block min-w-[5.75rem] max-w-[6.5rem] min-[1800px]:max-w-none">{limitsOf(k)}</span>
                        </td>
                        <td>
                          <Lamp state={state.lamp}>{state.text}</Lamp>
                          {state.note && (
                            <span className="hint cell-note max-w-[8rem] whitespace-normal min-[1800px]:max-w-none">{state.note}</span>
                          )}
                        </td>
                        <td className="right">
                          <span className="acts">
                            {/* The organisation's own switch, which the platform's overrides. */}
                            {active && !k.disabled_by_platform && (
                              <Button
                                size="sm"
                                disabled={busy === k.id}
                                busy={busy === k.id && acting === "switch"}
                                busyLabel={k.disabled ? "启用" : "停用"}
                                onClick={() => void act(k, "switch", () => keyApi.setDisabled(k.id, !k.disabled, k.version))}
                              >
                                {k.disabled ? "启用" : "停用"}
                              </Button>
                            )}
                            <TwoStep
                              label="吊销"
                              size="sm"
                              warning="吊销后立即失效，且无法恢复"
                              disabled={busy === k.id}
                              busy={busy === k.id && acting === "revoke"}
                              onConfirm={() => void act(k, "revoke", () => keyApi.revoke(k.id, k.version))}
                            />
                          </span>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </DataTable>
            )}
          </Card>
        }
        aside={
          active ? (
            <NewKeyForm
              me={member}
              manager={manager}
              catalog={catalog}
              groups={groups}
              caps={caps}
              people={people}
              readErr={readErr}
              onInactive={onSelfChanged}
              onCreated={(plaintext) => {
                setMinted(plaintext);
                void load();
              }}
            />
          ) : undefined
        }
      />
    </>
  );
}

/** A count, with thousands separators. */
function count(n: number): string {
  return n.toLocaleString("en-US");
}

/**
 * The group a key routes to, by display name: what it is called, whether it
 * still routes, and, for a key an older release left without one, the one
 * time it may take one. Never a routing name, which no answer carries.
 *
 * A key whose group its operator unassigned stays named by that group and
 * is closed instead: the page says so, and the gateway refuses it.
 */
function GroupCell({
  k,
  groups,
  active,
  busy,
  acting,
  onChoose,
}: {
  k: TenantKey;
  groups: PortalGroup[] | null;
  active: boolean;
  busy: boolean;
  acting: "switch" | "revoke" | "group" | null;
  onChoose: (groupId: string) => void;
}) {
  const [choosing, setChoosing] = useState(false);
  const [choice, setChoice] = useState("");

  if (k.group === null) {
    if (!active || !choosing) {
      return (
        <span className="acts">
          <span className="hint">未选分组</span>
          {active && (
            <Button size="sm" disabled={busy} onClick={() => setChoosing(true)}>
              选择分组
            </Button>
          )}
        </span>
      );
    }
    return (
      <span className="acts">
        <span className="hint">分组只能选择一次，选定后不能更改，并决定这个密钥的价格。</span>
        <select aria-label="分组" value={choice} onChange={(e) => setChoice(e.target.value)}>
          <option value="">请选择</option>
          {(groups ?? []).map((g) => (
            <option key={g.id} value={g.id}>
              {g.display_name}
            </option>
          ))}
        </select>
        <Button
          size="sm"
          disabled={busy || choice === ""}
          busy={busy && acting === "group"}
          busyLabel="选择分组"
          onClick={() => onChoose(choice)}
        >
          确认选择
        </Button>
        <Button size="sm" disabled={busy} onClick={() => setChoosing(false)}>
          取消
        </Button>
      </span>
    );
  }

  return (
    <span>
      <span className="trunc" style={{ "--trunc": "144px" } as CSSProperties} title={k.group.display_name}>
        {k.group.display_name}
      </span>
      {!k.group_available && <span className="hint cell-note">分组已停用</span>}
    </span>
  );
}

function stateOf(k: TenantKey): { text: string; lamp: LampState; note?: string } {
  if (k.disabled_by_platform) return { text: "已被平台停用", lamp: "disabled", note: "只有平台可以重新启用" };
  if (k.disabled) return { text: "已停用", lamp: "disabled" };
  if (k.expires_at && Date.parse(k.expires_at) <= Date.now()) return { text: "已过期", lamp: "disabled" };
  // A key with no group, or whose group its organisation no longer has, is
  // closed by the gateway whatever its switches say; the group cell says why.
  if (!k.group_available) return { text: "已停用", lamp: "disabled" };
  return { text: "可用", lamp: "applied", note: k.expires_at ? `${when(k.expires_at)} 到期` : undefined };
}

type Field = "label" | "group" | "models" | "concurrency" | "rpm" | "expires" | "owner";
type FieldErrors = Partial<Record<Field, string>>;

/** The form field a refusal's JSON path names, or null for one the form has no field for. */
function fieldOf(path: string): Field | null {
  if (path === "/label") return "label";
  if (path === "/group_id") return "group";
  if (path === "/model_ids" || path.startsWith("/model_ids/")) return "models";
  if (path === "/rate_limit/concurrency") return "concurrency";
  if (path === "/rate_limit/rpm") return "rpm";
  if (path === "/expires_at") return "expires";
  if (path === "/member_id") return "owner";
  return null;
}

/** A limit as typed: none when empty, a whole number, or why it is neither. */
function limit(raw: string): { value?: number; problem?: string } {
  const text = raw.trim();
  if (text === "") return {};
  if (!/^\d+$/.test(text)) return { problem: "请填整数；留空表示不单独限制" };
  return { value: Number(text) };
}

function capHint(cap: number | undefined, caps: Caps | null): string {
  if (caps === null) return "组织上限载入中…";
  return cap === undefined ? "组织未设上限；留空表示不限" : `组织上限 ${count(cap)}；留空则按此上限`;
}

function NewKeyForm({
  me,
  manager,
  catalog,
  groups,
  caps,
  people,
  readErr,
  onInactive,
  onCreated,
}: {
  me: SessionMember;
  manager: boolean;
  catalog: PortalModel[] | null;
  groups: PortalGroup[] | null;
  caps: Caps | null;
  people: Member[] | null;
  readErr: string | null;
  /** Called when the control plane says the organisation is no longer active. */
  onInactive: () => void;
  onCreated: (plaintext: string) => void;
}) {
  const id = useId();
  const [label, setLabel] = useState("");
  // The group the key is bought in: what it is priced at, so it is chosen
  // with the key and never changed afterwards.
  const [group, setGroup] = useState("");
  const [picked, setPicked] = useState<string[]>([]);
  const [concurrency, setConcurrency] = useState("");
  const [rpm, setRpm] = useState("");
  const [expires, setExpires] = useState("");
  // Empty: the caller.
  const [owner, setOwner] = useState("");
  const [errs, setErrs] = useState<FieldErrors>({});
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  // One group is the organisation's only choice, so it is preselected; it is
  // still shown, since it is what the key is priced at.
  useEffect(() => {
    if (groups !== null && groups.length === 1) setGroup(groups[0]?.id ?? "");
  }, [groups]);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    const found: FieldErrors = {};
    // The group is what the key is priced at, so it is never left to the
    // backend's guess: with several offered, one is chosen or the submit is
    // refused here, and with none the button is disabled below.
    if (groups !== null && groups.length > 0 && group === "") found.group = "请选择分组";
    const c = limit(concurrency);
    const r = limit(rpm);
    if (c.problem) found.concurrency = c.problem;
    if (r.problem) found.rpm = r.problem;
    let expiresAt: string | undefined;
    if (expires) {
      const at = new Date(expires);
      if (Number.isNaN(at.getTime())) found.expires = "时间格式不正确";
      else expiresAt = at.toISOString();
    }
    setErrs(found);
    if (Object.keys(found).length > 0) {
      focusRefusal();
      return;
    }

    // Only what was set is sent: no model picked follows every grant, and a
    // limit left empty follows the organisation's cap.
    const body: NewKey = { label };
    if (group !== "") body.group_id = group;
    if (picked.length > 0) body.model_ids = picked;
    if (c.value !== undefined || r.value !== undefined) body.rate_limit = { concurrency: c.value, rpm: r.value };
    if (expiresAt) body.expires_at = expiresAt;
    if (manager && owner) body.member_id = owner;
    setBusy(true);
    try {
      const { plaintext } = await keyApi.create(body);
      setLabel("");
      setGroup(groups !== null && groups.length === 1 ? (groups[0]?.id ?? "") : "");
      setPicked([]);
      setConcurrency("");
      setRpm("");
      setExpires("");
      setOwner("");
      onCreated(plaintext);
    } catch (e) {
      if (e instanceof ApiError && e.code === "validation_failed" && e.details.length > 0) {
        // Each refusal beside its field; one the form has no field for above the button.
        const byField: FieldErrors = {};
        const rest: string[] = [];
        for (const d of e.details) {
          const f = fieldOf(d.path);
          if (f) byField[f] = byField[f] ? `${byField[f]}；${d.message}` : d.message;
          else rest.push(d.message);
        }
        setErrs(byField);
        if (rest.length > 0) setErr(rest.join("；"));
        focusRefusal();
      } else if (e instanceof ApiError && e.code === "unavailable") {
        setErr("平台暂时无法创建密钥，请稍后再试");
      } else if (e instanceof ApiError && e.code === "tenant_not_active") {
        setErr(messageFor(e));
        onInactive();
      } else {
        setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
      }
    } finally {
      setBusy(false);
    }
  }

  // The models are a fieldset rather than one Field: their hint and refusal are wired here.
  const modelsHint = `${id}-models-hint`;
  const modelsErr = `${id}-models-err`;
  const groupHint = `${id}-group-hint`;
  const groupErr = `${id}-group-err`;
  const choices = (people ?? []).filter((p) => p.id !== me.id && p.role !== "billing");
  // The hint is rendered only when there is a group to choose, so it is
  // described by only then.
  const groupDescribedBy = [groups !== null && groups.length > 0 ? groupHint : null, errs.group ? groupErr : null]
    .filter(Boolean)
    .join(" ");
  // With no group assigned the form cannot create anything: the button says
  // why and the submit never reaches the server to earn its refusal.
  const noGroup = groups !== null && groups.length === 0;

  return (
    <Card>
      <CardHeader title="新建密钥" />
      {readErr && (
        <div className="note crit" role="alert">
          {readErr}
        </div>
      )}
      <form ref={form} className="grid gap-x-4 gap-y-5 sm:grid-cols-2 xl:grid-cols-1 min-[1800px]:grid-cols-2" onSubmit={submit} noValidate>
        <Field label="名称" wide error={errs.label}>
          {(aria) => <input value={label} onChange={(e) => setLabel(e.target.value)} {...aria} />}
        </Field>

        {/* The group, as radio cards: one per group the organisation is
            assigned, by display name and description. With one it is
            preselected and still shown, since it is what the key is priced
            at; with none the form cannot create anything and says why. */}
        <div className="field wide">
          <fieldset
            className="field"
            aria-describedby={groupDescribedBy || undefined}
            aria-invalid={errs.group ? true : undefined}
          >
            <legend>分组</legend>
            {groups === null ? (
              <span className="hint">载入中…</span>
            ) : groups.length === 0 ? (
              <span className="hint">运营尚未为本组织分配分组。请联系平台运营分配分组后，再新建密钥。</span>
            ) : (
              <div className="picks picks-cards">
                {groups.map((g) => (
                  <label key={g.id} className="pick">
                    <input
                      type="radio"
                      name={`${id}-group`}
                      checked={group === g.id}
                      onChange={() => {
                        setGroup(g.id);
                        // The refusal belonged to an empty choice; one made
                        // is not refused any more.
                        setErrs((was) => {
                          if (!was.group) return was;
                          const { group: _, ...rest } = was;
                          return rest;
                        });
                      }}
                    />
                    <span className="pick-text">
                      <strong>{g.display_name}</strong>
                      {g.description !== "" && <span className="hint">{g.description}</span>}
                    </span>
                  </label>
                ))}
              </div>
            )}
          </fieldset>
          {groups !== null && groups.length > 0 && (
            <p className="hint" id={groupHint}>
              分组建好密钥后不能更换；如需更换分组，请新建密钥
              {/* The prices of the group just picked, which is what the
                  choice costs: the address carries the group's id, so the
                  page opens on that tab (never on its first one). */}
              {group !== "" && (
                <>
                  {" · "}
                  <a href={`#prices?group=${encodeURIComponent(group)}`}>查看价格</a>
                </>
              )}
            </p>
          )}
          {errs.group && (
            <p className="field-problem" id={groupErr}>
              {errs.group}
            </p>
          )}
        </div>

        <div className="field wide">
          <fieldset
            className="field"
            aria-describedby={errs.models ? `${modelsHint} ${modelsErr}` : modelsHint}
            aria-invalid={errs.models ? true : undefined}
          >
            <legend>可用模型</legend>
            {catalog === null ? (
              <span className="hint">载入中…</span>
            ) : catalog.length === 0 ? (
              <span className="hint">组织还没有获授权的模型</span>
            ) : (
              <div className="picks">
                {catalog.map((m) => (
                  <label key={m.id} className="pick">
                    <input
                      type="checkbox"
                      checked={picked.includes(m.id)}
                      onChange={(e) =>
                        setPicked(e.target.checked ? [...picked, m.id] : picked.filter((p) => p !== m.id))
                      }
                    />
                    {m.name}
                  </label>
                ))}
              </div>
            )}
          </fieldset>
          <p className="hint" id={modelsHint}>
            不勾选即可使用组织获授权的全部模型，包括以后新授权的
          </p>
          {errs.models && (
            <p className="field-problem" id={modelsErr}>
              {errs.models}
            </p>
          )}
        </div>

        <Field label="并发" hint={capHint(caps?.key_concurrency_max, caps)} error={errs.concurrency}>
          {(aria) => (
            <input inputMode="numeric" value={concurrency} onChange={(e) => setConcurrency(e.target.value)} {...aria} />
          )}
        </Field>

        <Field label="每分钟请求" hint={capHint(caps?.key_rpm_max, caps)} error={errs.rpm}>
          {(aria) => <input inputMode="numeric" value={rpm} onChange={(e) => setRpm(e.target.value)} {...aria} />}
        </Field>

        <Field label="过期时间" wide hint="留空表示永不过期；按本机时区" error={errs.expires}>
          {(aria) => (
            <input type="datetime-local" value={expires} onChange={(e) => setExpires(e.target.value)} {...aria} />
          )}
        </Field>

        {manager && (
          <Field label="归属成员" wide error={errs.owner}>
            {(aria) => (
              <select value={owner} onChange={(e) => setOwner(e.target.value)} {...aria}>
                <option value="">我</option>
                {choices.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.display_name ?? p.email}
                    {p.disabled ? "（已停用）" : ""}
                  </option>
                ))}
              </select>
            )}
          </Field>
        )}

        {err && (
          <div className="note crit col-span-full" role="alert">
            {err}
          </div>
        )}
        <div className="col-span-full -mx-6 -mb-6 flex justify-end gap-2 rounded-b-xl border-t border-border bg-muted/40 px-6 py-3 max-[719px]:-mx-4 max-[719px]:-mb-4 max-[719px]:px-4">
          <Button kind="primary" type="submit" busy={busy} disabled={noGroup}>
            新建密钥
          </Button>
        </div>
      </form>
    </Card>
  );
}
