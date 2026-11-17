import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useState } from "react";
import {
  ApiError,
  logout,
  onSessionEnded,
  session,
  type Session,
  type SessionMember,
  type SessionTenant,
} from "./lib/api";
import { EXPECTED_API_CONTRACT } from "./lib/contract";
import { messageFor } from "./lib/errors";
import { useTheme } from "./lib/useTheme";
import { Button } from "./ui/Button";
import { Activity, ChartNoAxesCombined, KeyRound, LayoutDashboard, Layers3, ReceiptText, UsersRound, Wallet, UserRound } from "lucide-react";
import { PORTAL_TABS as TABS, canSeePortalTab, portalRouteOf as routeOf, type PortalTabId } from "./lib/navigation";
import { RouteNav } from "./ui/RouteNav";
import { SidebarMenuButton, SidebarShell } from "./ui/Sidebar";
import { ThemeToggle } from "./ui/ThemeToggle";
import { Gate } from "./Gate";
import { Org, ROLE_LABEL } from "./Org";
import "./App.css";

// 业务页按实际导航加载，登录与概览不再下载全部管理表单。
const AccountSettings = lazy(() => import("./AccountSettings").then((module) => ({ default: module.AccountSettings })));
const Keys = lazy(() => import("./Keys").then((module) => ({ default: module.Keys })));
const KeyDetail = lazy(() => import("./KeyDetail").then((module) => ({ default: module.KeyDetail })));
const Members = lazy(() => import("./Members").then((module) => ({ default: module.Members })));
const MemberDetail = lazy(() => import("./MemberDetail").then((module) => ({ default: module.MemberDetail })));
const Billing = lazy(() => import("./Billing").then((module) => ({ default: module.Billing })));
const Budgets = lazy(() => import("./Budgets").then((module) => ({ default: module.Budgets })));
const Prices = lazy(() => import("./Prices").then((module) => ({ default: module.Prices })));
const Usage = lazy(() => import("./Usage").then((module) => ({ default: module.Usage })));

export type TabId = PortalTabId;

