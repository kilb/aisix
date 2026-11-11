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
    <div id="gate" className="relative flex min-h-screen flex-col items-center justify-center bg-background px-4 py-16">
      <div className="absolute top-4 right-4 sm:top-6 sm:right-6">
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

      <div className="w-full max-w-[400px]">
        <div className="mb-8 flex flex-col items-center text-center">
          <span
            className="brand-mark size-11 rounded-[11px] shadow-xs after:top-[17px] after:right-[10px] after:left-[10px] after:h-[10px]"
            aria-hidden="true"
          />
          <h1 className="display mt-5 text-2xl font-semibold tracking-tight">Routio 运维台</h1>
          <p className="mt-1.5 text-sm text-muted-foreground">控制面 · 运维登录</p>
        </div>

        <form
          className="gate-form panel m-0 grid gap-0 rounded-2xl p-6 sm:p-8"
          onSubmit={submit}
        >
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
