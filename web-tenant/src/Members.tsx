import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import { when } from "./Keys";
import { ROLE_LABEL } from "./Org";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "./components/page";
import { formatCount } from "./lib/chartFormat";
import { rankCurrentMemberCosts } from "./lib/memberBento";
import { CreateMemberDialog } from "./CreateMemberDialog";
import "./Keys.css";

// 成员列表只保留费用摘要，两个表格各占整行。

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
    <>
      <PageHeader
        title="成员"
        eyebrow="组织成员与权限"
        dataLead={people === null ? "管理组织成员、访问角色与加入方式。" : <>
          组织现有 <span className="font-semibold text-fg">{formatCount(people.length)}</span> 位成员。
          {active ? "查看成员费用，管理访问权限与独立额度。" : "组织已停用，目前仅支持移除成员。"}
        </>}
        actions={<>{active && <Button onClick={() => { setIssued(null); setInviteOpen(true); }} icon="plus">邀请成员</Button>}{canCreate && <CreateMemberDialog
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
      <BentoGrid className="tenant-members">
        {active && <Card tone="lilac" padding="sm" className={span.d12} aria-label="成员本月费用">
          <div className="flex flex-wrap items-center justify-between gap-4">
            <div className="min-w-0">
              <p className="text-small font-semibold text-fg-2">本月成员已计费费用</p>
              {usageErr || listErr ? <CardMessage onRetry={() => void load()}>成员费用读取失败，请重试。</CardMessage>
                : usage === null || people === null ? <MetricSkeleton />
                  : <Metric size="value" className="mt-2">{totalCost === null ? "未知" : fmtMicroCny(totalCost)}</Metric>}
            </div>
            <p className="max-w-xl text-caption text-fg-3">{usage ? <>{fmtBeijing(usage.from)} 至 {fmtBeijing(usage.to)}（UTC+8）。<br /></> : null}仅含当前成员的已计费费用，可能含估算用量；已离开成员、无成员归属和待计价费用不计入。</p>
          </div>
        </Card>}
        <Card padding="table" className={`${span.d12} tenant-members-list`}>
          <CardHeader number={active ? 3 : 2} title="组织成员" description={active ? "本月费用按 UTC+8 月初至当前时间统计；点击成员查看用量明细并管理权限和额度。" : undefined} />
          <RecordTableToolbar search={{ value: memberTable.query, onChange: memberTable.setQuery, label: "搜索成员", placeholder: "显示名或邮箱" }}>
            <Field label="成员角色"><Select size="compact" value={roleFilter} onValueChange={setRoleFilter} options={[{ value: "", label: "全部角色" }, ...(["owner", "admin", "member", "billing"] as const).map((role) => ({ value: role, label: ROLE_LABEL[role] }))]} /></Field>
            <Field label="成员状态"><Select size="compact" value={statusFilter} onValueChange={setStatusFilter} options={[{ value: "", label: "全部状态" }, { value: "active", label: "正常" }, { value: "disabled", label: "已被平台停用" }]} /></Field>
          </RecordTableToolbar>
          {!active && <div className="note crit">组织已被停用；这里只能移除成员。</div>}
          {active && usageErr && <div className="note crit" role="alert">成员用量读取失败：{usageErr}</div>}
          {active && budgetsErr && <div className="note crit" role="alert">成员额度读取失败：{budgetsErr}</div>}
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
          {people === null ? (
            !listErr && <Loading />
          ) : (
            <DataTable label="组织成员">
              <thead>
                <tr>
                  <th>成员</th>
                  <th>角色</th>
                  <th>状态</th>
                  <th className="num">本月费用</th>
                  <th className="num">成员额度</th>
                  <th>
                    <span className="vh">操作</span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {memberTable.rows.map((m) => {
                  const self = m.id === member.id;
                  const memberUsageRow = usageByMember.get(m.id);
                  return (
                    <tr key={m.id}>
                      <td data-label="成员">
                        <a className="dt-cell-primary font-bold underline-offset-4 hover:underline" href={`#members?member=${encodeURIComponent(m.id)}`} aria-label={`${m.email} 的成员详情`} onClick={(event) => {
                          if (onOpenMember) { event.preventDefault(); onOpenMember(m.id); }
                        }} title={m.display_name ?? m.email}>
                          {m.display_name ?? m.email}
                          {self && <span className="self">（我）</span>}
                        </a>
                        <span className="dt-cell-secondary" title={m.email}>
                          {m.email}
                        </span>
                      </td>
                      <td data-label="角色">
                        {ROLE_LABEL[m.role] ?? m.role}
                      </td>
                      <td data-label="状态">
                        <Lamp state={m.disabled ? "disabled" : "applied"}>{m.disabled ? "已被平台停用" : "正常"}</Lamp>
                      </td>
                      <td className="num" data-label="本月费用">
                        {!active ? <span className="hint">组织未启用，无法读取用量</span> : usageErr ? <span className="hint">用量读取失败</span>
                          : usage === null ? <span className="hint">用量载入中…</span>
                            : <span className="tabular">{memberChargeLabel(memberUsageRow)}</span>}
                      </td>
                      <td className="num" data-label="成员额度">
                        {!active ? <span className="hint">组织未启用，无法读取额度</span> : budgetsErr ? <span className="hint">额度读取失败</span>
                          : budgets === null ? <span className="hint">额度载入中…</span>
                            : <BudgetCell budgets={budgetsByMember.get(m.id) ?? []} />}
                      </td>
                      <td className="right" data-label="操作">
                        <a className="inline-flex min-h-11 items-center whitespace-nowrap text-primary underline-offset-4 hover:underline" href={`#members?member=${encodeURIComponent(m.id)}`} onClick={(event) => {
                          if (onOpenMember) { event.preventDefault(); onOpenMember(m.id); }
                        }}>查看详情</a>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </DataTable>
          )}
          {people !== null && <RecordTablePager page={memberTable.page} pageSize={memberTable.pageSize} onPageChange={memberTable.setPage} onPageSizeChange={memberTable.setPageSize} total={memberTable.filteredCount} label="成员" />}
        </Card>

        {active && (
          <Card padding="table" className={`${span.d12} tenant-invites-list`}>
            <CardHeader number={4} title="待接受的邀请" description="邀请在被接受后才会成为组织成员。" />
            {invitesErr && (
              <div className="note crit" role="alert">
                {invitesErr}
              </div>
            )}
            <RecordTableToolbar search={{ value: inviteTable.query, onChange: inviteTable.setQuery, label: "搜索邀请", placeholder: "邮箱或邀请人" }}>
              <Field label="邀请角色"><Select size="compact" value={inviteRole} onValueChange={setInviteRole} options={[{ value: "", label: "全部角色" }, ...roles.map((role) => ({ value: role, label: ROLE_LABEL[role] }))]} /></Field>
            </RecordTableToolbar>
            {invites === null ? (
              !invitesErr && <Loading />
            ) : invites.length === 0 ? (
              <EmptyState mark="mail" text="没有待接受的邀请。" />
            ) : (
              <DataTable label="待接受的邀请">
                <thead>
                  <tr>
                    <th>邮箱</th>
                    <th>角色</th>
                    <th>邀请人</th>
                    <th>有效期至</th>
                    <th>
                      <span className="vh">操作</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {inviteTable.rows.map((inv) => {
                    const by = inv.invited_by === null ? "平台" : (inv.invited_by.display_name ?? "成员");
                    return (
                      <tr key={inv.id}>
                        <td data-label="邮箱">
                          <span className="dt-cell-primary font-bold" title={inv.email}>
                            {inv.email}
                          </span>
                        </td>
                        <td data-label="角色">{ROLE_LABEL[inv.role] ?? inv.role}</td>
                        <td data-label="邀请人">
                          <span className="trunc" title={by}>
                            {by}
                          </span>
                        </td>
                        <td className="tabular" data-label="有效期至">{when(inv.expires_at)}</td>
                        <td className="right" data-label="操作">
                          {(isOwner || inv.role !== "owner") && (
                            <Button
                              size="sm"
                              disabled={busy === inv.id}
                              busy={busy === inv.id && acting === "revoke"}
                              busyLabel="撤销"
                              onClick={() => void act(inv.id, "revoke", () => memberApi.revokeInvite(inv.id))}
                            >
                              撤销
                            </Button>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </DataTable>
            )}
            {invites !== null && <RecordTablePager page={inviteTable.page} pageSize={inviteTable.pageSize} onPageChange={inviteTable.setPage} onPageSizeChange={inviteTable.setPageSize} total={inviteTable.filteredCount} label="邀请" />}
          </Card>
        )}
        {active && <FormDialog open={inviteOpen} onOpenChange={(next) => { setInviteOpen(next); if (!next) setIssued(null); }} busy={inviteBusy} title={issued ? "邀请已创建" : "邀请成员"} description="创建邀请链接，由新成员自行设置登录口令。">
          {issued ? <div ref={issuedResult} tabIndex={-1} className="key-created-result" aria-label="邀请已创建"><ShownOnce title="邀请链接只显示这一次" masked={false} value={issued.link} onDone={() => { setInviteOpen(false); setIssued(null); }}><span className="hint">发给 {issued.email}，{when(issued.expiresAt)} 前有效。</span></ShownOnce></div> :
          <InviteForm
            roles={roles}
            onBusyChange={setInviteBusy}
            onInactive={onSelfChanged}
            onIssued={(email, invite) => {
              setIssued({ email, link: inviteLink(invite), expiresAt: invite.expires_at });
              void load();
            }}
          />}
        </FormDialog>}
      </BentoGrid>
    </>
  );
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
    <Card className={span.d5Wide}>
      <CardHeader number={5} title="邀请成员" description="生成邀请链接，由新成员自行设置登录口令。" />
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
    </Card>
  );
}
