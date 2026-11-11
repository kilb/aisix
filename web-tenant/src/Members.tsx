import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
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
import { memberUsage, type MemberUsage, type MemberUsageSummary } from "./lib/memberUsage";
import { fmtMicroCny, fmtUtc } from "./lib/money";
import { formMessageFor, messageFor } from "./lib/errors";
import { ShownOnce } from "./controls";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { TwoStep } from "./ui/TwoStep";
import { when } from "./Keys";
import { ROLE_LABEL } from "./Org";
import { Card, CardHeader, PageHeader, Split } from "./components/page";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

const QUANTITY_LABEL: Record<string, string> = {
  input: "输入", output: "输出", cache_read: "缓存读", cache_write: "缓存写",
  cache_write_1h: "1 小时缓存写", reasoning: "推理", audio_input: "音频输入",
  audio_output: "音频输出", image_input: "图片输入", image_output: "图片输出", web_search: "联网搜索",
};

function UsageCell({ usage }: { usage: MemberUsage }) {
  const incomplete = usage.unknown_quantity_requests > 0;
  const quantities = Object.entries(usage.quantities);
  const tokens = quantities
    .filter(([dimension]) => dimension in QUANTITY_LABEL && dimension !== "web_search")
    .reduce((total, [, quantity]) => total + BigInt(quantity), 0n);
  return (
    <>
      <span className="tabular">{usage.requests.toLocaleString()} 次调用</span>
      <span className="cell-note tabular">
        {incomplete && quantities.length === 0 ? "计费用量未知" : `${incomplete ? "已知" : ""}计费用量 ${tokens.toString()} token`}
      </span>
      <span className="cell-note tabular">已计费 {fmtMicroCny(usage.charged_micro_cny)}</span>
      {quantities.map(([dimension, quantity]) => (
        <span className="cell-note tabular" key={dimension}>
          {QUANTITY_LABEL[dimension] ?? dimension} {quantity.toLocaleString()} {dimension === "web_search" ? "次" : dimension in QUANTITY_LABEL ? "token" : ""}
        </span>
      ))}
      {incomplete && <span className="cell-note hint">{usage.unknown_quantity_requests} 次用量明细未记录</span>}
      {usage.unpriced_requests > 0 && <span className="cell-note hint">{usage.unpriced_requests} 次未计价，费用尚未包含</span>}
      {usage.pending_requests > usage.unpriced_requests && (
        <span className="cell-note hint">另有 {usage.pending_requests - usage.unpriced_requests} 次待计价</span>
      )}
      {usage.estimated_requests > 0 && <span className="cell-note hint">{usage.estimated_requests} 次含估算用量</span>}
    </>
  );
}

