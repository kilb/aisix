import { useState } from "react";
import { ApiError, login, type Operator } from "./lib/api";
import { messageFor } from "./lib/errors";
import type { Theme } from "./lib/useTheme";

/**
 * The login gate. The Dashboard edits the gateways' configuration, so
 * nothing else is rendered, not even the shell, until an operator signs in.
 */
export function Gate({
  onIn,
  theme,
  onToggleTheme,
}: {
  onIn: (operator: Operator) => void;
  theme: Theme;
  onToggleTheme: () => void;
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
      <div className="gate-plate">
        <div className="gate-mark">
          <h1>AISIX Dashboard</h1>
          <span className="gate-sub">控制面 · 运维登录</span>
        </div>
        <button
          className="ghost"
          onClick={onToggleTheme}
          aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}
          title={theme === "dark" ? "切换到浅色" : "切换到深色"}
        >
          {theme === "dark" ? "浅色" : "深色"}
        </button>
      </div>

      <form className="gate-form panel" onSubmit={submit}>
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
        <button className="act" type="submit" disabled={busy}>
          {busy ? "校验中…" : "登录"}
        </button>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
      </form>
    </div>
  );
}
