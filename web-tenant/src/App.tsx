import { useCallback, useEffect, useState } from "react";
import { ApiError, logout, session, type Session, type SessionMember, type SessionTenant } from "./lib/api";
import { EXPECTED_API_CONTRACT } from "./lib/contract";
import { messageFor } from "./lib/errors";
import { Gate } from "./Gate";
import { Org, ROLE_LABEL } from "./Org";

const TABS = [
  { id: "org", label: "组织" },
  { id: "keys", label: "密钥" },
  { id: "members", label: "成员" },
] as const;

export type TabId = (typeof TABS)[number]["id"];

/**
 * Whether `member` sees tab `id` of an active organisation. A billing member
 * holds no keys and only owners and admins manage members; a role a newer
 * control plane adds sees the organisation only, until this build knows it.
 */
function canSee(id: TabId, member: SessionMember): boolean {
  switch (id) {
    case "org":
      return true;
    case "keys":
      return member.role === "owner" || member.role === "admin" || member.role === "member";
    case "members":
      return member.role === "owner" || member.role === "admin";
  }
}

/** A tab whose page is not built yet. */
function NotBuilt({ label }: { label: string }) {
  return (
    <div className="panel">
      <h2>{label}</h2>
      <p className="hint">此页尚未开放。</p>
    </div>
  );
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
  const [tab, setTab] = useState<TabId>("org");

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
    return <Gate invite={invite} onIn={signIn} onLeaveInvite={() => setInvite(null)} />;
  }

  const { member, tenant } = me;
  // Only an active organisation has pages beyond its own: the control plane
  // refuses a pending or suspended one's members everything else.
  const visible = tenant.status === "active" ? TABS.filter((t) => canSee(t.id, member)) : [];
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
        {current === "org" ? (
          <Org member={member} status={tenant.status} />
        ) : (
          <NotBuilt label={TABS.find((t) => t.id === current)?.label ?? current} />
        )}
      </main>
    </div>
  );
}
