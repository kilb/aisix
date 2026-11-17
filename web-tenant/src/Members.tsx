import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowUpRight, MailPlus, X } from "lucide-react";
import { Tabs } from "radix-ui";
import {
  ApiError,
  budgets as budgetApi,
  members as memberApi,
  type Budget,
  type Invite,
  type IssuedInvite,
  type Member,
  type MemberRole,
  type SessionMember,
  type TenantStatus,
} from "./lib/api";
import { memberUsage, type MemberUsageSummary } from "./lib/memberUsage";
import { memberChargeLabel } from "./lib/memberUsagePresentation";
import { fmtMicroCny } from "./lib/money";
import { fmtBeijing } from "./lib/time";
import { useRecordTable } from "./lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "./ui/RecordTable";
import { formMessageFor, messageFor } from "./lib/errors";
import { ShownOnce } from "./controls";
import { FormDialog } from "./ui/FormDialog";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { Select } from "./ui/Select";
import { ROLE_LABEL } from "./Org";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "./components/page";
import { exactRatio, formatCount } from "./lib/chartFormat";
import { rankCurrentMemberActivity, rankCurrentMemberCosts } from "./lib/memberBento";
import { CreateMemberDialog } from "./CreateMemberDialog";
import "./Members.css";

// 成员与邀请共用一个列表区域；统计始终使用同一次批量用量响应。

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

function BudgetCell({ budgets }: { budgets: Budget[] }) {
  if (budgets.length === 0) return <span className="hint">不限制</span>;
  const period = { day: "每日", week: "每周", month: "每月" };
  return <div className="grid gap-1">{budgets.map((budget) => (
    <span className="tabular" key={budget.id}>{budget.kind === "cumulative" ? "累计" : period[budget.period ?? "month"]} {fmtMicroCny(budget.limit_micro_cny)}</span>
  ))}</div>;
}

/**
 * The link an invitee opens: the control plane's when it builds one, else
 * this portal's own invitation address, the token in its fragment, which a
 * browser never sends to a server (main.tsx reads it there).
 */
function inviteLink(issued: IssuedInvite): string {
  return (
    issued.invite_url ??
    `${window.location.origin}${import.meta.env.BASE_URL}invite#token=${encodeURIComponent(issued.invite_token)}`
  );
}

