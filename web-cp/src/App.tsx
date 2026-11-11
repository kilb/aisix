import { useCallback, useEffect, useState } from "react";
import {
  ApiError,
  environment,
  logout,
  onSessionEnded,
  rollout,
  session,
  tenants,
  topups,
  type Environment,
  type Operator,
  type OperatorRole,
  type RolloutSummary,
  type Tenant,
  type Topup,
} from "./lib/api";
import { EXPECTED_API_CONTRACT } from "./lib/contract";
import { errorText, fmtInt } from "./lib/fmt";
import { useTheme } from "./lib/useTheme";
import { ChangePassword } from "./components/ChangePassword";
import { Button } from "./ui/Button";
import { Lamp, type LampState } from "./ui/Lamp";
import { RouteNav } from "./ui/RouteNav";
import {
  Braces,
  Building2,
  DatabaseZap,
  KeyRound,
  LayoutDashboard,
  Moon,
  Plug,
  ScrollText,
  Server,
  ShieldUser,
  Sun,
  Tag,
  Wallet,
  Waypoints,
} from "lucide-react";
import { Gate } from "./Gate";
import { Audit } from "./tabs/Audit";
import { Cache } from "./tabs/Cache";
import { Gateways } from "./tabs/Gateways";
import { Keys } from "./tabs/Keys";
import { Models } from "./tabs/Models";
import { Billing } from "./tabs/Billing";
import { Operators } from "./tabs/Operators";
import { Overview } from "./tabs/Overview";
import { Pricing } from "./tabs/Pricing";
import { Providers } from "./tabs/Providers";
import { Resources } from "./tabs/Resources";
import { Tenants } from "./tabs/Tenants";

const TABS = [
  { id: "overview", label: "概览", icon: LayoutDashboard },
  { id: "gateways", label: "网关", icon: Server },
  { id: "providers", label: "上游账号", icon: Plug },
  { id: "models", label: "模型与路由", icon: Waypoints },
  { id: "pricing", label: "价格", icon: Tag },
  { id: "tenants", label: "租户", icon: Building2 },
  { id: "billing", label: "计费", icon: Wallet },
  { id: "keys", label: "密钥", icon: KeyRound },
  { id: "cache", label: "缓存策略", icon: DatabaseZap },
  { id: "resources", label: "全部资源", icon: Braces },
  { id: "operators", label: "运维账号", icon: ShieldUser, adminOnly: true },
  { id: "audit", label: "审计日志", icon: ScrollText },
] as const;

export type TabId = (typeof TABS)[number]["id"];

/** What every tab page is given. */
export interface TabProps {
  operator: Operator;
  /** False for a viewer: the page renders no edit controls. */
  canEdit: boolean;
  /** Opens `tab`, and on a tab of sections the section named `section`. */
  onGoto: (tab: TabId, section?: string) => void;
  /** The section a link asked this tab to open. */
  initialSection?: string;
  /** Counts the links that named a section, so asking for the same one again still switches. */
  sectionVisit?: number;
  /** Reads the shell's attention markers again, after a page changed what they show. */
  refreshShell: () => void;
}

const ROLE_LABEL: Record<OperatorRole, string> = {
  admin: "管理员",
  operator: "运维",
  viewer: "只读",
};

/** How often the shell reads the fleet's state again for its header and markers. */
const SHELL_REFRESH_MS = 10_000;

/** What the shell reads for its header and the sections' attention markers. */
export interface ShellData {
  env: Environment | null;
  summary: RolloutSummary | null;
  pendingTenants: Tenant[] | null;
  pendingTopups: Topup[] | null;
}

/**
 * The sections whose stop carries an attention marker. Each check reads only
 * what the shell already holds and says, in words, what needs attention
 * there; null when nothing does. Entries are appended, one per source.
 */
const ATTENTION: { tab: TabId; check: (d: ShellData) => string | null }[] = [
  {
    tab: "overview",
    check: ({ summary }) =>
      summary && summary.rejected.length > 0 ? `${fmtInt(summary.rejected.length)} 个资源被网关拒绝` : null,
  },
  {
    tab: "tenants",
    check: ({ pendingTenants }) =>
      pendingTenants && pendingTenants.length > 0 ? `${fmtInt(pendingTenants.length)} 个租户待审核` : null,
  },
  {
    tab: "billing",
    check: ({ pendingTopups }) => (pendingTopups && pendingTopups.length > 0 ? "有待处理事项" : null),
  },
];

