import { useCallback, useEffect, useState } from "react";
import {
  ApiError,
  logout,
  onSessionEnded,
  session,
  type Session,
  type SessionMember,
  type SessionTenant,
  type TenantStatus,
} from "./lib/api";
import { EXPECTED_API_CONTRACT } from "./lib/contract";
import { messageFor } from "./lib/errors";
import { Gate } from "./Gate";
import { Keys } from "./Keys";
import { Members } from "./Members";
import { Org, ROLE_LABEL } from "./Org";

const TABS = [
  { id: "org", label: "组织" },
  { id: "keys", label: "密钥" },
  { id: "members", label: "成员" },
] as const;

export type TabId = (typeof TABS)[number]["id"];

/**
 * Whether `member` sees tab `id` of an organisation in state `status`. A
 * billing member holds no keys and only owners and admins manage members;
 * a role a newer control plane adds sees the organisation only, until this
 * build knows it.
 *
 * An active organisation has every page. A suspended one keeps its keys and
 * members pages, which then only close things (revoke a key, remove a
 * member): a key that leaked must not wait for the reactivation. A pending
 * one has neither, since nothing can exist there before the approval.
 */
function canSee(id: TabId, member: SessionMember, status: TenantStatus): boolean {
  if (status !== "active" && status !== "suspended") return false;
  switch (id) {
    case "org":
      return true;
    case "keys":
      return member.role === "owner" || member.role === "admin" || member.role === "member";
    case "members":
      return member.role === "owner" || member.role === "admin";
  }
}

interface SignedIn {
  member: SessionMember;
  tenant: SessionTenant;
}

function signedIn(s: Session): SignedIn | null {
  return s.authed && s.member && s.tenant ? { member: s.member, tenant: s.tenant } : null;
}

export function App({ inviteToken }: { inviteToken: string | null }) {
  // null until the session is read: defaulting to signed-out would flash
  // the gate on every reload, and most reloads are signed in.
  const [apiContract, setApiContract] = useState<number | null>(null);
  const [me, setMe] = useState<SignedIn | null>(null);
  const [invite, setInvite] = useState(inviteToken);
  const [bootError, setBootError] = useState<string | null>(null);
  const [signOutError, setSignOutError] = useState<string | null>(null);
  /** Why the gate is showing again, when a session ended under the member. */
  const [notice, setNotice] = useState<string | null>(null);
  const [tab, setTab] = useState<TabId>("org");

  // Any page's request that finds the session gone drops back to the gate:
  // what was on screen is no longer the member's to see or act on. (The
  // gate's own requests never answer `unauthenticated`.)
  useEffect(() => {
    onSessionEnded(() => {
      setMe(null);
      setNotice("登录已过期，请重新登录");
    });
    return () => onSessionEnded(null);
  }, []);

  // The member removed themself, which ended their session on the control
  // plane already: nothing is left to sign out of, and a logout request
  // would only answer that the session is gone.
  const leave = useCallback(() => {
    setMe(null);
    setNotice("你已离开该组织");
  }, []);

  const boot = useCallback(async () => {
    setBootError(null);
    try {
      const s = await session();
      setApiContract(s.apiContract);
      setMe(signedIn(s));
    } catch (e) {
      setBootError(e instanceof ApiError ? messageFor(e) : String(e));
    }
  }, []);

  useEffect(() => {
    void boot();
  }, [boot]);

  const signIn = useCallback((s: Session) => {
    setMe(signedIn(s));
    setInvite(null);
    setSignOutError(null);
    setNotice(null);
    setTab("org");
  }, []);

  // Signed out only once the control plane has ended the session (or says
  // there was none): a failed request leaves the session live, and showing
  // the gate then would tell the member they are out while they are not.
  const signOut = useCallback(async () => {
    setSignOutError(null);
    try {
      await logout();
    } catch (e) {
      if (!(e instanceof ApiError && e.code === "unauthenticated")) {
        setSignOutError("登出失败，请重试");
        return;
      }
    }
    setMe(null);
  }, []);

  if (bootError) {
    return (
      <div className="centered">
        <div className="note crit" role="alert">
          <strong>无法读取登录状态。</strong> {bootError}{" "}
          <button className="ghost" onClick={() => void boot()}>
            重试
          </button>
        </div>
      </div>
    );
  }

  if (apiContract === null) return <div className="boot">载入中…</div>;

  // A skew stops everything, reads included: this build's idea of the API's
  // shapes no longer holds, so whatever it shows or sends could be misread.
  if (apiContract !== EXPECTED_API_CONTRACT) {
    return (
      <div className="centered">
        <div className="note crit">
          <strong>门户与平台的接口版本不一致，已停止一切操作。</strong>
          <p>
            门户期望契约 v{EXPECTED_API_CONTRACT}，平台报告 v{apiContract}。请稍后再试，或联系平台。
          </p>
        </div>
      </div>
    );
  }

  // An invitation link opens the acceptance form even over a live session:
  // accepting signs in as the invited member in its place.
  if (!me || invite !== null) {
    return (
      <Gate
        invite={invite}
        signedIn={me !== null}
        notice={notice}
        onIn={signIn}
        onLeaveInvite={() => setInvite(null)}
      />
    );
  }

  const { member, tenant } = me;
  const visible = TABS.filter((t) => canSee(t.id, member, tenant.status));
  const current: TabId = visible.some((t) => t.id === tab) ? tab : "org";

  return (
    <div className="frame">
      <header className="book-head">
        <h1>{tenant.name}</h1>
        <span className="sub">AISIX 组织门户</span>
        <div className="tools">
          <span className="pill" title={member.email}>
            {member.display_name || member.email} · {ROLE_LABEL[member.role] ?? member.role}
          </span>
          <button className="ghost" onClick={() => void signOut()}>
            登出
          </button>
        </div>
      </header>

      {signOutError && (
        <div className="note crit" role="alert">
          {signOutError}
        </div>
      )}

      {visible.length > 0 && (
        <nav className="tabs" role="tablist" aria-label="组织门户分区">
          {visible.map((t) => (
            <button
              key={t.id}
              role="tab"
              data-tab={t.id}
              aria-selected={current === t.id}
              onClick={() => setTab(t.id)}
            >
              {t.label}
            </button>
          ))}
        </nav>
      )}

      <main>
        {current === "keys" ? (
          <Keys member={member} status={tenant.status} onSelfChanged={() => void boot()} />
        ) : current === "members" ? (
          <Members member={member} status={tenant.status} onSelfChanged={() => void boot()} onLeft={leave} />
        ) : (
          <Org member={member} status={tenant.status} />
        )}
      </main>
    </div>
  );
}
