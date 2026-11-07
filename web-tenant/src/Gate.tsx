import { useId, useState } from "react";
import { ApiError, MIN_PASSWORD_CHARS, acceptInvite, login, signup, type Session } from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";

type Mode = "login" | "signup";

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
  invite,
  onIn,
  onLeaveInvite,
}: {
  /** The invitation token, when the page was opened from an invitation link. */
  invite: string | null;
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
      // answers 404 alike.
      void attempt(
        () => acceptInvite(invite, password, name),
        (e) => (e.code === "not_found" ? "邀请无效或已过期" : formMessageFor(e)),
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

  const passwordField = (
    <>
      <label className="f">
        <span>口令</span>
        <input
          type="password"
          autoComplete={invite === null && mode === "login" ? "current-password" : "new-password"}
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          aria-describedby={invite !== null || mode === "signup" ? hintId : undefined}
        />
      </label>
      {(invite !== null || mode === "signup") && (
        <p className="hint" id={hintId}>
          至少 {MIN_PASSWORD_CHARS} 个字符
        </p>
      )}
    </>
  );

  const displayNameField = (
    <label className="f">
      <span>显示名</span>
      <input
        autoComplete="name"
        placeholder="选填"
        value={displayName}
        onChange={(e) => setDisplayName(e.target.value)}
      />
    </label>
  );

  const alert = err && (
    <div className="note crit" role="alert">
      {err}
    </div>
  );

  return (
    <div id="gate">
      <header className="plate">
        <h1>AISIX 组织门户</h1>
        <span className="sub">组织、成员与 API 密钥</span>
      </header>

      {invite !== null ? (
        <div className="panel">
          <h2>接受邀请</h2>
          <p className="hint">设置口令后即加入邀请你的组织，之后用受邀的邮箱和这个口令登录。</p>
          <form onSubmit={submit}>
            {passwordField}
            {displayNameField}
            <button className="act" type="submit" disabled={busy}>
              {busy ? "处理中…" : "接受邀请"}
            </button>
          </form>
          {alert}
          <button className="ghost" type="button" onClick={leaveInvite}>
            返回登录
          </button>
        </div>
      ) : (
        <div className="panel">
          <div className="seg" role="tablist" aria-label="登录或注册">
            <button type="button" role="tab" aria-selected={mode === "login"} onClick={() => switchMode("login")}>
              登录
            </button>
            <button type="button" role="tab" aria-selected={mode === "signup"} onClick={() => switchMode("signup")}>
              注册组织
            </button>
          </div>
          <form onSubmit={submit}>
            {mode === "signup" && (
              <label className="f">
                <span>组织名称</span>
                <input
                  autoComplete="organization"
                  required
                  value={orgName}
                  onChange={(e) => setOrgName(e.target.value)}
                />
              </label>
            )}
            <label className="f">
              <span>邮箱</span>
              <input
                type="email"
                autoComplete="username"
                required
                value={email}
                onChange={(e) => setEmail(e.target.value)}
              />
            </label>
            {passwordField}
            {mode === "signup" && displayNameField}
            <button className="act" type="submit" disabled={busy}>
              {busy ? "处理中…" : mode === "login" ? "登录" : "注册组织"}
            </button>
          </form>
          {alert}
        </div>
      )}

      {invite === null && mode === "signup" && (
        <p className="foot">新注册的组织需要平台审核。审核通过前可以登录查看组织状态、修改口令。</p>
      )}
    </div>
  );
}
