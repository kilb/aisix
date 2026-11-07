import { useCallback, useEffect, useState } from "react";
import { ApiError, logout, session, type Operator, type OperatorRole } from "./lib/api";
import { EXPECTED_API_CONTRACT } from "./lib/contract";
import { messageFor } from "./lib/errors";
import { useTheme } from "./lib/useTheme";
import { Gate } from "./Gate";
import { Keys } from "./tabs/Keys";
import { Overview } from "./tabs/Overview";
import { Tenants } from "./tabs/Tenants";

const TABS = [
  { id: "overview", label: "概览" },
  { id: "gateways", label: "网关" },
  { id: "providers", label: "上游账号" },
  { id: "models", label: "模型与路由" },
  { id: "tenants", label: "租户" },
  { id: "keys", label: "密钥" },
  { id: "cache", label: "缓存策略" },
  { id: "resources", label: "全部资源" },
  { id: "operators", label: "运维账号", adminOnly: true },
  { id: "audit", label: "审计日志" },
] as const;

export type TabId = (typeof TABS)[number]["id"];

/** What every tab page is given. */
export interface TabProps {
  operator: Operator;
  /** False for a viewer: the page renders no edit controls. */
  canEdit: boolean;
  onGoto: (tab: TabId) => void;
}

const ROLE_LABEL: Record<OperatorRole, string> = {
  admin: "管理员",
  operator: "运维",
  viewer: "只读",
};

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
    case "tenants":
      return <Tenants {...props} />;
    case "keys":
      return <Keys {...props} />;
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
  const [tab, setTab] = useState<TabId>("overview");
  const [theme, toggleTheme] = useTheme();

  const boot = useCallback(async () => {
    setBootError(null);
    try {
      const s = await session();
      setApiContract(s.apiContract);
      setOperator(s.authed ? s.operator : null);
    } catch (e) {
      setBootError(e instanceof ApiError ? messageFor(e) : String(e));
    }
  }, []);

  useEffect(() => {
    void boot();
  }, [boot]);

  const signIn = useCallback((op: Operator) => {
    setOperator(op);
    setSignOutError(null);
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
          <button className="ghost" onClick={() => void boot()}>
            重试
          </button>
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
        <div className="note crit" style={{ margin: "24px" }}>
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

  if (!operator) return <Gate onIn={signIn} theme={theme} onToggleTheme={toggleTheme} />;

  const visible = TABS.filter((t) => !("adminOnly" in t) || operator.role === "admin");
  const current: TabId = visible.some((t) => t.id === tab) ? tab : "overview";

  return (
    <div className="frame">
      <header className="book-head">
        <div className="book-mark">
          <h1>AISIX Dashboard</h1>
          <span className="book-sub">控制面</span>
        </div>
        <div className="book-foli">
          <span className="pill value" title={operator.email}>
            {operator.display_name || operator.email} · {ROLE_LABEL[operator.role]}
          </span>
          <button
            className="ghost"
            onClick={toggleTheme}
            aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
            title={theme === "dark" ? "切换到浅色" : "切换到深色"}
          >
            {theme === "dark" ? "浅色" : "深色"}
          </button>
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

      <div className="leafs">
        <nav className="tabs" role="tablist" aria-label="Dashboard 分区">
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

        <main>
          <section id={`tab-${current}`}>
            <Page
              tab={current}
              operator={operator}
              canEdit={operator.role !== "viewer"}
              onGoto={setTab}
            />
          </section>
        </main>
      </div>
    </div>
  );
}
