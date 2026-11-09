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
import { useTheme } from "./lib/useTheme";
import { Button } from "./ui/Button";
import { RouteNav } from "./ui/RouteNav";
import { ThemeToggle } from "./ui/ThemeToggle";
import { Gate } from "./Gate";
import { Keys } from "./Keys";
import { Members } from "./Members";
import { Org, ROLE_LABEL } from "./Org";
import { Billing } from "./Billing";
import { Budgets } from "./Budgets";
import { Prices } from "./Prices";
import { Usage } from "./Usage";

const TABS = [
  { id: "org", label: "组织" },
  { id: "keys", label: "密钥" },
  { id: "prices", label: "价格" },
  { id: "members", label: "成员" },
  { id: "usage", label: "用量" },
  { id: "budgets", label: "预算" },
  { id: "billing", label: "账户" },
] as const;

export type TabId = (typeof TABS)[number]["id"];

/**
 * What the address asks for, when it names a section: `#<tab>` or
 * `#<tab>?group=<id>`. The portal is one page whose sections are state, so
 * the fragment is the only part of the address a link can carry; it is read
 * once at boot (`#prices?group=<id>` from a key form's 查看价格) and again
 * whenever it changes while the page is open. A fragment that names no
 * known tab opens the organisation, as an absent one does.
 */
function routeOf(hash: string): { tab: TabId; group: string | null } {
  const [name, query = ""] = hash.replace(/^#/, "").split("?", 2);
  const tab = TABS.find((t) => t.id === name)?.id ?? "org";
  const group = new URLSearchParams(query).get("group");
  return { tab, group: group === "" ? null : group };
}

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
 *
 * The usage, budgets and account pages show only for an active organisation,
 * because their routes answer `403 tenant_not_active` otherwise: usage and
 * budgets to every known role, the account to those who see money (owner,
 * admin, billing).
 */
function canSee(id: TabId, member: SessionMember, status: TenantStatus): boolean {
  if (status !== "active" && status !== "suspended") return false;
  if ((id === "usage" || id === "budgets" || id === "billing" || id === "prices") && status !== "active") return false;
  switch (id) {
    case "org":
      return true;
    case "keys":
      return member.role === "owner" || member.role === "admin" || member.role === "member";
    case "members":
      return member.role === "owner" || member.role === "admin";
    case "usage":
    case "budgets":
    case "prices":
      return member.role === "owner" || member.role === "admin" || member.role === "member" || member.role === "billing";
    case "billing":
      return member.role === "owner" || member.role === "admin" || member.role === "billing";
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
  /** The group a `#prices?group=<id>` link named, read when that page opens. */
  const [priceGroup, setPriceGroup] = useState<string | null>(null);
  const [theme, toggleTheme] = useTheme();

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

  // The address opens a section, and a link to one opens it without a
  // reload: `#prices?group=<id>` from a key form's 查看价格. Read once here,
  // then on every change while the page is open.
  useEffect(() => {
    const apply = () => {
      const route = routeOf(window.location.hash);
      setTab(route.tab);
      setPriceGroup(route.group);
    };
    apply();
    window.addEventListener("hashchange", apply);
    return () => window.removeEventListener("hashchange", apply);
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
          <Button size="sm" icon="refresh" onClick={() => void boot()}>
            重试
          </Button>
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
        theme={theme}
        onToggleTheme={toggleTheme}
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
    <div className="shell">
      <header className="topbar">
        <div className="wrap">
          <span className="brand">
            <span className="mark">AISIX</span> <span className="app">组织门户</span>
          </span>
          <h1 className="org-name" title={tenant.name}>
            {tenant.name}
          </h1>
          {/* The separator stays in the text (and in what a screen reader
              hears); on screen the role reads as a tag after the name. */}
          <span className="me" title={member.email}>
            <span className="me-name">{member.display_name || member.email}</span>
            <span className="vh"> · </span>
            <span className="tag">{ROLE_LABEL[member.role] ?? member.role}</span>
          </span>
          <span className="tools">
            <ThemeToggle theme={theme} onToggle={toggleTheme} />
            <Button kind="quiet" size="sm" onClick={() => void signOut()}>
              登出
            </Button>
          </span>
        </div>
      </header>

      {signOutError && (
        <div className="wrap">
          <div className="note crit signout-err" role="alert">
            {signOutError}
          </div>
        </div>
      )}

      {visible.length > 0 && (
        <div className="wrap route-bar">
          <RouteNav label="组织门户分区" items={visible} current={current} onSelect={setTab} docked />
        </div>
      )}

      <main className="wrap page">
        <section
          id={`tab-${current}`}
          role={visible.length > 0 ? "tabpanel" : undefined}
          aria-labelledby={visible.length > 0 ? `stop-${current}` : undefined}
        >
          {current === "keys" ? (
            <Keys member={member} status={tenant.status} onSelfChanged={() => void boot()} />
          ) : current === "prices" ? (
            <Prices initialGroup={priceGroup} />
          ) : current === "members" ? (
            <Members member={member} status={tenant.status} onSelfChanged={() => void boot()} onLeft={leave} />
          ) : current === "usage" ? (
            <Usage member={member} />
          ) : current === "budgets" ? (
            <Budgets member={member} />
          ) : current === "billing" ? (
            <Billing member={member} />
          ) : (
            <Org member={member} status={tenant.status} />
          )}
        </section>
      </main>
    </div>
  );
}
