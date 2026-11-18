import { useCallback, useEffect, useId, useRef, useState, type CSSProperties } from "react";
import { Pause, Play, Plus, Trash2 } from "lucide-react";
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
import { FormDialog } from "./ui/FormDialog";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field, useRefusalFocus } from "./ui/Field";
import { KeySecret } from "./ui/KeySecret";
import { Lamp, type LampState } from "./ui/Lamp";
import { TwoStep } from "./ui/TwoStep";
import { BentoGrid, Card, CardHeader, Metric, PageHeader, span } from "./components/page";
import { formatCount } from "./lib/chartFormat";
import { cn } from "./lib/cn";
import { beijingInputValue, fmtBeijing, parseBeijingInput } from "./lib/time";
import { keySettingsPatch } from "./lib/keySettings";
import { fetchKeyCosts, keyCostNotes, keyMonthRange, type KeyCosts } from "./lib/keyUsage";
import { fmtMicroCny } from "./lib/money";
import { isKeyBudgetAmount } from "./lib/keyBudget";
import { useRecordTable } from "./lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "./ui/RecordTable";
import { Select } from "./ui/Select";
import { Menu } from "./ui/Menu";
import "./Keys.css";

// 费用摘要与密钥表格分别通栏，宽表仅在表格区域内滚动。

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
  const [costs, setCosts] = useState<KeyCosts | null>(null);
  const [costRange, setCostRange] = useState(keyMonthRange);
  const [costErr, setCostErr] = useState<string | null>(null);
  const [catalog, setCatalog] = useState<PortalModel[] | null>(null);
  const [groups, setGroups] = useState<PortalGroup[] | null>(null);
  const [caps, setCaps] = useState<Caps | null>(null);
  const [people, setPeople] = useState<Member[] | null>(null);
  const [readErr, setReadErr] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);
  // The plaintext, in this page's state alone: it goes with a reload or a
  // change of page. The list's own 复制 reveals it again.
  const [minted, setMinted] = useState<string | null>(null);
  const createdResult = useRef<HTMLDivElement>(null);
  useEffect(() => { if (minted && createOpen) createdResult.current?.focus(); }, [minted, createOpen]);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // Which action on that key is in flight.
  const [acting, setActing] = useState<"switch" | "revoke" | "group" | null>(null);
  const loadTicket = useRef(0);

  const load = useCallback(async () => {
    const ticket = ++loadTicket.current;
    const range = keyMonthRange();
    setCostRange(range);
    setCosts(null);
    setCostErr(null);
    // 列表与已授权的批量费用并行；每次载入只有一个费用请求。
    await Promise.all([
      keyApi.list().then(items => { if (ticket === loadTicket.current) { setItems(items); setListErr(null); } }, error => { if (ticket === loadTicket.current) setListErr(failure(error)); }),
      fetchKeyCosts(range).then(costs => { if (ticket === loadTicket.current) setCosts(costs); }, error => { if (ticket === loadTicket.current) setCostErr(failure(error)); }),
    ]);
  }, []);

  useEffect(() => {
    void load();
    return () => { loadTicket.current += 1; };
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

  function creatorOf(k: TenantKey): string {
    if (k.created_by?.kind === "operator") return "平台管理员";
    if (k.created_by?.kind !== "member") return "未知";
    return k.created_by.display_name ?? (k.created_by.id === member.id ? "我" : k.created_by.id ? `成员 ${k.created_by.id}` : "成员");
  }

  const [stateFilter, setStateFilter] = useState("");
  const [groupFilter, setGroupFilter] = useState("");
  const table = useRecordTable({
    items: (items ?? []).filter((key) => (!stateFilter || stateOf(key).text === stateFilter) && (!groupFilter || key.group?.id === groupFilter)),
    searchText: (key) => [key.label, key.prefix, ownerOf(key), creatorOf(key), key.group?.display_name ?? ""],
    resetKey: `${stateFilter}/${groupFilter}`,
  });

  const visibleScope = manager ? "全组织密钥" : "我的密钥";
  const usableCount = items === null ? null : active ? items.filter(key => stateOf(key).text === "可用").length : 0;
  // 费用缺失不补零；摘要和列表使用同一批精确金额。
  const totalCost = items === null || costs === null ? null : items.reduce<bigint | null>((total, key) => {
    const amount = costs.items.get(key.id)?.chargedMicroCny;
    return total === null || amount === null || amount === undefined ? null : total + amount;
  }, 0n);

  return (
    <>
      <PageHeader
        actions={active && <Button kind="primary" onClick={() => { setMinted(null); setCreateOpen(true); }}><Plus aria-hidden />新建密钥</Button>}
        title="API 密钥"
        eyebrow={visibleScope}
        dataLead={`${visibleScope}，管理调用凭据、模型范围与使用限制。`}
      />
      <BentoGrid className="tenant-keys">
        <Card padding="sm" className={`${span.d12} keys-expense-summary`} aria-label="密钥统计">
          <dl className="keys-summary-grid">
            <div><dt>当前可见密钥</dt><dd><Metric>{items === null ? "—" : formatCount(items.length)}</Metric><p>{manager ? "组织内全部密钥" : "归属于我的密钥"}</p></dd></div>
            <div><dt>当前可用</dt><dd><Metric>{usableCount === null ? "—" : formatCount(usableCount)}</Metric><p>组织与密钥状态均可用</p></dd></div>
            <div><dt>当前不可用</dt><dd><Metric>{items === null || usableCount === null ? "—" : formatCount(items.length - usableCount)}</Metric><p>含停用、过期及分组不可用</p></dd></div>
            <div className="keys-summary-cost"><dt>本月已计价费用</dt><dd><Metric>{costErr ? "读取失败" : items === null || costs === null ? "—" : totalCost === null ? "未知" : fmtMicroCny(totalCost)}</Metric><p>仅统计当前可见密钥</p></dd></div>
          </dl>
          <p className="keys-summary-period">费用期间：{when(costs?.from ?? costRange.from)} 至 {when(costs?.to ?? costRange.to)}（UTC+8，不含结束时间）。待计价与处理中记录见列表；数量不受下方筛选影响。</p>
        </Card>
        <Card padding="table" className={cn(span.d12, "keys-list")}>
          <CardHeader number={3} title="密钥列表" description="复制或查看明文会重新获取密钥；吊销后立即失效，无法恢复。" actions={<Button onClick={() => void load()}>刷新列表</Button>} />
          <p className="hint px-5">组织密钥归组织所有；创建者被停用或移除后仍保留，不会自动吊销。</p>
          {costErr && <div className="note crit" role="alert">费用读取失败：{costErr}<Button onClick={() => void load()}>重新读取费用</Button></div>}
          <RecordTableToolbar search={{ value: table.query, onChange: table.setQuery, label: "搜索密钥", placeholder: "名称、公开前缀、归属、创建者或分组" }}>
            <Field label="密钥状态"><Select value={stateFilter} onValueChange={setStateFilter} size="compact"><option value="">全部状态</option>{["可用", "已被平台停用", "已停用", "已过期"].map((value) => <option key={value}>{value}</option>)}</Select></Field>
            <Field label="密钥分组"><Select value={groupFilter} onValueChange={setGroupFilter} size="compact"><option value="">全部分组</option>{(groups ?? []).map((group) => <option key={group.id} value={group.id}>{group.display_name}</option>)}</Select></Field>
          </RecordTableToolbar>
          {!active && <div className="note crit">组织已被停用，所有密钥暂时无法调用；这里只能吊销密钥。</div>}
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
            <DataTable label="密钥列表">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>归属</th>
                  <th>创建者</th>
                  <th>模型</th>
                  <th>分组</th>
                  <th>限制</th>
                  <th className="num">本月费用</th>
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
                  const cost = costs?.items.get(k.id);
                  const detailHref = `#keys?${new URLSearchParams({ key: k.id, from: costs?.from ?? costRange.from, to: costs?.to ?? costRange.to })}`;
                  return (
                    <tr key={k.id}>
                      <td data-label="名称">
                        <span className="key-name" title={k.label}>
                          <a className="key-detail-link" href={detailHref}>{k.label}</a>
                        </span>
                        <span className="key-prefix">
                          <KeySecret
                            prefix={k.prefix}
                            className="acts inline-flex shrink-0"
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
                        </span>
                      </td>
                      <td data-label="归属">
                        <span className="trunc" style={{ "--trunc": "128px" } as CSSProperties} title={owner}>
                          {owner}
                        </span>
                      </td>
                      <td data-label="创建者"><span className="trunc [--trunc:128px]" title={creatorOf(k)}>{creatorOf(k)}</span></td>
                      <td data-label="模型">
                        <span className="trunc [--trunc:120px] min-[1800px]:[--trunc:216px]" title={models}>
                          {models}
                        </span>
                      </td>
                      <td data-label="分组">
                        <GroupCell
                          k={k}
                          groups={groups}
                          active={active}
                          busy={busy === k.id}
                          acting={acting}
                          onChoose={(groupId) => void act(k, "group", () => keyApi.setGroup(k.id, groupId, k.version))}
                        />
                      </td>
                      <td className="tabular" data-label="限制">
                        <span className="block">{limitsOf(k)}</span>
                      </td>
                      <td className="num tabular" data-label="已用费用">
                        <span className="block">{costErr ? "读取失败" : costs === null ? "读取中…" : cost === undefined ? "未读取" : cost.chargedMicroCny === null ? "未知" : fmtMicroCny(cost.chargedMicroCny)}</span>
                        {cost && keyCostNotes(cost).map(note => <span className="hint cell-note" key={note}>{note}</span>)}
                      </td>
                      <td data-label="状态">
                        <Lamp state={state.lamp}>{state.text}</Lamp>
                        {state.note && (
                          <span className="hint cell-note max-w-[8rem] whitespace-normal min-[1800px]:max-w-none">{state.note}</span>
                        )}
                      </td>
                      <td className="right" data-label="操作">
                        <span className="acts">
                          <Menu label={`更多操作：${k.label}`} disabled={busy === k.id} items={[
                            { label: "查看用量", onSelect: () => { window.location.hash = detailHref; } },
                            ...(active ? [{ label: "修改密钥", onSelect: () => { window.location.hash = `${detailHref}&edit=1`; } }] : []),
                          ]} />
                          {/* The organisation's own switch, which the platform's overrides. */}
                          {active && !k.disabled_by_platform && (
                            <Button
                              size="sm"
                              kind="quiet"
                              iconOnly
                              className="tenant-icon-action"
                              aria-label={k.disabled ? "启用" : "停用"}
                              title={k.disabled ? "启用" : "停用"}
                              disabled={busy === k.id}
                              busy={busy === k.id && acting === "switch"}
                              busyLabel={k.disabled ? "启用" : "停用"}
                              onClick={() => void act(k, "switch", () => keyApi.setDisabled(k.id, !k.disabled, k.version))}
                            >
                              {k.disabled ? <Play aria-hidden="true" /> : <Pause aria-hidden="true" />}
                            </Button>
                          )}
                          <TwoStep
                            label="吊销"
                            size="sm"
                            kind="quiet"
                            icon={<Trash2 aria-hidden="true" />}
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
        {active && <FormDialog open={createOpen} onOpenChange={(next) => { setCreateOpen(next); if (!next) setMinted(null); }} busy={createBusy} title={minted ? "密钥已创建" : "新建密钥"} description="先选择计价分组，再设置模型范围与限制。留空的限制沿用组织上限。">
          {minted ? <div ref={createdResult} tabIndex={-1} aria-label="密钥已创建" className="key-created-result"><ShownOnce title="密钥已创建" masked value={minted} onDone={() => { setCreateOpen(false); setMinted(null); }}><span className="hint">密钥已隐藏，复制后即可使用；之后也可以在密钥列表里重新复制。</span></ShownOnce></div> : <KeyForm
            me={member}
            manager={manager}
            catalog={catalog}
            groups={groups}
            caps={caps}
            people={people}
            readErr={readErr}
            onInactive={onSelfChanged}
            onBusyChange={setCreateBusy}
            onCancel={() => setCreateOpen(false)}
            onCreated={(plaintext) => {
              setMinted(plaintext);
              void load();
            }}
          />}
        </FormDialog>}
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
        <Select aria-label="分组" value={choice} onValueChange={setChoice} size="compact">
          <option value="">请选择</option>
          {(groups ?? []).map((g) => (
            <option key={g.id} value={g.id}>
              {g.display_name}
            </option>
          ))}
        </Select>
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

export function stateOf(k: TenantKey): { text: string; lamp: LampState; note?: string } {
  if (k.disabled_by_platform) return { text: "已被平台停用", lamp: "disabled", note: "只有平台可以重新启用" };
  if (k.disabled) return { text: "已停用", lamp: "disabled" };
  if (k.expires_at && Date.parse(k.expires_at) <= Date.now()) return { text: "已过期", lamp: "disabled" };
  // A key with no group, or whose group its organisation no longer has, is
  // closed by the gateway whatever its switches say; the group cell says why.
  if (!k.group_available) return { text: "已停用", lamp: "disabled" };
  return { text: "可用", lamp: "applied", note: k.expires_at ? `${when(k.expires_at)} 到期` : undefined };
}

type Field = "label" | "group" | "models" | "concurrency" | "rpm" | "expires" | "owner" | "budget";
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
  if (path === "/budget" || path.startsWith("/budget/")) return "budget";
  return null;
}

/** A limit as typed: none when empty, a whole number, or why it is neither. */
function limit(raw: string): { value?: number; problem?: string } {
  const text = raw.trim();
  if (text === "") return {};
  if (!/^\d+$/.test(text)) return { problem: "请填整数；留空表示不单独限制" };
  const value = Number(text);
  return Number.isSafeInteger(value) && value <= 4_294_967_295 ? { value } : { problem: "请填 0 到 4,294,967,295 之间的整数；留空表示不单独限制" };
}

function capHint(cap: number | undefined, caps: Caps | null): string {
  if (caps === null) return "组织上限载入中…";
  return cap === undefined ? "组织未设上限；留空表示不限" : `组织上限 ${count(cap)}；留空则按此上限`;
}

export function KeyForm({
  me,
  manager,
  catalog,
  groups,
  caps,
  people,
  readErr,
  onInactive,
  onCreated,
  onBusyChange,
  onCancel,
  initial,
  onSaved,
  onConflict,
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
  onCreated?: (plaintext: string) => void;
  initial?: TenantKey;
  onSaved?: (key: TenantKey) => void;
  onConflict?: () => void;
  onBusyChange: (busy: boolean) => void;
  onCancel: () => void;
}) {
  const id = useId();
  const [label, setLabel] = useState(initial?.label ?? "");
  // The group the key is bought in: what it is priced at, so it is chosen
  // with the key and never changed afterwards.
  const [group, setGroup] = useState(initial?.group?.id ?? "");
  const [modelScope, setModelScope] = useState<"inherited" | "selected">(initial && initial.model_ids !== null ? "selected" : "inherited");
  const [modelsOpen, setModelsOpen] = useState(false);
  const [modelSearch, setModelSearch] = useState("");
  const [picked, setPicked] = useState<string[]>(initial?.model_ids ?? []);
  const [concurrency, setConcurrency] = useState(initial?.rate_limit.concurrency?.toString() ?? "");
  const [rpm, setRpm] = useState(initial?.rate_limit.rpm?.toString() ?? "");
  const [expires, setExpires] = useState(initial?.expires_at ? beijingInputValue(new Date(initial.expires_at)) : "");
  const [budgetAmount, setBudgetAmount] = useState("");
  const [budgetPeriod, setBudgetPeriod] = useState<"cumulative" | "day" | "week" | "month">("cumulative");
  const [disabled, setDisabled] = useState(initial?.disabled ?? false);
  // Empty: the caller.
  const [owner, setOwner] = useState("");
  const [errs, setErrs] = useState<FieldErrors>({});
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const noModels = !initial && modelScope === "selected" && picked.length === 0;
  const modelsEmptyMessage = "请至少选择一个模型，或改为不限制模型";

  // One group is the organisation's only choice, so it is preselected; it is
  // still shown, since it is what the key is priced at.
  useEffect(() => {
    if (!initial?.group && groups !== null && groups.length === 1) setGroup(groups[0]?.id ?? "");
  }, [groups, initial?.group]);

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
    const amount = budgetAmount.trim();
    if (!initial && amount !== "" && !isKeyBudgetAmount(amount)) found.budget = "请输入大于 0 的金额，最多两位小数";
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
    if (!initial && amount !== "") body.budget = budgetPeriod === "cumulative"
      ? { kind: "cumulative", limit_cny: amount }
      : { kind: "periodic", period: budgetPeriod, limit_cny: amount };
    setBusy(true);
    onBusyChange(true);
    try {
      if (initial) {
        const patch = keySettingsPatch(initial, {
          label, modelIds: modelScope === "inherited" ? null : picked, groupId: group,
          rateLimit: { concurrency: c.value, rpm: r.value }, disabled, expiresAt: expiresAt ?? null,
        });
        const updated = Object.keys(patch).length === 0 ? initial : await keyApi.update(initial.id, patch, initial.version);
        onSaved?.(updated);
        return;
      }
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
      setBudgetAmount("");
      setBudgetPeriod("cumulative");
      setOwner("");
      onCreated?.(plaintext);
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
        setErr(initial ? "平台暂时无法修改密钥，请稍后再试" : "平台暂时无法创建密钥，请稍后再试");
      } else if (e instanceof ApiError && e.code === "version_mismatch") {
        setErr("该密钥已被他人修改。请重新载入最新配置后再修改，未保存的内容将被替换。");
      } else if (e instanceof ApiError && e.code === "tenant_not_active") {
        setErr(messageFor(e));
        onInactive();
      } else {
        setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
      }
    } finally {
      setBusy(false);
      onBusyChange(false);
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
  const unavailableModels = catalog === null ? [] : picked.filter(modelId => !catalog.some(model => model.id === modelId));
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
  const groupDescribedBy = [initial?.group || (groups !== null && groups.length > 0) ? groupHint : null, errs.group ? groupErr : null]
    .filter(Boolean)
    .join(" ");
  // With no group assigned the form cannot create anything: the button says
  // why and the submit never reaches the server to earn its refusal.
  const noGroup = !initial?.group && groups !== null && groups.length === 0;

  return (
    <Card className={cn(span.d12, "tenant-key-create")}>
      <CardHeader number={4} title={initial ? "密钥配置" : "新建密钥"} description={initial ? "修改调用范围与限制。未填写的限制沿用组织上限。" : "先选择计价分组，再设置模型范围与限制。未填写的限制沿用组织上限。"} />
      {readErr && (
        <div className="note crit" role="alert">
          {readErr}
        </div>
      )}
      <form ref={form} className="tenant-form tenant-key-form grid min-w-0 gap-x-7 gap-y-5 tablet:grid-cols-2" onSubmit={submit} noValidate>
        <Field label="名称" error={errs.label}>
          {(aria) => <input value={label} onChange={(e) => setLabel(e.target.value)} {...aria} />}
        </Field>

        <Field label="过期时间" hint="留空表示永不过期；时间采用 UTC+8" error={errs.expires}>
          {(aria) => (
            <input type="datetime-local" value={expires} onChange={(e) => setExpires(e.target.value)} {...aria} />
          )}
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
            {initial?.group ? (
              <p>{initial.group.display_name}（只读）</p>
            ) : groups === null ? (
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
          {(initial?.group || (groups !== null && groups.length > 0)) && (
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
            <div className="key-model-scope-options">
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
                  已选 {formatCount(picked.length)} 个模型{initial && picked.length === 0 ? "，该密钥无法调用任何模型" : ""}
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
                    {unavailableModels.length > 0 && <div className="picks [overflow-wrap:anywhere]">
                      {unavailableModels.map(modelId => <label key={modelId} className="pick min-w-0">
                        <input type="checkbox" checked onChange={() => setPicked(was => was.filter(value => value !== modelId))} />
                        已不在组织授权内（{modelId}）
                      </label>)}
                    </div>}
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

        {!initial && <>
          <Field label="费用额度（元）" hint="留空表示不限制；仍受组织、成员和平台额度约束。" error={errs.budget}>
            {(aria) => <input inputMode="decimal" autoComplete="off" value={budgetAmount} disabled={busy} onChange={(event) => setBudgetAmount(event.target.value)} {...aria} />}
          </Field>
          <Field label="额度周期" hint="累计额度从创建时开始统计；周期额度按 UTC 自然周期统计。">
            <Select value={budgetPeriod} disabled={busy || budgetAmount.trim() === ""} onValueChange={(value) => setBudgetPeriod(value as typeof budgetPeriod)} options={[
              { value: "cumulative", label: "累计" }, { value: "day", label: "每天" }, { value: "week", label: "每周" }, { value: "month", label: "每月" },
            ]} />
          </Field>
        </>}

        {initial && <Field label="密钥状态" hint={initial.disabled_by_platform ? "平台已停用此密钥，只有平台可以重新启用。" : "停用后无法调用，重新启用仍受组织授权和额度约束。"}>
          <Select value={disabled ? "disabled" : "enabled"} disabled={initial.disabled_by_platform} onValueChange={value => setDisabled(value === "disabled")}>
            <option value="enabled">启用</option><option value="disabled">停用</option>
          </Select>
        </Field>}
        {initial ? <div className="field"><span className="f-label">归属成员</span><p>{initial.owner === null ? "组织" : initial.owner.id === me.id ? "我" : initial.owner.display_name ?? "成员"}（只读）</p><p className="hint">密钥创建后不能更换归属成员；如需更换，请新建密钥。</p></div> : manager && (
          <Field label="归属成员" error={errs.owner}>
            {(aria) => (
              <Select value={owner} onValueChange={setOwner} {...aria}>
                <option value="">我</option>
                {choices.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.display_name ?? p.email}
                    {p.disabled ? "（已停用）" : ""}
                  </option>
                ))}
              </Select>
            )}
          </Field>
        )}

        {err && (
          <div className="note crit col-span-full" role="alert">
            {err}
            {err.startsWith("该密钥已被他人修改") && onConflict && <Button onClick={onConflict}>重新载入最新配置</Button>}
          </div>
        )}
        <div className="tenant-form-actions col-span-full">
          <Button onClick={onCancel} disabled={busy}>取消</Button>
          <Button
            kind="primary"
            type="submit"
            busy={busy}
            disabled={noGroup || noModels}
            disabledReason={noModels ? modelsEmptyMessage : undefined}
          >
            {initial ? "保存修改" : "新建密钥"}
          </Button>
        </div>
      </form>
    </Card>
  );
}
