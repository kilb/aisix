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
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "./components/page";
import { RankBars } from "./components/charts";
import { formatCount } from "./lib/chartFormat";
import { cn } from "./lib/cn";
import { fmtBeijing, parseBeijingInput } from "./lib/time";
import { useRecordTable } from "./lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "./ui/RecordTable";

// 桌面：5 + 7 / 12 / 12；平板与手机按卡片顺序各占整行。

type Caps = Org["limits"];

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** API 时间统一显示为北京时间。 */
export function when(at: string): string {
  return fmtBeijing(at);
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
    if (!catalog) return `${formatCount(k.model_ids.length)} 个模型`;
    const names = k.model_ids.flatMap((id) => catalog.find((m) => m.id === id)?.name ?? []);
    const gone = k.model_ids.length - names.length;
    return [...names, ...(gone > 0 ? [`${formatCount(gone)} 个已不在授权内`] : [])].join("、");
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

  const [stateFilter, setStateFilter] = useState("");
  const [groupFilter, setGroupFilter] = useState("");
  const table = useRecordTable({
    items: (items ?? []).filter((key) => (!stateFilter || stateOf(key).text === stateFilter) && (!groupFilter || key.group?.id === groupFilter)),
    searchText: (key) => [key.label, key.prefix, ownerOf(key), key.group?.display_name ?? ""],
    resetKey: `${stateFilter}/${groupFilter}`,
  });

  const visibleScope = manager ? "全组织密钥" : "我的密钥";
  const readyKeys = items?.filter((key) => stateOf(key).lamp === "applied").length;
  const stateCounts = items === null ? [] : [
    { label: "配置可用", value: 0 },
    { label: "平台停用", value: 0 },
    { label: "主动停用", value: 0 },
    { label: "已过期", value: 0 },
    { label: "分组不可用", value: 0 },
  ];
  for (const key of items ?? []) {
    const index = key.disabled_by_platform ? 1 : key.disabled ? 2
      : key.expires_at && Date.parse(key.expires_at) <= Date.now() ? 3 : !key.group_available ? 4 : 0;
    const bucket = stateCounts[index];
    if (bucket) bucket.value += 1;
  }

  return (
    <>
      <PageHeader
        title="密钥"
        eyebrow={visibleScope}
        dataLead={items === null ? "管理调用凭据、模型范围与使用限制。" : <>
          当前可见 <span className="font-medium text-ink">{formatCount(items.length)}</span> 把密钥，
          其中 <span className="font-medium text-ink">{formatCount(readyKeys ?? 0)}</span> 把配置可用。
          {active ? "实际调用仍受组织授权和额度约束。" : "组织已停用，所有密钥暂时无法调用。"}
        </>}
      />
      <BentoGrid>
        <Card className={span.d5Wide}>
          <CardHeader number={1} title="当前可见密钥" />
          <RecordTableToolbar search={{ value: table.query, onChange: table.setQuery, label: "搜索密钥", placeholder: "名称、公开前缀、归属或分组" }}>
            <Field label="密钥状态"><select value={stateFilter} onChange={(e) => setStateFilter(e.target.value)}><option value="">全部状态</option>{["可用", "已被平台停用", "已停用", "已过期"].map((value) => <option key={value}>{value}</option>)}</select></Field>
            <Field label="密钥分组"><select value={groupFilter} onChange={(e) => setGroupFilter(e.target.value)}><option value="">全部分组</option>{(groups ?? []).map((group) => <option key={group.id} value={group.id}>{group.display_name}</option>)}</select></Field>
          </RecordTableToolbar>
          {items === null ? (listErr ? <CardMessage onRetry={() => void load()}>密钥读取失败，请重试。</CardMessage> : <MetricSkeleton />) : (
            <>
              <Metric size="hero">{formatCount(items.length)}</Metric>
              <CardFooter>
                <p className="text-body text-ink-soft">{manager ? "组织成员及组织持有的全部密钥" : "仅包含归属你的密钥"}</p>
                <p className="mt-2 font-mono text-meta text-ink-soft">完整可见列表 · 不包含已吊销密钥</p>
              </CardFooter>
            </>
          )}
        </Card>
        <Card className={span.d7}>
          <CardHeader number={2} title="密钥配置状态" description="每把密钥只计入一个状态；不代表实时调用结果。" />
          <CardFooter>
            {items === null ? (listErr ? <CardMessage onRetry={() => void load()}>状态读取失败，请重试。</CardMessage> : <Loading />) : (
              <RankBars
                items={stateCounts.map((item) => ({ ...item, displayValue: `${formatCount(item.value)} 把` }))}
                accessibleLabel={`当前可见密钥中，${formatCount(readyKeys ?? 0)} 把配置可用，共 ${formatCount(items.length)} 把`}
                emptyLabel="还没有密钥"
              />
            )}
            <p className="mt-4 text-caption text-ink-soft">平台停用、主动停用、过期、分组不可用按此顺序归类。组织状态、预算及实时额度另行生效。</p>
          </CardFooter>
        </Card>
        <Card className={cn(span.d12, "keys-list")}>
          <CardHeader number={3} title="密钥列表" description="复制或查看明文会重新获取密钥；吊销后立即失效，无法恢复。" />
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
                {table.rows.map((k) => {
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
          {items !== null && <RecordTablePager page={table.page} pageSize={table.pageSize} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} total={table.filteredCount} label="密钥" />}
        </Card>
        {active && (
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
        )}
      </BentoGrid>
    </>
  );
}

/** A count, with thousands separators. */
function count(n: number): string {
  return formatCount(n);
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
  const [modelScope, setModelScope] = useState<"inherited" | "selected">("inherited");
  const [modelsOpen, setModelsOpen] = useState(false);
  const [modelSearch, setModelSearch] = useState("");
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
  const noModels = modelScope === "selected" && picked.length === 0;
  const modelsEmptyMessage = "请至少选择一个模型，或改为不限制模型";

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
    if (noModels) {
      found.models = modelsEmptyMessage;
      setModelsOpen(true);
      setModelSearch("");
    }
    const c = limit(concurrency);
    const r = limit(rpm);
    if (c.problem) found.concurrency = c.problem;
    if (r.problem) found.rpm = r.problem;
    let expiresAt: string | undefined;
    if (expires) {
      const at = parseBeijingInput(expires);
      if (at === null) found.expires = "时间格式不正确";
      else expiresAt = at;
    }
    setErrs(found);
    if (Object.keys(found).length > 0) {
      focusRefusal();
      return;
    }

    // 不限制模型时省略名单，继承组织当前及后续授权；指定模式始终发送所选 ID。
    // 留空的限流值继续使用组织上限。
    const body: NewKey = { label };
    if (group !== "") body.group_id = group;
    if (modelScope === "selected") body.model_ids = picked;
    if (c.value !== undefined || r.value !== undefined) body.rate_limit = { concurrency: c.value, rpm: r.value };
    if (expiresAt) body.expires_at = expiresAt;
    if (manager && owner) body.member_id = owner;
    setBusy(true);
    try {
      const { plaintext } = await keyApi.create(body);
      setLabel("");
      setGroup(groups !== null && groups.length === 1 ? (groups[0]?.id ?? "") : "");
      setModelScope("inherited");
      setModelsOpen(false);
      setModelSearch("");
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
        if (byField.models) {
          setModelsOpen(true);
          setModelSearch("");
        }
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
  const modelsList = `${id}-models-list`;
  const modelsEmpty = `${id}-models-empty`;
  const modelsDescribedBy = [modelsHint, errs.models ? modelsErr : noModels ? modelsEmpty : null].filter(Boolean).join(" ");
  const search = modelSearch.trim().toLocaleLowerCase();
  const visibleModels = (catalog ?? []).filter((m) => m.name.toLocaleLowerCase().includes(search));
  function clearModelsError() {
    setErrs((was) => {
      const { models: _, ...rest } = was;
      return rest;
    });
  }
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
    <Card className={span.d12}>
      <CardHeader number={4} title="新建密钥" description="先选择计价分组，再设置模型范围与限制。未填写的限制沿用组织上限。" />
      {readErr && (
        <div className="note crit" role="alert">
          {readErr}
        </div>
      )}
      <form ref={form} className="grid min-w-0 gap-x-7 gap-y-5 tablet:grid-cols-2" onSubmit={submit} noValidate>
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
                  <a className="inline-flex min-h-11 min-w-11 items-center" href={`#prices?group=${encodeURIComponent(group)}`}>查看价格</a>
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
          <fieldset className="field" aria-describedby={modelsDescribedBy}>
            <legend>可用模型</legend>
            <div className="grid gap-3">
              <label className="pick">
                <input
                  type="radio"
                  name={`${id}-model-scope`}
                  checked={modelScope === "inherited"}
                  aria-describedby={modelsHint}
                  onChange={() => {
                    setModelScope("inherited");
                    setModelsOpen(false);
                    clearModelsError();
                  }}
                />
                不限制模型（继承组织授权）
              </label>
              <label className="pick">
                <input
                  type="radio"
                  name={`${id}-model-scope`}
                  checked={modelScope === "selected"}
                  aria-invalid={errs.models ? true : undefined}
                  aria-describedby={modelsDescribedBy}
                  onChange={() => {
                    setModelScope("selected");
                    clearModelsError();
                  }}
                />
                指定模型
              </label>
            </div>
            {modelScope === "selected" && (
              <div className="grid min-w-0 gap-3">
                <p className="hint" aria-live="polite">
                  已选 {formatCount(picked.length)} 个模型
                </p>
                {noModels && !errs.models && <p className="hint" id={modelsEmpty}>{modelsEmptyMessage}</p>}
                <div>
                  <Button
                    size="sm"
                    aria-expanded={modelsOpen}
                    aria-controls={modelsList}
                    onClick={() => setModelsOpen((open) => !open)}
                  >
                    {modelsOpen ? "收起模型列表" : "展开模型列表"}
                  </Button>
                </div>
                <div id={modelsList} hidden={!modelsOpen}>
                  <div className="grid gap-3">
                    <Field label="搜索模型">
                      <input type="search" value={modelSearch} onChange={(e) => setModelSearch(e.target.value)} />
                    </Field>
                    {catalog === null ? (
                      <span className="hint">载入中…</span>
                    ) : catalog.length === 0 ? (
                      <span className="hint">组织还没有获授权的模型</span>
                    ) : visibleModels.length === 0 ? (
                      <span className="hint">没有匹配的模型</span>
                    ) : (
                      <div className="picks max-h-64 overflow-y-auto [overflow-wrap:anywhere]">
                        {visibleModels.map((m) => (
                          <label key={m.id} className="pick min-w-0">
                            <input
                              type="checkbox"
                              checked={picked.includes(m.id)}
                              onChange={(e) => {
                                const checked = e.target.checked;
                                setPicked((was) => checked ? [...was, m.id] : was.filter((p) => p !== m.id));
                                clearModelsError();
                              }}
                            />
                            {m.name}
                          </label>
                        ))}
                      </div>
                    )}
                  </div>
                </div>
              </div>
            )}
          </fieldset>
          <p className="hint" id={modelsHint}>
            不限制模型时，使用组织当前及后续获授权的模型；仍受组织权限和其他限制约束。
            {modelScope === "inherited" && catalog?.length === 0 && "当前无可用模型，组织后续授权后自动可用。"}
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

        <Field label="过期时间（北京时间）" wide hint="留空表示永不过期；按北京时间保存" error={errs.expires}>
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
        <div className="col-span-full flex justify-end gap-2 border-t border-ink/7 pt-5">
          <Button
            kind="primary"
            type="submit"
            busy={busy}
            disabled={noGroup || noModels}
            disabledReason={noModels ? modelsEmptyMessage : undefined}
          >
            新建密钥
          </Button>
        </div>
      </form>
    </Card>
  );
}