function BudgetCell({ budgets }: { budgets: Budget[] }) {
  if (budgets.length === 0) return <span className="hint">未设置成员额度；仍受组织及密钥限制</span>;
  const period = { day: "每日", week: "每周", month: "每月" };
  return (
    <div className="grid gap-3">
      {budgets.map((budget) => (
        <div key={budget.id}>
          <span>{budget.kind === "cumulative" ? "累计" : period[budget.period ?? "month"]}额度</span>
          <span className="cell-note tabular">限额 {fmtMicroCny(budget.limit_micro_cny)}</span>
          <span className="cell-note tabular">本期已用 {fmtMicroCny(budget.spent_micro_cny)}</span>
          <span className="cell-note tabular">剩余 {fmtMicroCny(budget.remaining_micro_cny)}{budget.exhausted && "（已用尽）"}</span>
          <span className="cell-note hint">
            {budget.kind === "cumulative" ? "累计自 " : "本期自 "}{fmtUtc(budget.counted_from ?? budget.period_start ?? budget.created_at)}
          </span>
        </div>
      ))}
    </div>
  );
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

/**
 * The organisation's members and open invitations, for an owner or an
 * admin. Only an owner makes, changes or removes an owner, so an admin is
 * offered neither the role nor an owner's row. While the organisation is
 * suspended the page offers removal only.
 *
 * `onSelfChanged` re-reads the session: after a change to the caller's own
 * role, and after a refusal that says the organisation is no longer active,
 * since what the portal shows them is the control plane's to say.
 * `onLeft` follows the caller removing themself.
 */
export function Members({
  member,
  status,
  onSelfChanged,
  onLeft,
}: {
  member: SessionMember;
  status: TenantStatus;
  onSelfChanged: () => void;
  onLeft: () => void;
}) {
  const active = status === "active";
  const isOwner = member.role === "owner";
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
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // Which action on that member or invitation is in flight.
  const [acting, setActing] = useState<"role" | "remove" | "revoke" | null>(null);
  /** A role picked for a member and not yet confirmed. */
  const [drafts, setDrafts] = useState<Record<string, MemberRole>>({});
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

  /**
   * One change, keyed `key` while it runs. On success `done`, when given,
   * takes over from reading the lists again: a change to the caller's own
   * row can leave them unable to read them. Otherwise, and on any refusal,
   * the lists are read again.
   */
  async function act(key: string, what: "role" | "remove" | "revoke", change: () => Promise<unknown>, done?: () => void) {
    setBusy(key);
    setActing(what);
    setActionErr(null);
    try {
      await change();
      if (done) {
        done();
        return;
      }
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onSelfChanged();
      setActionErr(failure(e));
    } finally {
      setBusy(null);
      setActing(null);
    }
    await load();
  }

  function dropDraft(id: string) {
    setDrafts(({ [id]: _, ...rest }) => rest);
  }

  /** Whether the caller may change or remove `m`. */
  const manages = (m: Member) => isOwner || m.role !== "owner";

  return (
    <>
      <PageHeader title="成员" />
      <Split
        asideWidth="400px"
        main={
          <div className="grid gap-6">
            <Card>
              <CardHeader title="组织成员" description={active ? "本月用量按 UTC 月初至当前时间统计；成员额度按各自周期计算。" : undefined} />
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
                <DataTable>
                  <thead>
                    <tr>
                      <th>显示名</th>
                      <th>邮箱</th>
                      <th>角色</th>
                      <th>状态</th>
                      <th>本月用量</th>
                      <th>成员额度</th>
                      <th>
                        <span className="vh">操作</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {people.map((m) => {
                      const self = m.id === member.id;
                      const draft = drafts[m.id];
                      const memberUsageRow = usageByMember.get(m.id);
                      return (
                        <tr key={m.id}>
                          <td>
                            <span className="trunc" title={m.display_name ?? undefined}>
                              {m.display_name ?? "—"}
                              {self && <span className="self">（我）</span>}
                            </span>
                          </td>
                          <td>
                            <span className="trunc" style={{ "--trunc": "240px" } as CSSProperties} title={m.email}>
                              {m.email}
                            </span>
                          </td>
                          <td>
                            {active && manages(m) ? (
                              <span className="acts">
                                <select
                                  aria-label={`${m.email} 的角色`}
                                  value={draft ?? m.role}
                                  onChange={(e) => setDrafts({ ...drafts, [m.id]: e.target.value as MemberRole })}
                                >
                                  {roles.map((r) => (
                                    <option key={r} value={r}>
                                      {ROLE_LABEL[r]}
                                    </option>
                                  ))}
                                </select>
                                {/* A role change takes a second step: one that takes access away is destructive. */}
                                {draft && draft !== m.role && (
                                  <span className="role-confirm is-in">
                                    {draft === "billing" && (
                                      <span className="hint warn-inline">账单角色不持有密钥，该成员的密钥将停止工作</span>
                                    )}
                                    <Button
                                      kind="danger"
                                      size="sm"
                                      disabled={busy === m.id}
                                      busy={busy === m.id && acting === "role"}
                                      busyLabel="确认更改角色"
                                      onClick={() =>
                                        // The step stays, busy, until the change is answered.
                                        void act(
                                          m.id,
                                          "role",
                                          () => memberApi.setRole(m.id, draft),
                                          self ? onSelfChanged : undefined,
                                        ).finally(() => dropDraft(m.id))
                                      }
                                    >
                                      确认更改角色
                                    </Button>
                                    <Button size="sm" disabled={busy === m.id} onClick={() => dropDraft(m.id)}>
                                      取消
                                    </Button>
                                  </span>
                                )}
                              </span>
                            ) : (
                              (ROLE_LABEL[m.role] ?? m.role)
                            )}
                          </td>
                          <td>
                            <Lamp state={m.disabled ? "disabled" : "applied"}>{m.disabled ? "已被平台停用" : "正常"}</Lamp>
                          </td>
                          <td>
                            {!active ? <span className="hint">组织未启用，无法读取用量</span> : usageErr ? <span className="hint">用量读取失败</span>
                              : usage === null ? <span className="hint">用量载入中…</span>
                                : memberUsageRow ? <UsageCell usage={memberUsageRow} /> : <span className="hint">暂无用量数据</span>}
                          </td>
                          <td>
                            {!active ? <span className="hint">组织未启用，无法读取额度</span> : budgetsErr ? <span className="hint">额度读取失败</span>
                              : budgets === null ? <span className="hint">额度载入中…</span>
                                : <BudgetCell budgets={budgetsByMember.get(m.id) ?? []} />}
                          </td>
                          <td className="right">
                            {manages(m) && (
                              <TwoStep
                                label="移除"
                                size="sm"
                                warning="该成员的所有密钥会被吊销"
                                disabled={busy === m.id}
                                busy={busy === m.id && acting === "remove"}
                                onConfirm={() => void act(m.id, "remove", () => memberApi.remove(m.id), self ? onLeft : undefined)}
                              />
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </DataTable>
              )}
            </Card>

            {active && (
              <Card>
                <CardHeader title="待接受的邀请" />
                {invitesErr && (
                  <div className="note crit" role="alert">
                    {invitesErr}
                  </div>
                )}
                {invites === null ? (
                  !invitesErr && <Loading />
                ) : invites.length === 0 ? (
                  <EmptyState mark="mail" text="没有待接受的邀请。" />
                ) : (
                  <DataTable>
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
                      {invites.map((inv) => {
                        const by = inv.invited_by === null ? "平台" : (inv.invited_by.display_name ?? "成员");
                        return (
                          <tr key={inv.id}>
                            <td>
                              <span className="trunc" style={{ "--trunc": "240px" } as CSSProperties} title={inv.email}>
                                {inv.email}
                              </span>
                            </td>
                            <td>{ROLE_LABEL[inv.role] ?? inv.role}</td>
                            <td>
                              <span className="trunc" title={by}>
                                {by}
                              </span>
                            </td>
                            <td className="tabular">{when(inv.expires_at)}</td>
                            <td className="right">
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
              </Card>
            )}
          </div>
        }
        aside={
          active || issued ? (
            <div className="grid gap-4">
              {active && (
                <InviteForm
                  roles={roles}
                  onInactive={onSelfChanged}
                  onIssued={(email, invite) => {
                    setIssued({ email, link: inviteLink(invite), expiresAt: invite.expires_at });
                    void load();
                  }}
                />
              )}
              {issued && (
                <ShownOnce title="邀请链接只显示这一次" value={issued.link} onDone={() => setIssued(null)}>
                  <span className="hint">
                    发给 {issued.email}，{when(issued.expiresAt)} 前有效。对方打开链接、设置口令即可加入。
                  </span>
                </ShownOnce>
              )}
            </div>
          ) : undefined
        }
      />
    </>
  );
}

function InviteForm({
  roles,
  onInactive,
  onIssued,
}: {
  roles: MemberRole[];
  /** Called when the control plane says the organisation is no longer active. */
  onInactive: () => void;
  onIssued: (email: string, invite: IssuedInvite) => void;
}) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<MemberRole>("member");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    setBusy(true);
    try {
      const invite = await memberApi.invite(email, role);
      onIssued(email, invite);
      setEmail("");
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onInactive();
      setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardHeader title="邀请成员" />
      <form className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_9rem]" onSubmit={submit}>
        <Field label="邮箱">
          <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} />
        </Field>
        <Field label="角色">
          <select value={role} onChange={(e) => setRole(e.target.value as MemberRole)}>
            {roles.map((r) => (
              <option key={r} value={r}>
                {ROLE_LABEL[r]}
              </option>
            ))}
          </select>
        </Field>
        {err && (
          <div className="note crit col-span-full" role="alert">
            {err}
          </div>
        )}
        <div className="col-span-full flex justify-end">
          <Button kind="primary" type="submit" busy={busy}>
            邀请成员
          </Button>
        </div>
      </form>
    </Card>
  );
}
