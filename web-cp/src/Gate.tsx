import { useState } from "react";
import { ApiError, login, type Operator } from "./lib/api";
import { messageFor } from "./lib/errors";
import type { Theme } from "./lib/useTheme";
import { Button } from "./ui/Button";

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
    <div id="gate">
      <div className="gate-card">
        <div className="gate-plate">
          <div className="gate-mark">
            <h1 className="display">AISIX Dashboard</h1>
            <span className="gate-sub">控制面 · 运维登录</span>
          </div>
          <Button
            kind="quiet"
            size="sm"
            onClick={onToggleTheme}
            aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
            title={theme === "dark" ? "切换到浅色" : "切换到深色"}
          >
            {theme === "dark" ? "浅色" : "深色"}
          </Button>
        </div>
        {/* The route line: two stops passed, the sign at the end of it. */}
        <div className="gate-line" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>

        <form className="gate-form panel" onSubmit={submit}>
          {notice && (
            <div className="note warn" role="status">
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
          <label className="f">
            <span>口令</span>
            <input
              type="password"
              autoComplete="current-password"
              required
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </label>
          <Button kind="primary" type="submit" busy={busy} busyLabel="校验中…">
            登录
          </Button>
          {err && (
            <div className="note crit" role="alert">
              {err}
            </div>
          )}
        </form>
      </div>
    </div>
  );
}
