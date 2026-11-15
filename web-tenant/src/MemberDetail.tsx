import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, budgets as budgetApi, members as memberApi, type Budget, type Member, type MemberRole, type SessionMember, type TenantStatus } from "./lib/api";
import { memberUsage, type MemberUsageSummary } from "./lib/memberUsage";
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtMicroCny } from "./lib/money";
import { budgetPeriodRange, fmtBeijing } from "./lib/time";
import { exactRatio } from "./lib/chartFormat";
import { ROLE_LABEL } from "./Org";
import { MemberBudgetDialog } from "./MemberBudgetDialog";
import { MemberUsageDetails } from "./MemberUsageDetails";
import { ResetMemberPassword } from "./ResetMemberPassword";
import { Card, CardHeader, CardMessage, DescList, PageHeader } from "./components/page";
import { BudgetProgress } from "./components/charts";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { Select } from "./ui/Select";
import { TwoStep } from "./ui/TwoStep";

const PERIOD = { day: "每日", week: "每周", month: "每月" };
const SET_BY = { operator: "平台", org_admin: "组织管理员", member: "成员" };
function failure(error: unknown): string { return error instanceof ApiError ? formMessageFor(error) : String(error); }

/** 正常详情页，权限与租户 API 一致；密码、额度与角色独立操作。 */
export function MemberDetail({ memberId, member, status, onBack, onSelfChanged, onLeft }: {
  memberId: string;
  member: SessionMember;
  status: TenantStatus;
  onBack: () => void;
  onSelfChanged: () => void;
  onLeft: () => void;
}) {
  const active = status === "active";
  const manager = member.role === "owner" || member.role === "admin";
  const [person, setPerson] = useState<Member | null>(null);
  const [found, setFound] = useState(false);
  const [personError, setPersonError] = useState<string | null>(null);
  const [usage, setUsage] = useState<MemberUsageSummary | null>(null);
  const [usageError, setUsageError] = useState<string | null>(null);
  const [budgets, setBudgets] = useState<Budget[] | null>(null);
  const [budgetError, setBudgetError] = useState<string | null>(null);
  const [role, setRole] = useState<MemberRole>("member");
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [budgetDialog, setBudgetDialog] = useState<{ budget: Budget | null } | null>(null);
  const [passwordOpen, setPasswordOpen] = useState(false);
  const loadId = useRef(0);
  const pending = useRef(false);
  const load = useCallback(async () => {
    const id = ++loadId.current;
    async function read<T>(fetch: () => Promise<T>, done: (value: T) => void, refused: (message: string | null) => void) {
      try {
        const value = await fetch();
        if (id !== loadId.current) return;
        done(value);
        refused(null);
      } catch (error) { if (id === loadId.current) refused(failure(error)); }
    }
    await Promise.all([
      read(memberApi.list, (people) => {
        const target = people.find((row) => row.id === memberId) ?? null;
        setPerson(target);
        setFound(true);
        if (target) setRole(target.role);
      }, setPersonError),
      ...(active ? [read(memberUsage, setUsage, setUsageError), read(budgetApi.list, setBudgets, setBudgetError)] : []),
    ]);
  }, [memberId, active]);
  useEffect(() => {
    setPerson(null); setFound(false); setPersonError(null);
    setUsage(null); setUsageError(null); setBudgets(null); setBudgetError(null);
    setActionError(null); setNotice(null); setBudgetDialog(null); setPasswordOpen(false);
    if (manager) void load();
    return () => { loadId.current += 1; };
  }, [load, manager, member.role]);

  async function act(key: string, change: () => Promise<unknown>, done?: () => void) {
    if (pending.current) return;
    pending.current = true;
    setBusy(key); setActionError(null); setNotice(null);
    try {
      await change();
      if (done) { done(); return; }
      setNotice(key === "role" ? "成员角色已更改。" : "成员额度已移除。");
    } catch (error) {
      if (error instanceof ApiError && error.code === "tenant_not_active") onSelfChanged();
      setActionError(error instanceof ApiError ? messageFor(error) : String(error));
    } finally { pending.current = false; setBusy(null); }
    await load();
  }

  if (!manager) return <><PageHeader title="成员详情" actions={<Button onClick={onBack}>返回成员列表</Button>} /><div className="note crit" role="alert">当前角色无权查看组织成员。</div></>;
  const self = person?.id === member.id;
  const managesPerson = person !== null && (member.role === "owner" || person.role !== "owner");
  const roleOptions = (member.role === "owner" ? ["owner", "admin", "member", "billing"] : ["admin", "member", "billing"]) as MemberRole[];
  const memberBudgets = budgets?.filter((budget) => budget.scope === "member" && budget.target.id === memberId) ?? [];
  const usageRow = usage?.items.find((row) => row.member_id === memberId);
  return <>
    <PageHeader title={person?.display_name ?? person?.email ?? "成员详情"} description={person?.display_name ? person.email : undefined} actions={<Button onClick={onBack}>返回成员列表</Button>} />
    {actionError && <div className="note crit mb-4" role="alert">{actionError}</div>}
    {notice && <div className="note ok mb-4" role="status">{notice}</div>}
    {!active && <div className="note crit mb-4">组织已被停用；这里只能移除成员。</div>}
    {personError ? <Card><CardMessage onRetry={() => void load()}>成员读取失败：{personError}</CardMessage></Card> : !found ? <Loading /> : !person ? <Card><CardMessage>该成员不存在或已离开组织。</CardMessage></Card> : <div className="grid gap-(--card-gap)">
      <Card role="region" aria-label="成员信息">
        <CardHeader title="成员信息" />
        <DescList items={[
          { label: "显示名", value: person.display_name ?? "—" },
          { label: "邮箱", value: <span className="break-all">{person.email}</span> },
          { label: "加入时间", value: fmtBeijing(person.created_at) },
          { label: "状态", value: <Lamp state={person.disabled ? "disabled" : "applied"}>{person.disabled ? "已被平台停用" : "正常"}</Lamp> },
        ]} />
        <div className="mt-5 flex flex-wrap items-end gap-3">
          {active && managesPerson ? <>
            <Field label="角色"><Select value={role} onValueChange={(value) => setRole(value as MemberRole)} disabled={busy !== null} options={roleOptions.map((value) => ({ value, label: ROLE_LABEL[value] }))} /></Field>
            {role !== person.role && <TwoStep label="更改角色" disabled={busy !== null} busy={busy === "role"} warning={role === "billing" ? "账单角色不持有密钥，该成员的密钥将停止工作。" : self ? "更改自己的角色可能失去当前管理权限。" : "该成员的访问权限将随角色更改。"} onConfirm={() => void act("role", () => memberApi.setRole(person.id, role), self ? onSelfChanged : undefined)} />}
          </> : <p>角色：{ROLE_LABEL[person.role]}</p>}
        </div>
      </Card>
      <Card role="region" aria-label="本月用量明细">
        <CardHeader title="本月用量明细" description={usage ? `${fmtBeijing(usage.from)} 至 ${fmtBeijing(usage.to)}，按 UTC+8 月初至当前时间统计。` : "本月费用与计费用量。"} />
        {!active ? <CardMessage>组织未启用，无法读取用量。</CardMessage> : usageError ? <CardMessage onRetry={() => void load()}>成员用量读取失败：{usageError}</CardMessage> : usage === null ? <Loading /> : usageRow ? <MemberUsageDetails usage={usageRow} /> : <CardMessage>暂无该成员的用量数据。</CardMessage>}
      </Card>
      <Card role="region" aria-label="成员额度">
        <CardHeader title="成员额度" description="各项成员额度独立生效；组织与密钥额度仍然适用。" actions={active && <Button onClick={() => setBudgetDialog({ budget: null })}>设置成员额度</Button>} />
        {!active ? <CardMessage>组织未启用，无法读取额度。</CardMessage> : budgetError ? <CardMessage onRetry={() => void load()}>成员额度读取失败：{budgetError}</CardMessage> : budgets === null ? <Loading /> : memberBudgets.length === 0 ? <p className="hint">不限制</p> : <DataTable label="成员额度">
          <thead><tr><th>周期</th><th>限额</th><th>本期已用</th><th>剩余</th><th>设置方</th><th><span className="vh">操作</span></th></tr></thead>
          <tbody>{memberBudgets.map((budget) => <tr key={budget.id}>
            <td data-label="周期">{budget.kind === "cumulative" ? "累计" : PERIOD[budget.period ?? "month"]}<span className="cell-note">{budget.kind === "cumulative" ? `自 ${fmtBeijing(budget.counted_from ?? budget.created_at)}` : budget.period && budget.period_start ? budgetPeriodRange(budget.period_start, budget.period) : "本期起止未知"}</span></td>
            <td className="tabular" data-label="限额">{fmtMicroCny(budget.limit_micro_cny)}</td>
            <td data-label="本期已用"><span className="tabular">{fmtMicroCny(budget.spent_micro_cny)}</span><BudgetProgress className="mt-2" value={exactRatio(budget.spent_micro_cny, budget.limit_micro_cny)} accessibleLabel={`本期已用 ${fmtMicroCny(budget.spent_micro_cny)}，限额 ${fmtMicroCny(budget.limit_micro_cny)}`} /></td>
            <td className="tabular" data-label="剩余">{fmtMicroCny(budget.remaining_micro_cny)}{budget.exhausted ? "（已用尽）" : ""}</td>
            <td data-label="设置方">{SET_BY[budget.set_by]}</td>
            <td className="right" data-label="操作">{budget.set_by !== "operator" ? <div className="flex flex-wrap justify-end gap-2"><Button size="sm" disabled={busy !== null} onClick={() => setBudgetDialog({ budget })}>调整额度</Button><TwoStep label="移除额度" size="sm" disabled={busy !== null} busy={busy === budget.id} warning="移除后不再受此项成员额度限制，其他额度仍然适用。" onConfirm={() => void act(budget.id, () => budgetApi.remove(budget.id))} /></div> : <span className="hint">由平台管理</span>}</td>
          </tr>)}</tbody>
        </DataTable>}
      </Card>
      <Card role="region" aria-label="登录与访问">
        <CardHeader title="登录与访问" />
        <div className="grid gap-5 tablet:grid-cols-2">
          <div><h3 className="mb-2 font-medium">登录密码</h3>{self ? <p className="hint">请在个人账户中修改自己的密码。</p> : active && managesPerson ? <><p className="mb-3 hint">重置后，该成员的现有登录会话将结束，需要使用新密码重新登录。</p><Button disabled={busy !== null} onClick={() => setPasswordOpen(true)}>重置密码</Button></> : <p className="hint">{!active ? "组织未启用，无法重置密码。" : "只有组织所有者可以重置所有者的密码。"}</p>}</div>
          <div><h3 className="mb-2 font-medium">账号状态</h3><p className="hint">{person.disabled ? "该账号已被平台停用，请联系平台管理员恢复。" : "该账号正常。账号启停由平台管理员管理。"}</p></div>
        </div>
        {managesPerson && <div className="mt-6 border-t border-divider pt-5"><h3 className="mb-2 font-medium">移除成员</h3><p className="mb-3 hint">移除后将结束登录会话并吊销该成员的所有密钥。</p><TwoStep label="移除成员" disabled={busy !== null} busy={busy === "remove"} warning={self ? "你将离开此组织，所有密钥会被吊销。" : "该成员的所有密钥会被吊销。"} onConfirm={() => void act("remove", () => memberApi.remove(person.id), self ? onLeft : onBack)} /></div>}
      </Card>
    </div>}
    {person && budgetDialog && <MemberBudgetDialog memberId={person.id} budget={budgetDialog.budget} onClose={() => setBudgetDialog(null)} onInactive={onSelfChanged} onSaved={() => { setBudgetDialog(null); setNotice("成员额度已保存。"); void load(); }} />}
    {person && passwordOpen && <ResetMemberPassword memberId={person.id} onClose={() => setPasswordOpen(false)} onInactive={onSelfChanged} onSaved={() => { setPasswordOpen(false); setNotice("密码已重置。该成员需要使用新密码重新登录。"); }} />}
  </>;
}