/** One lamp for the whole fleet: what stops a saved document first, else how far rollouts have got. */
function fleetLamp(s: RolloutSummary): { state: LampState; text: string } {
  if (s.projection_blocked || s.projection_stalled) return { state: "rejected", text: "下发已停止" };
  if (s.rejected.length > 0) return { state: "rejected", text: "有资源被拒绝" };
  if (s.pending + s.propagating > 0) return { state: "propagating", text: "下发中" };
  if (s.online_instances === 0) return { state: "disabled", text: "无在线网关" };
  return { state: "applied", text: "已生效" };
}

/**
 * The header's environment, version and fleet lamp, and the attention
 * markers' data, read while an operator is signed in. A failed read keeps
 * what was last read; the pages report their own failures.
 */
function useShellData(signedIn: boolean, tab: TabId): ShellData & { refresh: () => void } {
  const [env, setEnv] = useState<Environment | null>(null);
  const [summary, setSummary] = useState<RolloutSummary | null>(null);
  const [pendingTenants, setPendingTenants] = useState<Tenant[] | null>(null);
  const [pendingTopups, setPendingTopups] = useState<Topup[] | null>(null);
  const [reads, setReads] = useState(0);

  useEffect(() => {
    if (!signedIn) return;
    let live = true;
    environment.get().then(
      (e) => live && setEnv(e),
      () => undefined,
    );
    return () => {
      live = false;
    };
  }, [signedIn]);

  // Read again on every change of section, so a marker clears once its cause is dealt with.
  useEffect(() => {
    if (!signedIn) return;
    let live = true;
    let timer: number | undefined;
    const tick = async () => {
      const [s, p, t] = await Promise.allSettled([
        rollout.summary(),
        tenants.list("pending"),
        topups.list({ status: "pending" }),
      ]);
      if (!live) return;
      if (s.status === "fulfilled") setSummary(s.value);
      if (p.status === "fulfilled") setPendingTenants(p.value);
      if (t.status === "fulfilled") setPendingTopups(t.value.items);
      timer = window.setTimeout(() => void tick(), SHELL_REFRESH_MS);
    };
    void tick();
    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [signedIn, tab, reads]);

  const refresh = useCallback(() => setReads((n) => n + 1), []);
  return { env, summary, pendingTenants, pendingTopups, refresh };
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

/** The page of `tab`. Each tab's task replaces its own case. */
function Page({ tab, ...props }: TabProps & { tab: TabId }) {
  switch (tab) {
    case "overview":
      return <Overview {...props} />;
    case "gateways":
      return <Gateways {...props} />;
    case "providers":
      return <Providers {...props} />;
    case "models":
      return <Models {...props} />;
    case "pricing":
      return <Pricing {...props} />;
    case "tenants":
      return <Tenants {...props} />;
    case "billing":
      return <Billing {...props} />;
    case "keys":
      return <Keys {...props} />;
    case "cache":
      return <Cache {...props} />;
    case "resources":
      return <Resources {...props} />;
    case "operators":
      return <Operators {...props} />;
    case "audit":
      return <Audit {...props} />;
    default:
      return <NotBuilt label={TABS.find((t) => t.id === tab)?.label ?? tab} />;
  }
}

export function App() {
  // null until the session is read: defaulting to signed-out would flash
  // the gate on every reload, and most reloads are signed in.
  const [apiContract, setApiContract] = useState<number | null>(null);
  const [operator, setOperator] = useState<Operator | null>(null);
  const [bootError, setBootError] = useState<string | null>(null);
  const [signOutError, setSignOutError] = useState<string | null>(null);
  const [tab, setTabOnly] = useState<TabId>("overview");
  const [section, setSection] = useState<string | undefined>(undefined);
  // Each link that names a section bumps this: the same section asked for twice
  // must still switch, and the name alone would not change.
  const [sectionVisit, setSectionVisit] = useState(0);
  const setTab = useCallback((t: TabId, s?: string) => {
    setTabOnly(t);
    setSection(s);
    if (s !== undefined) setSectionVisit((n) => n + 1);
  }, []);
  const [sessionEnded, setSessionEnded] = useState(false);
  const [passwordOpen, setPasswordOpen] = useState(false);
  const [theme, toggleTheme] = useTheme();
  const shell = useShellData(operator !== null && apiContract === EXPECTED_API_CONTRACT, tab);

  const boot = useCallback(async () => {
    setBootError(null);
    try {
      const s = await session();
      setApiContract(s.apiContract);
      setOperator(s.authed ? s.operator : null);
    } catch (e) {
      setBootError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void boot();
  }, [boot]);

  // The contract is read again whenever the window regains focus: a control
  // plane redeployed while the page stayed open stops it as a skew at boot
  // would. A failed read changes nothing; the next request reports it.
  useEffect(() => {
    const recheck = () => {
      session().then(
        (s) => setApiContract(s.apiContract),
        () => undefined,
      );
    };
    window.addEventListener("focus", recheck);
    return () => window.removeEventListener("focus", recheck);
  }, []);

  // Any request that finds the session gone, on any page, drops back to the
  // gate. Only while signed in: a request still in flight when the operator
  // signed out must not report their own sign-out as an expiry.
  useEffect(() => {
    if (!operator) return;
    return onSessionEnded(() => {
      setOperator(null);
      setPasswordOpen(false);
      setSessionEnded(true);
    });
  }, [operator]);

  const signIn = useCallback((op: Operator) => {
    setOperator(op);
    setSignOutError(null);
    setSessionEnded(false);
    setPasswordOpen(false);
    setTab("overview");
  }, []);

  // Signed out only once the control plane has revoked the session (or says
  // there was none): a failed request leaves the session live, and showing
  // the gate then would tell the operator they are out while they are not.
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
    setOperator(null);
  }, []);

  if (bootError) {
    return (
      <div className="centered">
        <div className="note crit" role="alert">
          <strong>无法读取登录状态。</strong> {bootError}{" "}
          <Button size="sm" onClick={() => void boot()}>
            重试
          </Button>
        </div>
      </div>
    );
  }

  if (apiContract === null) return <div className="boot" />;

  // A skew stops everything, reads included, rather than showing a banner
  // over pages that keep editing: this build's idea of the API's shapes no
  // longer holds, so whatever it saves could write a document it misread.
  if (apiContract !== EXPECTED_API_CONTRACT) {
    const stale = apiContract < EXPECTED_API_CONTRACT ? "控制面" : "界面";
    return (
      <div className="centered">
        <div className="note crit skew">
          <strong>界面与控制面的接口版本不一致，已停止一切操作。</strong>
          <p>
            界面期望契约 v{EXPECTED_API_CONTRACT}，控制面报告 v{apiContract}——{stale}
            是旧的那一侧，需要更新它。
          </p>
          <p className="hint">
            两者分开部署：界面是 <code>web-cp/</code> 的构建产物，控制面是{" "}
            <code>aisix-control-plane</code> 二进制。只更新了一侧就会出现这个状态。
          </p>
        </div>
      </div>
    );
  }

  if (!operator) {
    return (
      <Gate
        onIn={signIn}
        theme={theme}
        onToggleTheme={toggleTheme}
        notice={sessionEnded ? "登录已过期，请重新登录。" : null}
      />
    );
  }

  const visible = TABS.filter((t) => !("adminOnly" in t) || operator.role === "admin");
  const current: TabId = visible.some((t) => t.id === tab) ? tab : "overview";
  const attention: Partial<Record<TabId, string>> = {};
  for (const { tab: id, check } of ATTENTION) {
    const note = check(shell);
    if (note) attention[id] = note;
  }
  const fleet = shell.summary && fleetLamp(shell.summary);

  const here = visible.find((t) => t.id === current);
  return (
    <div className="shell flex min-h-screen">
      {/* The sidebar from 1024px. Below it the aside's box goes away: the
          brand and the operator move to the top bar's row, and the sections
          become the bar along the bottom (RouteNav). */}
      <aside className="rail flex w-60 shrink-0 flex-col border-r border-sidebar-border bg-sidebar/80 backdrop-blur-xl max-lg:contents lg:sticky lg:top-0 lg:h-screen">
        <div className="brand flex h-14 shrink-0 items-center gap-2.5 px-4 max-lg:hidden">
          <span className="brand-mark" aria-hidden="true" />
          <h1 className="display text-[15px] font-semibold tracking-tight">Routio 运维台</h1>
          <span className="brand-sub vh">控制面</span>
        </div>
        <div className="flex-1 overflow-y-auto px-3 pb-3 max-lg:contents">
          <RouteNav items={visible} current={current} attention={attention} label="Dashboard 分区" onSelect={(t) => setTab(t)} />
        </div>
        <div className="who flex shrink-0 items-center gap-2.5 border-t border-sidebar-border px-4 py-3 max-lg:hidden">
          <span
            aria-hidden="true"
            className="grid size-8 shrink-0 place-items-center rounded-full bg-sidebar-accent text-xs font-semibold text-foreground uppercase"
          >
            {(operator.display_name || operator.email).slice(0, 1)}
          </span>
          <span className="min-w-0 flex-1 leading-tight">
            <span className="who-name block truncate text-sm font-medium" title={operator.email}>
              {operator.display_name || operator.email}
            </span>
            <span className="tag mt-1">{ROLE_LABEL[operator.role]}</span>
          </span>
        </div>
      </aside>

      <div className="shell-main min-w-0 flex-1">
        <header className="topbar sticky top-0 z-30 flex h-14 items-center gap-3 border-b border-border/70 bg-background/70 px-4 backdrop-blur-xl sm:px-6 lg:px-8">
          <span className="brand-mark lg:hidden" aria-hidden="true" />
          <nav aria-label="位置" className="flex min-w-0 items-center gap-1.5 text-sm">
            <span className="text-muted-foreground max-sm:hidden">运维台</span>
            <span className="text-muted-foreground/60 max-sm:hidden" aria-hidden="true">
              /
            </span>
            <span className="truncate font-medium">{here?.label}</span>
          </nav>
          <div className="ctx ml-auto flex min-w-0 items-center gap-2">
            {shell.env && (
              <span className="hidden items-center gap-2 rounded-full border border-border bg-card/80 px-3 py-1 text-xs text-muted-foreground xl:inline-flex">
                <span>
                  环境 <code className="text-foreground">{shell.env.env_id}</code>
                </span>
                <span className="h-3 w-px bg-border" aria-hidden="true" />
                <span>控制面 {shell.env.cp_version}</span>
              </span>
            )}
            {fleet && shell.summary && (
              <span className="fleet inline-flex items-center gap-2 rounded-full border border-border bg-card/80 px-3 py-1 text-xs">
                <Lamp state={fleet.state} title="配置下发状态">
                  {fleet.text}
                </Lamp>
                <span className="ctx-note text-muted-foreground max-sm:hidden">在线网关 {fmtInt(shell.summary.online_instances)}</span>
              </span>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-1">
            <Button
              kind="quiet"
              size="sm"
              onClick={toggleTheme}
              aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
              title={theme === "dark" ? "切换到浅色" : "切换到深色"}
            >
              {theme === "dark" ? <Sun aria-hidden /> : <Moon aria-hidden />}
              <span className="max-sm:hidden">{theme === "dark" ? "浅色" : "深色"}</span>
            </Button>
            <Button kind="quiet" size="sm" aria-expanded={passwordOpen} onClick={() => setPasswordOpen((o) => !o)}>
              修改口令
            </Button>
            <Button kind="quiet" size="sm" onClick={() => void signOut()}>
              登出
            </Button>
          </div>
        </header>

        <div className="workspace mx-auto w-full max-w-[1760px] px-4 pt-6 pb-24 sm:px-6 lg:px-8 lg:pt-8 lg:pb-12">
          {passwordOpen && <ChangePassword onClose={() => setPasswordOpen(false)} />}

          {signOutError && (
            <div className="note crit" role="alert">
              {signOutError}
            </div>
          )}

          <main>
            <section id={`tab-${current}`} role="tabpanel" aria-labelledby={`stop-${current}`}>
              <Page
                tab={current}
                operator={operator}
                canEdit={operator.role !== "viewer"}
                onGoto={setTab}
                refreshShell={shell.refresh}
                initialSection={section}
                sectionVisit={sectionVisit}
              />
            </section>
          </main>
        </div>
      </div>
    </div>
  );
}
