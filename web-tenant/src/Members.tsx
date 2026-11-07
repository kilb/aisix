import { useCallback, useEffect, useState } from "react";
import {
  ApiError,
  members as memberApi,
  type Invite,
  type IssuedInvite,
  type Member,
  type MemberRole,
  type SessionMember,
  type TenantStatus,
} from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { ConfirmButton, ShownOnce } from "./controls";
import { when } from "./Keys";
import { ROLE_LABEL } from "./Org";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
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
  const [issued, setIssued] = useState<{ email: string; link: string; expiresAt: string } | null>(null);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  /** A role picked for a member and not yet confirmed. */
  const [drafts, setDrafts] = useState<Record<string, MemberRole>>({});

  const load = useCallback(async () => {
    try {
      setPeople(await memberApi.list());
      setListErr(null);
    } catch (e) {
      setListErr(failure(e));
    }
    if (!active) return;
    try {
      setInvites(await memberApi.invites());
      setInvitesErr(null);
    } catch (e) {
      setInvitesErr(failure(e));
    }
  }, [active]);

  // Read again when the caller's own role changes (the session re-read after
  // a change to their own row): what they may see and do changed with it.
  useEffect(() => {
    void load();
  }, [load, member.role]);

  /**
   * One change, keyed `key` while it runs. On success `done`, when given,
   * takes over from reading the lists again: a change to the caller's own
   * row can leave them unable to read them. Otherwise, and on any refusal,
   * the lists are read again.
   */
  async function act(key: string, change: () => Promise<unknown>, done?: () => void) {
    setBusy(key);
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
    }
    await load();
  }

  function dropDraft(id: string) {
    setDrafts(({ [id]: _, ...rest }) => rest);
  }

  /** Whether the caller may change or remove `m`. */
  const manages = (m: Member) => isOwner || m.role !== "owner";

  return (
    <div className="stack">
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

      <div className="panel">
        <h2>成员</h2>
        {!active && <div className="note crit">组织已被停用；这里只能移除成员。</div>}
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
          !listErr && <p className="hint">载入中…</p>
        ) : (
          <div className="scroll">
            <table>
              <thead>
                <tr>
                  <th>显示名</th>
                  <th>邮箱</th>
                  <th>角色</th>
                  <th>状态</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {people.map((m) => {
                  const self = m.id === member.id;
                  const draft = drafts[m.id];
                  return (
                    <tr key={m.id}>
                      <td>
                        {m.display_name ?? "—"}
                        {self && "（我）"}
                      </td>
                      <td>{m.email}</td>
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
                              <>
                                {draft === "billing" && (
                                  <span className="hint warn-inline">账单角色不持有密钥，该成员的密钥将停止工作</span>
                                )}
                                <button
                                  className="ghost danger"
                                  disabled={busy === m.id}
                                  onClick={() => {
                                    dropDraft(m.id);
                                    void act(
                                      m.id,
                                      () => memberApi.setRole(m.id, draft),
                                      self ? onSelfChanged : undefined,
                                    );
                                  }}
                                >
                                  确认更改角色
                                </button>
                                <button className="ghost" onClick={() => dropDraft(m.id)}>
                                  取消
                                </button>
                              </>
                            )}
                          </span>
                        ) : (
                          (ROLE_LABEL[m.role] ?? m.role)
                        )}
                      </td>
                      <td>{m.disabled ? "已被平台停用" : "正常"}</td>
                      <td className="right">
                        {manages(m) && (
                          <ConfirmButton
                            label="移除"
                            warning="该成员的所有密钥会被吊销"
                            disabled={busy === m.id}
                            onConfirm={() => void act(m.id, () => memberApi.remove(m.id), self ? onLeft : undefined)}
                          />
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {active && (
        <div className="panel">
          <h2>待接受的邀请</h2>
          {invitesErr && (
            <div className="note crit" role="alert">
              {invitesErr}
            </div>
          )}
          {invites === null ? (
            !invitesErr && <p className="hint">载入中…</p>
          ) : invites.length === 0 ? (
            <p className="hint">没有待接受的邀请。</p>
          ) : (
            <div className="scroll">
              <table>
                <thead>
                  <tr>
                    <th>邮箱</th>
                    <th>角色</th>
                    <th>邀请人</th>
                    <th>有效期至</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {invites.map((inv) => (
                    <tr key={inv.id}>
                      <td>{inv.email}</td>
                      <td>{ROLE_LABEL[inv.role] ?? inv.role}</td>
                      <td>{inv.invited_by === null ? "平台" : (inv.invited_by.display_name ?? "成员")}</td>
                      <td>{when(inv.expires_at)}</td>
                      <td className="right">
                        {(isOwner || inv.role !== "owner") && (
                          <button
                            className="ghost"
                            disabled={busy === inv.id}
                            onClick={() => void act(inv.id, () => memberApi.revokeInvite(inv.id))}
                          >
                            撤销
                          </button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      )}
    </div>
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
    <div className="panel">
      <h2>邀请成员</h2>
      <form onSubmit={submit}>
        <label className="f">
          <span>邮箱</span>
          <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} />
        </label>
        <label className="f">
          <span>角色</span>
          <select value={role} onChange={(e) => setRole(e.target.value as MemberRole)}>
            {roles.map((r) => (
              <option key={r} value={r}>
                {ROLE_LABEL[r]}
              </option>
            ))}
          </select>
        </label>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        <button className="act narrow" type="submit" disabled={busy}>
          {busy ? "处理中…" : "邀请成员"}
        </button>
      </form>
    </div>
  );
}
