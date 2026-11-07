import { useId, useState } from "react";
import { ApiError, MIN_PASSWORD_CHARS, acceptInvite, login, signup, type Session } from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import type { Theme } from "./lib/useTheme";
import { Button } from "./ui/Button";
import { Field } from "./ui/Field";
import { RouteNav } from "./ui/RouteNav";
import { ThemeToggle } from "./ui/ThemeToggle";

type Mode = "login" | "signup";

const MODES = [
  { id: "login", label: "登录" },
  { id: "signup", label: "注册组织" },
] as const satisfies readonly { id: Mode; label: string }[];

const TOO_SHORT = `口令至少需要 ${MIN_PASSWORD_CHARS} 个字符`;

/** Whether `password` is long enough, counted in characters as the control plane counts them. */
function longEnough(password: string): boolean {
  return [...password].length >= MIN_PASSWORD_CHARS;
}

/**
 * The gate: logging in and signing an organisation up or, opened from an
 * invitation link, accepting the invitation. Nothing of an organisation is
 * rendered until someone is signed in.
 *
 * No answer here tells whether an email has an account: login shows one
 * text for a wrong password and an unknown email alike, and sign-up's
 * "email taken" reads as a plain refusal (lib/errors.ts).
 */
export function Gate({
  theme,
  onToggleTheme,
  invite,
  signedIn,
  notice,
  onIn,
  onLeaveInvite,
}: {
  theme: Theme;
  onToggleTheme: () => void;
  /** The invitation token, when the page was opened from an invitation link. */
  invite: string | null;
  /** Whether a session is live behind an invitation form opened over it. */
  signedIn: boolean;
  /** Why the member is here again, such as a session that ended. */
  notice: string | null;
  onIn: (session: Session) => void;
  onLeaveInvite: () => void;
}) {
  const [mode, setMode] = useState<Mode>("login");
  const [orgName, setOrgName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const hintId = useId();

  /** Sends one sign-in request; a refusal is shown as `describe` words it. */
  async function attempt(request: () => Promise<Session>, describe: (e: ApiError) => string) {
    setErr(null);
    setBusy(true);
    try {
      const session = await request();
      setPassword("");
      onIn(session);
    } catch (e) {
      setErr(e instanceof ApiError ? describe(e) : "请求失败");
    } finally {
      setBusy(false);
    }
  }

  function submit(e: React.FormEvent) {
    e.preventDefault();
    const name = displayName.trim() || undefined;
    if (invite === null && mode === "login") {
      void attempt(() => login(email.trim(), password), messageFor);
      return;
    }
    if (!longEnough(password)) {
      setErr(TOO_SHORT);
      return;
    }
    if (invite !== null) {
      // An invitation that was used, revoked, has expired or never existed
      // answers 404 alike. One whose email has an account by now answers 409
      // `email_taken`, which only the inviter can sort out; the text does not
      // say why.
      void attempt(
        () => acceptInvite(invite, password, name),
        (e) =>
          e.code === "not_found"
            ? "邀请无效或已过期"
            : e.code === "email_taken"
              ? "无法接受邀请，请联系邀请人"
              : formMessageFor(e),
      );
    } else {
      void attempt(() => signup(orgName.trim(), email.trim(), password, name), formMessageFor);
    }
  }

  function switchMode(next: Mode) {
    setMode(next);
    setErr(null);
  }

  function leaveInvite() {
    setErr(null);
    setPassword("");
    onLeaveInvite();
  }

  const hinted = invite !== null || mode === "signup";
  const passwordField = (
    <Field label="口令" hint={hinted ? <>至少 {MIN_PASSWORD_CHARS} 个字符</> : undefined} hintId={hintId}>
      <input
        type="password"
        autoComplete={invite === null && mode === "login" ? "current-password" : "new-password"}
        required
        value={password}
        onChange={(e) => setPassword(e.target.value)}
        aria-describedby={hinted ? hintId : undefined}
      />
    </Field>
  );

  const displayNameField = (
    <Field label="显示名">
      <input
        autoComplete="name"
        placeholder="选填"
        value={displayName}
        onChange={(e) => setDisplayName(e.target.value)}
      />
    </Field>
  );

  const alert = err && (
    <div className="note crit" role="alert">
      {err}
    </div>
  );

  return (
    <div className="gate-page">
      <div className="gate-tools">
        <ThemeToggle theme={theme} onToggle={onToggleTheme} />
      </div>
      <div id="gate">
        <header className="plate">
          <h1>
            <span className="mark">AISIX</span> 组织门户
          </h1>
          <span className="sub">组织、成员与 API 密钥</span>
        </header>

        {notice && (
          <div className="note warn" role="status">
            {notice}
          </div>
        )}

        {invite !== null ? (
          <div className="panel">
            <h2>接受邀请</h2>
            <p className="hint">设置口令后即加入邀请你的组织，之后用受邀的邮箱和这个口令登录。</p>
            <form className="form gate-form" onSubmit={submit}>
              {passwordField}
              {displayNameField}
              <Button kind="primary" block type="submit" busy={busy}>
                接受邀请
              </Button>
            </form>
            {alert}
            <Button block onClick={leaveInvite}>
              {signedIn ? "取消" : "返回登录"}
            </Button>
          </div>
        ) : (
          <div className="panel">
            <RouteNav label="登录或注册" items={MODES} current={mode} onSelect={switchMode} />
            <form className="form gate-form" onSubmit={submit}>
              {mode === "signup" && (
                <Field label="组织名称">
                  <input
                    autoComplete="organization"
                    required
                    value={orgName}
                    onChange={(e) => setOrgName(e.target.value)}
                  />
                </Field>
              )}
              <Field label="邮箱">
                <input
                  type="email"
                  autoComplete="username"
                  required
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                />
              </Field>
              {passwordField}
              {mode === "signup" && displayNameField}
              <Button kind="primary" block type="submit" busy={busy}>
                {mode === "login" ? "登录" : "注册组织"}
              </Button>
            </form>
            {alert}
          </div>
        )}

        {invite === null && mode === "signup" && (
          <p className="foot">新注册的组织需要平台审核。审核通过前可以登录查看组织状态、修改口令。</p>
        )}
      </div>
    </div>
  );
}
