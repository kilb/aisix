import { useState } from "react";
import { ApiError, login, type Operator } from "./lib/api";
import { messageFor } from "./lib/errors";
import type { Theme } from "./lib/useTheme";
import { Button } from "./ui/Button";
import { Moon, Sun } from "lucide-react";

/**
 * The login gate. The Dashboard edits the gateways' configuration, so
 * nothing else is rendered, not even the shell, until an operator signs in.
 */
export function Gate({
  onIn,
  theme,
  onToggleTheme,
  notice,
}: {
  onIn: (operator: Operator) => void;
  theme: Theme;
  onToggleTheme: () => void;
  /** Why the gate is shown again, such as a session that ended mid-use. */
  notice?: string | null;
}) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    setBusy(true);
    try {
      const operator = await login(email.trim(), password);
      setPassword("");
      onIn(operator);
    } catch (e) {
      setErr(e instanceof ApiError ? messageFor(e) : "登录失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div id="gate" className="cp-gate">
      <div className="cp-gate-theme">
        <Button
          kind="quiet"
          size="sm"
          onClick={onToggleTheme}
          aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
          title={theme === "dark" ? "切换到浅色" : "切换到深色"}
        >
          {theme === "dark" ? <Sun aria-hidden /> : <Moon aria-hidden />}
          {theme === "dark" ? "浅色" : "深色"}
        </Button>
      </div>

      <header className="cp-gate-intro">
        <div className="cp-gate-brand">
          <span className="brand-mark" aria-hidden="true" />
          <span>Routio</span>
        </div>
        <span className="font-mono text-meta text-muted-foreground">运维工作空间</span>
        <h1 className="cp-gate-title">Routio 运维台</h1>
        <p>连接上游，管理路由。<br />在一个工作空间查看网关、配置与计费。</p>
        <div className="cp-gate-meta">
          <span>网关与路由</span><span>组织与账务</span><span>配置下发状态</span>
        </div>
      </header>
      <div className="cp-gate-form">
        <form
          className="gate-form panel grid gap-0"
          onSubmit={submit}
        >
          <div className="mb-6">
            <div className="mb-3 font-mono text-meta text-muted-foreground" aria-hidden="true">01 — 账户</div>
            <h2 className="text-stat font-medium">运维登录</h2>
            <p className="mt-2 text-caption text-muted-foreground">使用平台分配的运维账号继续。</p>
          </div>
          {notice && (
            <div className="note warn mt-0 mb-5" role="status">
              {notice}
            </div>
          )}
          <label className="f">
            <span>邮箱</span>
            <input
              type="email"
              autoComplete="username"
              required
              autoFocus
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
          </label>
          <label className="f mb-6">
            <span>口令</span>
            <input
              type="password"
              autoComplete="current-password"
              required
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </label>
          <Button kind="primary" type="submit" block busy={busy} busyLabel="校验中…" className="mt-0">
            登录
          </Button>
          {err && (
            <div className="note crit mt-4 mb-0" role="alert">
              {err}
            </div>
          )}
        </form>
      </div>
    </div>
  );
}