const NAV_ICONS = { org: LayoutDashboard, usage: ChartNoAxesCombined, calls: Activity, keys: KeyRound, prices: Layers3, budgets: Wallet, billing: ReceiptText, members: UsersRound, account: UserRound } as const;

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
  const [memberId, setMemberId] = useState<string | null>(null);
  const [keyId, setKeyId] = useState<string | null>(null);
  const [theme, toggleTheme] = useTheme();
  /** Whether the page has scrolled under the sticky header, which then shows its rule. */
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const read = () => setScrolled(window.scrollY > 0);
    read();
    window.addEventListener("scroll", read, { passive: true });
    return () => window.removeEventListener("scroll", read);
  }, []);

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
      setMemberId(route.memberId ?? null);
      setKeyId(route.keyId ?? null);
    };
    apply();
    window.addEventListener("hashchange", apply);
    return () => window.removeEventListener("hashchange", apply);
  }, []);

  // A section opened from the nav writes its own fragment, so the address
  // always names the section shown: a reload reopens it, and a link to the
  // section the address held before is a change again (a 查看价格 followed a
  // second time). Navigation entries also support the browser Back action.
  const openSection = useCallback((t: TabId) => {
    setTab(t);
    setPriceGroup(null);
    setMemberId(null);
    setKeyId(null);
    window.location.hash = `#${t}`;
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
    // 更新地址并卸载认证表单，让密码管理器识别异步登录已成功。
    window.history.replaceState(null, "", "#org");
    openSection("org");
  }, [openSection]);

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

  const visible = me ? TABS.filter((t) => canSeePortalTab(t.id, me.member.role, me.tenant.status)) : [];
  const current: TabId = visible.some((t) => t.id === tab) ? tab : "org";

  // 长列表进入新页时回到标题，避免把账号页等短页停在旧页的滚动位置。
  useLayoutEffect(() => {
    window.scrollTo({ top: 0, left: 0, behavior: "instant" });
  }, [current, memberId, keyId, me?.tenant.id, me?.member.id]);

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

  if (apiContract === null) return <div className="boot" role="status">载入中…</div>;

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
  const who = member.display_name || member.email;

  return (
    <SidebarShell
      storageKey="aisix.tenant.sidebar.collapsed"
      label="组织门户导航"
      routeKey={`${tenant.id}:${current}:${priceGroup ?? ""}:${memberId ?? ""}:${keyId ?? ""}`}
      brand={
        <span className="brand">
          <span className="brand-mark" aria-hidden="true" />
          <span className="brand-name">
            <span className="mark">Routio</span> <span className="app">组织门户</span>
          </span>
        </span>
      }
      context={
        <span className="tenant-context">
          <span className="tenant-context-avatar" aria-hidden="true">{[...tenant.name][0] ?? "组"}</span>
          <span className="tenant-context-copy">
          <span className="tenant-context-label">当前组织</span>
          <span className="org-name" title={tenant.name}>{tenant.name}</span>
          </span>
        </span>
      }
      navigation={(close) => visible.length > 0 && (
        <RouteNav
          label="组织门户分区"
          items={visible.filter((item) => item.id !== "account").map((item) => ({ ...item, icon: NAV_ICONS[item.id] }))}
          current={current}
          onSelect={(next) => {
            openSection(next);
            close();
          }}
          docked
        />
      )}
      footer={(close) => (
        <button type="button" id="stop-account" className="shell-account-link shell-member" title={member.email}
          aria-current={current === "account" ? "page" : undefined}
          onClick={() => { openSection("account"); close(); }}>
          <span className="avatar" data-initial={[...who][0] ?? ""} aria-hidden="true" />
          <span className="me-name">{who}</span>
          <span className="shell-account-caption">{ROLE_LABEL[member.role] ?? member.role} · 账号设置</span>
        </button>
      )}
    >
      <header className={scrolled ? "shell-view-header is-scrolled" : "shell-view-header"}>
        <div className="shell-view-location">
          <SidebarMenuButton />
          <span className="shell-location-text">
            <span className="shell-current-context" title={tenant.name}>{tenant.name}</span>
            <span className="shell-location-divider" aria-hidden="true">/</span>
            <span className="shell-current-label">{TABS.find((item) => item.id === current)?.label}</span>
          </span>
        </div>
        <div className="shell-user-tools">
          <ThemeToggle theme={theme} onToggle={toggleTheme} />
          <Button kind="quiet" onClick={() => openSection("account")} aria-label="账号设置">
            <UserRound aria-hidden />
            <span className="cp-account-label">账号</span>
          </Button>
        </div>
      </header>

      {signOutError && (
        <div className="route-bar-in shell-note">
          <div className="note crit signout-err" role="alert">
            {signOutError}
          </div>
        </div>
      )}

      <main id="workspace-main" tabIndex={-1} className="page shell-content">
        <section
          key={`${tenant.id}:${member.id}:${member.role}:${tenant.status}`}
          id={`tab-${current}`}
          role="region"
          aria-labelledby={visible.length > 0 ? `stop-${current}` : undefined}
        >
          <Suspense fallback={<div className="min-h-16 text-caption text-muted" role="status">载入中…</div>}>
          {current === "account" ? (
            <AccountSettings member={member} onSignOut={() => void signOut()} />
          ) : current === "keys" ? (
            keyId ? <KeyDetail key={keyId} keyId={keyId} member={member} status={tenant.status} onSelfChanged={() => void boot()} onBack={() => openSection("keys")} /> : <Keys member={member} status={tenant.status} onSelfChanged={() => void boot()} />
          ) : current === "prices" ? (
            <Prices initialGroup={priceGroup} />
          ) : current === "members" ? (
            memberId ? <MemberDetail key={memberId} memberId={memberId} member={member} status={tenant.status} onBack={() => openSection("members")} onSelfChanged={() => void boot()} onLeft={leave} /> :
              <Members member={member} status={tenant.status} onSelfChanged={() => void boot()} onLeft={leave} onOpenMember={(id) => { setMemberId(id); window.location.hash = `#members?member=${encodeURIComponent(id)}`; }} />
          ) : current === "usage" || current === "calls" ? (
            <Usage member={member} page={current === "calls" ? "calls" : "analysis"} />
          ) : current === "budgets" ? (
            <Budgets member={member} />
          ) : current === "billing" ? (
            <Billing member={member} />
          ) : (
            <Org member={member} status={tenant.status} />
          )}
          </Suspense>
        </section>
      </main>
    </SidebarShell>
  );
}
