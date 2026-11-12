import { useState } from "react";
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
    <Field label="口令" hint={hinted ? <>至少 {MIN_PASSWORD_CHARS} 个字符</> : undefined}>
      {(aria) => (
        <input
          type="password"
          autoComplete={invite === null && mode === "login" ? "current-password" : "new-password"}
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          {...aria}
        />
      )}
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

  const title = invite !== null ? "接受邀请" : mode === "login" ? "登录" : "注册组织";
  const introduction = invite !== null
    ? "接受邀请，加入团队的工作空间。用自己的口令管理访问。"
    : mode === "login"
      ? "在一个工作空间中管理成员与 API 密钥，核对调用的用量、费用与额度。"
      : "创建组织与所有者账号。平台审核通过后，即可为团队创建密钥并管理调用。";

  return (
    <div className="gate-page">
      <header className="gate-topbar">
        <span className="brand">
          <span className="brand-mark" aria-hidden="true" />
          <span className="brand-name"><span className="mark">Routio</span> <span className="app">组织门户</span></span>
        </span>
        <ThemeToggle theme={theme} onToggle={onToggleTheme} />
      </header>
      <main id="gate" className="gate-layout">
        <section className="gate-introduction" aria-labelledby="gate-title">
          <p className="gate-eyebrow">组织工作空间</p>
          <h1 id="gate-title">组织门户</h1>
          <p className="gate-lead">{introduction}</p>
          <dl className="gate-guide">
            <div>
              <dt><span aria-hidden="true">01</span> 组织与成员</dt>
              <dd>按角色管理访问，邀请成员或创建团队账号。</dd>
            </div>
            <div>
              <dt><span aria-hidden="true">02</span> 调用与费用</dt>
              <dd>查看调用记录、用量与计费明细。</dd>
            </div>
            <div>
              <dt><span aria-hidden="true">03</span> 密钥与额度</dt>
              <dd>设置密钥的模型范围，查看适用的预算限制。</dd>
            </div>
          </dl>
        </section>
        <section className="panel gate-panel" aria-labelledby="gate-form-title">
          <div className="gate-panel-heading">
            <p className="gate-eyebrow">工作空间访问</p>
            <h2 id="gate-form-title">{title}</h2>
          </div>
          {notice && <div className="note warn" role="status">{notice}</div>}
          {invite !== null ? (
            <>
              <p className="hint">设置口令后即加入邀请你的组织，之后用受邀的邮箱和这个口令登录。</p>
              <form className="gate-form" onSubmit={submit}>
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
            </>
          ) : (
            <>
              <RouteNav
                label="登录或注册"
                items={MODES}
                current={mode}
                onSelect={switchMode}
                className="gate-modes"
              />
              <div id={`tab-${mode}`} role="tabpanel" aria-labelledby={`stop-${mode}`}>
                <form className="gate-form" onSubmit={submit}>
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
              </div>
              {alert}
              {mode === "signup" && (
                <p className="gate-foot">
                  新注册的组织需要平台审核。审核通过前可以登录查看组织状态、修改口令。
                </p>
              )}
            </>
          )}
        </section>
      </main>
    </div>
  );
}