/** 成员列表只列摘要；权限、用量和额度管理在独立成员详情页。 */
export function Members({
  member,
  status,
  onSelfChanged,
  onOpenMember,
}: {
  member: SessionMember;
  status: TenantStatus;
  onSelfChanged: () => void;
  onLeft: () => void;
  onOpenMember?: (id: string) => void;
}) {
  const active = status === "active";
  const isOwner = member.role === "owner";
  const canCreate = active && (isOwner || member.role === "admin");
  const roles: MemberRole[] = isOwner ? ["owner", "admin", "member", "billing"] : ["admin", "member", "billing"];
  const [people, setPeople] = useState<Member[] | null>(null);
  const [listErr, setListErr] = useState<string | null>(null);
  const [invites, setInvites] = useState<Invite[] | null>(null);
  const [invitesErr, setInvitesErr] = useState<string | null>(null);
  const [usage, setUsage] = useState<MemberUsageSummary | null>(null);
  const [usageErr, setUsageErr] = useState<string | null>(null);
  const [budgets, setBudgets] = useState<Budget[] | null>(null);
  const [budgetsErr, setBudgetsErr] = useState<string | null>(null);
  const loadId = useRef(0);
  const [issued, setIssued] = useState<{ email: string; link: string; expiresAt: string } | null>(null);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [inviteBusy, setInviteBusy] = useState(false);
  const issuedResult = useRef<HTMLDivElement>(null);
  useEffect(() => { if (issued && inviteOpen) issuedResult.current?.focus(); }, [issued, inviteOpen]);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [createdNotice, setCreatedNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // 邀请撤销请求进行中时，保留按钮反馈。
  const [acting, setActing] = useState<"revoke" | null>(null);
  const [roleFilter, setRoleFilter] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [inviteRole, setInviteRole] = useState("");
  const [directoryTab, setDirectoryTab] = useState("members");
  const memberTable = useRecordTable({
    items: (people ?? []).filter((person) => (!roleFilter || person.role === roleFilter) && (!statusFilter || (person.disabled ? "disabled" : "active") === statusFilter)),
    searchText: (person) => [person.email, person.display_name ?? ""],
    resetKey: `${roleFilter}/${statusFilter}`,
  });
  const inviteTable = useRecordTable({
    items: (invites ?? []).filter((invite) => !inviteRole || invite.role === inviteRole),
    searchText: (invite) => [invite.email, invite.invited_by?.display_name ?? "平台"],
    resetKey: inviteRole,
  });
  const usageByMember = useMemo(() => new Map(usage?.items.map((row) => [row.member_id, row]) ?? []), [usage]);
  const budgetsByMember = useMemo(() => {
    const grouped = new Map<string, Budget[]>();
    for (const budget of budgets ?? []) {
      if (budget.scope !== "member") continue;
      const rows = grouped.get(budget.target.id) ?? [];
      rows.push(budget);
      grouped.set(budget.target.id, rows);
    }
    return grouped;
  }, [budgets]);
  const rankedUsage = useMemo(() => {
    if (people === null || usage === null) return null;
    return rankCurrentMemberCosts(people, usage.items);
  }, [people, usage]);
  const totalCost = rankedUsage?.reduce((total, row) => total + row.amount, 0n) ?? null;
  const rankedActivity = useMemo(() => {
    if (people === null || usage === null) return null;
    return rankCurrentMemberActivity(people, usage.items);
  }, [people, usage]);
  const totalRequests = rankedActivity?.reduce((total, row) => total + row.requests, 0n) ?? null;

  const load = useCallback(async () => {
    const id = ++loadId.current;
    setUsage(null);
    setBudgets(null);
    setUsageErr(null);
    setBudgetsErr(null);
    async function read<T>(fetch: () => Promise<T>, success: (value: T) => void, refused: (message: string | null) => void) {
      try {
        const value = await fetch();
        if (id !== loadId.current) return;
        success(value);
        refused(null);
      } catch (e) {
        if (id === loadId.current) refused(failure(e));
      }
    }
    await Promise.all([
      read(memberApi.list, setPeople, setListErr),
      ...(active ? [
        read(memberUsage, setUsage, setUsageErr),
        read(budgetApi.list, setBudgets, setBudgetsErr),
        read(memberApi.invites, setInvites, setInvitesErr),
      ] : []),
    ]);
  }, [active]);

  // Read again when the caller's own role changes (the session re-read after
  // a change to their own row): what they may see and do changed with it.
  useEffect(() => {
    void load();
    return () => { loadId.current += 1; };
  }, [load, member.role]);

  async function act(key: string, what: "revoke", change: () => Promise<unknown>) {
    setBusy(key);
    setActing(what);
    setActionErr(null);
    try {
      await change();
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onSelfChanged();
      setActionErr(failure(e));
    } finally {
      setBusy(null);
      setActing(null);
    }
    await load();
  }

  return (
    <div className="tenant-members">
      <PageHeader
        title="成员"
        dataLead="查看成员使用情况，管理组织权限与独立额度。"
        actions={<>{active && <Button onClick={() => { setIssued(null); setInviteOpen(true); }}><MailPlus aria-hidden="true" />邀请成员</Button>}{canCreate && <CreateMemberDialog
          roles={roles}
          labels={ROLE_LABEL}
          onInactive={onSelfChanged}
          onCreated={(created) => {
            setCreatedNotice(`账号已创建：${created.email}，可使用邮箱和初始密码登录。`);
            void load();
          }}
        />}</>}
      />
      {createdNotice && <div className="note ok mb-3" role="status">{createdNotice}</div>}
      <BentoGrid>
        <Card padding="sm" className={`${span.d3} members-stat`} aria-label="成员数量">
          <p className="members-stat-label">成员数量</p>
          {listErr ? <p className="hint">读取失败</p> : people === null ? <MetricSkeleton /> : <Metric>{formatCount(people.length)}</Metric>}
          <p className="members-stat-note">当前组织内的所有成员</p>
        </Card>
        <Card padding="sm" className={`${span.d3} members-stat`} aria-label="本月活跃成员">
          <p className="members-stat-label">本月活跃成员</p>
          {!active ? <p className="hint">组织未启用</p> : usageErr || listErr ? <p className="hint">读取失败</p> : usage === null || people === null ? <MetricSkeleton /> : <Metric>{rankedActivity === null ? "未知" : formatCount(rankedActivity.length)}</Metric>}
          <p className="members-stat-note">本月有至少 1 次已记录调用</p>
        </Card>
        <Card padding="sm" className={`${span.d3} members-stat`} aria-label="待接受邀请数量">
          <p className="members-stat-label">待接受邀请</p>
          {!active ? <p className="hint">组织未启用</p> : invitesErr ? <p className="hint">读取失败</p> : invites === null ? <MetricSkeleton /> : <Metric>{formatCount(invites.length)}</Metric>}
          <p className="members-stat-note">接受邀请后才会成为成员</p>
        </Card>
        <Card tone="lilac" padding="sm" className={`${span.d3} members-stat members-cost-stat`} aria-label="成员本月费用">
          <p className="members-stat-label">本月成员已计费费用</p>
          {!active ? <p className="hint">组织未启用</p> : usageErr || listErr ? <p className="hint">读取失败</p> : usage === null || people === null ? <MetricSkeleton /> : <Metric>{totalCost === null ? "未知" : fmtMicroCny(totalCost)}</Metric>}
          <p className="members-stat-note">仅当前成员；待计价费用不计入</p>
        </Card>
        {active && <>
          <div className={`${span.d12} members-window`} role="note" aria-label="成员统计口径">
            <p>{usage ? <>{fmtBeijing(usage.from)} 至 {fmtBeijing(usage.to)}（UTC+8，不含结束时刻）</> : "统计窗口：UTC+8 月初至当前时间"}</p>
            <p>活跃按已记录调用判断，与账号启停无关。费用含调整，可能含估算用量，不含已离开成员和无成员归属的调用。</p>
          </div>
          <Card className={`${span.d6} members-ranking-card`} aria-label="成员费用排行榜">
            <CardHeader title="费用排行榜" description="本月已计费费用最高的 5 位当前成员" />
            {usageErr || listErr ? <CardMessage onRetry={() => void load()}>成员费用读取失败，请重试。</CardMessage> : usage === null || people === null ? <Loading /> : rankedUsage === null ? <CardMessage>费用数据不完整，暂时无法排名。</CardMessage> : rankedUsage.length === 0 ? <CardMessage>暂无成员费用数据。</CardMessage> : <MemberRanking rows={rankedUsage.slice(0, 5)} unit="cost" onOpenMember={onOpenMember} />}
          </Card>
          <Card className={`${span.d6} members-ranking-card members-activity-ranking`} aria-label="成员活跃排行榜">
            <CardHeader title="活跃排行榜" description="按本月已记录调用次数排序，展示前 5 位" />
            {usageErr || listErr ? <CardMessage onRetry={() => void load()}>成员调用数据读取失败，请重试。</CardMessage> : usage === null || people === null ? <Loading /> : rankedActivity === null ? <CardMessage>调用数据不完整，暂时无法排名。</CardMessage> : rankedActivity.length === 0 ? <CardMessage>本月暂无有已记录调用的成员。</CardMessage> : <MemberRanking rows={rankedActivity.slice(0, 5).map((row) => ({ ...row, amount: row.requests }))} unit="requests" onOpenMember={onOpenMember} />}
            {totalRequests !== null && !usageErr && !listErr && <p className="members-ranking-foot">当前成员本月共 {formatCount(totalRequests)} 次已记录调用</p>}
          </Card>
        </>}
        <Card padding="table" className={`${span.d12} members-directory`}>
          <Tabs.Root value={active ? directoryTab : "members"} onValueChange={setDirectoryTab}>
            <Tabs.List className="members-tabs" aria-label="成员与邀请">
              <Tabs.Trigger value="members">组织成员{people !== null && !listErr && <span>{formatCount(people.length)}</span>}</Tabs.Trigger>
              {active && <Tabs.Trigger value="invites">待接受邀请{invites !== null && !invitesErr && <span>{formatCount(invites.length)}</span>}</Tabs.Trigger>}
            </Tabs.List>
            {!active && <div className="note crit">组织已被停用；这里只能移除成员。</div>}
            {actionErr && <div className="note crit" role="alert">{actionErr}</div>}
            <Tabs.Content value="members" forceMount hidden={active && directoryTab !== "members"} className="members-tab-panel">
              <RecordTableToolbar search={{ value: memberTable.query, onChange: memberTable.setQuery, label: "搜索成员", placeholder: "显示名或邮箱" }}>
                <Field label="成员角色"><Select size="compact" value={roleFilter} onValueChange={setRoleFilter} options={[{ value: "", label: "全部角色" }, ...(["owner", "admin", "member", "billing"] as const).map((role) => ({ value: role, label: ROLE_LABEL[role] }))]} /></Field>
                <Field label="成员状态"><Select size="compact" value={statusFilter} onValueChange={setStatusFilter} options={[{ value: "", label: "全部状态" }, { value: "active", label: "正常" }, { value: "disabled", label: "已被平台停用" }]} /></Field>
              </RecordTableToolbar>
              {active && usageErr && <div className="note crit" role="alert">成员用量读取失败：{usageErr}</div>}
              {active && budgetsErr && <div className="note crit" role="alert">成员额度读取失败：{budgetsErr}</div>}
              {listErr && <div className="note crit" role="alert">{listErr}</div>}
              {people === null ? !listErr && <Loading /> : memberTable.rows.length === 0 ? <EmptyState mark="route" text={people.length === 0 ? "组织内还没有成员。" : "没有符合筛选条件的成员。"} /> : <DataTable label="组织成员" className="members-table">
                <thead><tr><th>成员</th><th>角色</th><th>状态</th><th>加入时间</th><th className="num">本月调用</th><th className="num">本月费用</th><th className="num">成员额度</th><th><span className="vh">操作</span></th></tr></thead>
                <tbody>{memberTable.rows.map((m) => {
                  const row = usageByMember.get(m.id);
                  const detailLabel = `查看 ${m.display_name ?? m.email} 的成员详情`;
                  return <tr key={m.id}>
                    <td data-label="成员"><a className="members-name" href={`#members?member=${encodeURIComponent(m.id)}`} aria-label={`${m.email} 的成员详情`} onClick={(event) => { if (onOpenMember) { event.preventDefault(); onOpenMember(m.id); } }} title={m.display_name ?? m.email}>{m.display_name ?? m.email}{m.id === member.id && <span className="self">（我）</span>}</a><span className="dt-cell-secondary" title={m.email}>{m.email}</span></td>
                    <td data-label="角色">{ROLE_LABEL[m.role] ?? m.role}</td>
                    <td data-label="状态"><Lamp state={m.disabled ? "disabled" : "applied"}>{m.disabled ? "已被平台停用" : "正常"}</Lamp></td>
                    <td data-label="加入时间"><time dateTime={m.created_at} title={`${fmtBeijing(m.created_at)}（UTC+8）`}>{fmtBeijing(m.created_at).slice(0, 10)}</time></td>
                    <td className="num tabular" data-label="本月调用">{!active ? "—" : usageErr ? "读取失败" : usage === null ? "载入中…" : formatCount(row?.requests)}</td>
                    <td className="num" data-label="本月费用">{!active ? <span className="hint">组织未启用</span> : usageErr ? <span className="hint">读取失败</span> : usage === null ? <span className="hint">载入中…</span> : <span className="tabular">{memberChargeLabel(row)}</span>}</td>
                    <td className="num" data-label="成员额度">{!active ? <span className="hint">组织未启用</span> : budgetsErr ? <span className="hint">读取失败</span> : budgets === null ? <span className="hint">载入中…</span> : <BudgetCell budgets={budgetsByMember.get(m.id) ?? []} />}</td>
                    <td className="right" data-label="操作"><a className="members-detail-action" href={`#members?member=${encodeURIComponent(m.id)}`} aria-label={detailLabel} title={detailLabel} onClick={(event) => { if (onOpenMember) { event.preventDefault(); onOpenMember(m.id); } }}><ArrowUpRight aria-hidden="true" /></a></td>
                  </tr>;
                })}</tbody>
              </DataTable>}
              {people !== null && <RecordTablePager page={memberTable.page} pageSize={memberTable.pageSize} onPageChange={memberTable.setPage} onPageSizeChange={memberTable.setPageSize} total={memberTable.filteredCount} label="成员" />}
            </Tabs.Content>
            {active && <Tabs.Content value="invites" forceMount hidden={directoryTab !== "invites"} className="members-tab-panel">
              <p className="members-invite-description">邀请在被接受后才会成为组织成员。</p>
              {invitesErr && <div className="note crit" role="alert">{invitesErr}</div>}
              <RecordTableToolbar search={{ value: inviteTable.query, onChange: inviteTable.setQuery, label: "搜索邀请", placeholder: "邮箱或邀请人" }}>
                <Field label="邀请角色"><Select size="compact" value={inviteRole} onValueChange={setInviteRole} options={[{ value: "", label: "全部角色" }, ...roles.map((role) => ({ value: role, label: ROLE_LABEL[role] }))]} /></Field>
              </RecordTableToolbar>
              {invites === null ? !invitesErr && <Loading /> : inviteTable.rows.length === 0 ? <EmptyState mark="mail" text={invites.length === 0 ? "没有待接受的邀请。" : "没有符合筛选条件的邀请。"} /> : <DataTable label="待接受的邀请" className="members-invites-table">
                <thead><tr><th>邮箱</th><th>角色</th><th>邀请人</th><th>邀请时间</th><th>有效期至</th><th><span className="vh">操作</span></th></tr></thead>
                <tbody>{inviteTable.rows.map((inv) => {
                  const by = inv.invited_by === null ? "平台" : (inv.invited_by.display_name ?? "成员");
                  return <tr key={inv.id}>
                    <td data-label="邮箱"><span className="dt-cell-primary font-bold" title={inv.email}>{inv.email}</span></td>
                    <td data-label="角色">{ROLE_LABEL[inv.role] ?? inv.role}</td>
                    <td data-label="邀请人"><span className="trunc" title={by}>{by}</span></td>
                    <td className="tabular" data-label="邀请时间">{fmtBeijing(inv.created_at).slice(0, 10)}</td>
                    <td className="tabular" data-label="有效期至">{fmtBeijing(inv.expires_at)}</td>
                    <td className="right" data-label="操作">{(isOwner || inv.role !== "owner") && <Button size="sm" iconOnly disabled={busy !== null} busy={busy === inv.id && acting === "revoke"} busyLabel="撤销邀请中…" aria-label={`撤销 ${inv.email} 的邀请`} title={`撤销 ${inv.email} 的邀请`} onClick={() => void act(inv.id, "revoke", () => memberApi.revokeInvite(inv.id))}><X aria-hidden="true" /></Button>}</td>
                  </tr>;
                })}</tbody>
              </DataTable>}
              {invites !== null && <RecordTablePager page={inviteTable.page} pageSize={inviteTable.pageSize} onPageChange={inviteTable.setPage} onPageSizeChange={inviteTable.setPageSize} total={inviteTable.filteredCount} label="邀请" />}
            </Tabs.Content>}
          </Tabs.Root>
        </Card>
      </BentoGrid>
      {active && <FormDialog open={inviteOpen} onOpenChange={(next) => { setInviteOpen(next); if (!next) setIssued(null); }} busy={inviteBusy} title={issued ? "邀请已创建" : "邀请成员"} description="创建邀请链接，由新成员自行设置登录口令。">
        {issued ? <div ref={issuedResult} tabIndex={-1} className="key-created-result" aria-label="邀请已创建"><ShownOnce title="邀请链接只显示这一次" masked={false} value={issued.link} onDone={() => { setInviteOpen(false); setIssued(null); }}><span className="hint">发给 {issued.email}，{fmtBeijing(issued.expiresAt)} 前有效。</span></ShownOnce></div> : <InviteForm roles={roles} onBusyChange={setInviteBusy} onInactive={onSelfChanged} onIssued={(email, invite) => { setIssued({ email, link: inviteLink(invite), expiresAt: invite.expires_at }); void load(); }} />}
      </FormDialog>}
    </div>
  );
}

/** 图形只投影精确金额/调用数的比例，标签仍显示未缩写的真实值。 */
function MemberRanking({ rows, unit, onOpenMember }: {
  rows: readonly { id: string; label: string; amount: bigint }[];
  unit: "cost" | "requests";
  onOpenMember?: (id: string) => void;
}) {
  const maximum = rows.reduce((max, row) => row.amount > max ? row.amount : max, 0n);
  return <ol className="members-ranking">
    {rows.map((row, index) => <li key={row.id}>
      <span className="members-rank-position" aria-label={`第 ${index + 1} 名`}>{index + 1}</span>
      <div className="members-rank-entry">
        <div className="members-rank-line"><a href={`#members?member=${encodeURIComponent(row.id)}`} title={row.label} onClick={(event) => { if (onOpenMember) { event.preventDefault(); onOpenMember(row.id); } }}>{row.label}</a><span className="tabular">{unit === "cost" ? fmtMicroCny(row.amount) : `${formatCount(row.amount)} 次`}</span></div>
        <div className="members-rank-track" aria-hidden="true"><span style={{ width: `${(exactRatio(row.amount, maximum) ?? 0) * 100}%` }} /></div>
      </div>
    </li>)}
  </ol>;
}

function InviteForm({
  roles,
  onInactive,
  onIssued,
  onBusyChange,
}: {
  roles: MemberRole[];
  /** Called when the control plane says the organisation is no longer active. */
  onInactive: () => void;
  onIssued: (email: string, invite: IssuedInvite) => void;
  onBusyChange: (busy: boolean) => void;
}) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<MemberRole>("member");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    setBusy(true);
    onBusyChange(true);
    try {
      const invite = await memberApi.invite(email, role);
      onIssued(email, invite);
      setEmail("");
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onInactive();
      setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
    } finally {
      setBusy(false);
      onBusyChange(false);
    }
  }

  return (
    <form className="tenant-form grid min-w-0 gap-5" onSubmit={submit}>
        <Field label="邮箱">
          <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} />
        </Field>
        <Field label="角色">
          <Select value={role} onValueChange={(value) => setRole(value as MemberRole)} disabled={busy} options={roles.map((value) => ({ value, label: ROLE_LABEL[value] }))} />
        </Field>
        {err && (
          <div className="note crit col-span-full" role="alert">
            {err}
          </div>
        )}
        <CardFooter className="tenant-form-actions col-span-full">
          <Button kind="primary" type="submit" busy={busy}>
            邀请成员
          </Button>
        </CardFooter>
    </form>
  );
}
