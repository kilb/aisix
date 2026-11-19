import { useEffect, useRef, useState } from "react";
import { Activity, Fingerprint, KeyRound, UsersRound } from "lucide-react";
import { ApiError, MIN_PASSWORD_CHARS, acceptInvite, login, signup, type Session } from "./lib/api";
import {
  browserAuthenticationError,
  loginWithPasskey,
  openSocialAuthorization,
  sendEmailCode,
  SOCIAL_NAMES,
  SOCIAL_PROVIDERS,
  supportsPasskeys,
  useAuthenticationOptions,
  type SocialProvider,
} from "./lib/authentication";
import { formMessageFor } from "./lib/errors";
import { loadTurnstile, turnstileToken } from "./lib/turnstile";
import type { Theme } from "./lib/useTheme";
import { Button } from "./ui/Button";
import { Field } from "./ui/Field";
import { RouteNav } from "./ui/RouteNav";
import { ThemeToggle } from "./ui/ThemeToggle";
import { Card } from "./components/page";

type Mode = "login" | "signup";
type Pending = "submit" | "email_code" | "passkey" | SocialProvider;

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
  const [emailCode, setEmailCode] = useState("");
  const [sentTo, setSentTo] = useState<string | null>(null);
  const [resendBlocked, setResendBlocked] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const emailField = useRef<HTMLInputElement>(null);
  const turnstile = useRef<HTMLDivElement>(null);
  const { options, failed, retry } = useAuthenticationOptions(invite === null);
  const siteKey = options?.turnstile_site_key ?? null;
  const busy = pending !== null;

  useEffect(() => {
    if (siteKey) void loadTurnstile().catch(() => { /* 用户提交时可重试。 */ });
  }, [siteKey]);

  useEffect(() => {
    if (!resendBlocked) return;
    const timeout = window.setTimeout(() => setResendBlocked(false), 60_000);
    return () => window.clearTimeout(timeout);
  }, [resendBlocked]);

  function challenge(action: Parameters<typeof turnstileToken>[2]) {
    return turnstileToken(turnstile.current, siteKey, action, theme);
  }

  /** Sends one sign-in request; a refusal is shown as `describe` words it. */
  async function attempt(
    activity: Pending,
    request: () => Promise<Session | void>,
    describe: (e: ApiError) => string,
  ) {
    setErr(null);
    setPending(activity);
    try {
      const session = await request();
      if (session) onIn(session);
    } catch (e) {
      if (e instanceof ApiError) {
        const turnstileRefused = e.details.some(({ path }) => path === "/turnstile_token");
        const emailCodeRefused = e.details.some(({ path }) => path === "/email_code");
        const settingsChanged = e.code === "auth_settings_changed" || e.code === "authentication_settings_changed"
          || (turnstileRefused && !siteKey) || (emailCodeRefused && !options?.email_verification)
          || (e.code === "not_found" && activity !== "submit");
        if (invite === null && (settingsChanged || turnstileRefused || emailCodeRefused)) retry();
        setErr(settingsChanged ? "认证设置已更新，请按当前页面提示重新提交。" : describe(e));
      } else {
        setErr(browserAuthenticationError(e));
      }
    } finally {
      setPending(null);
    }
  }

  function requestEmailCode() {
    const input = emailField.current;
    if (busy || !options || !input?.reportValidity()) return;
    const address = input.value.trim();
    setEmail(address);
    void attempt("email_code", async () => {
      await sendEmailCode(address, await challenge("email_code"));
      setSentTo(address.toLowerCase());
      setResendBlocked(true);
    }, formMessageFor);
  }

  function passkeySignIn() {
    const input = emailField.current;
    if (busy || !options || !input?.reportValidity()) return;
    const address = input.value.trim();
    setEmail(address);
    void attempt("passkey", async () => loginWithPasskey(address, await challenge("passkey_login")), (error) =>
      error.code === "invalid_passkey" ? "无法使用 Passkey 登录，请确认邮箱或改用口令。" : formMessageFor(error),
    );
  }

  function socialSignIn(provider: SocialProvider) {
    if (busy || !options) return;
    void attempt(provider, async () => openSocialAuthorization(provider, {
      mode: "login", turnstile_token: await challenge("oauth_login"),
    }), formMessageFor);
  }

  function submit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    if (busy || (invite === null && !options)) return;
    // 自动填充可直接更新控件；提交读取表单，避免依赖它是否触发 React 的 change。
    const data = new FormData(e.currentTarget);
    const enteredPassword = String(data.get("password") ?? "");
    const enteredEmail = String(data.get("email") ?? "").trim();
    const enteredOrgName = String(data.get("organization") ?? "").trim();
    const name = String(data.get("display_name") ?? "").trim() || undefined;
    setPassword(enteredPassword);
    if (data.has("email")) setEmail(enteredEmail);
    if (data.has("organization")) setOrgName(enteredOrgName);
    if (data.has("display_name")) setDisplayName(name ?? "");
    if (invite === null && mode === "login") {
      void attempt("submit", async () => login(enteredEmail, enteredPassword, await challenge("login")), formMessageFor);
      return;
    }
    if (!longEnough(enteredPassword)) {
      setErr(TOO_SHORT);
      return;
    }
    if (invite !== null) {
      // An invitation that was used, revoked, has expired or never existed
      // answers 404 alike. One whose email has an account by now answers 409
      // `email_taken`, which only the inviter can sort out; the text does not
      // say why.
      void attempt(
        "submit",
        () => acceptInvite(invite, enteredPassword, name),
        (e) =>
          e.code === "not_found"
            ? "邀请无效或已过期"
            : e.code === "email_taken"
              ? "无法接受邀请，请联系邀请人"
              : formMessageFor(e),
      );
    } else {
      const code = String(data.get("email_code") ?? "").trim();
      void attempt("submit", async () => signup(enteredOrgName, enteredEmail, enteredPassword, name, {
        email_code: options?.email_verification ? code : undefined,
        turnstile_token: await challenge("signup"),
      }), formMessageFor);
    }
  }

  function switchMode(next: Mode) {
    if (next === mode || busy) return;
    setMode(next);
    setPassword("");
    setErr(null);
  }

  function leaveInvite() {
    setErr(null);
    setPassword("");
    onLeaveInvite();
  }

  const hinted = invite !== null || mode === "signup";
  const formId = `tenant-${invite !== null ? "invite" : mode}`;
  const canUsePasskey = options?.passkey && supportsPasskeys();
  const socialProviders = SOCIAL_PROVIDERS.filter((provider) => options?.[provider]);
  const sentHere = sentTo !== null && sentTo === email.trim().toLowerCase();
  const passwordField = (
    <Field label="口令" hint={hinted ? <>至少 {MIN_PASSWORD_CHARS} 个字符</> : undefined}>
      {(aria) => (
        <input
          id={`${formId}-password`}
          name="password"
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
        id={`${formId}-display-name`}
        name="display_name"
        type="text"
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
    <div className="gate-page tenant-gate">
      <header className="gate-topbar">
        <span className="brand">
          <span className="brand-mark" aria-hidden="true" />
          <span className="brand-name"><span className="mark">Routio</span> <span className="app">组织门户</span></span>
        </span>
        <ThemeToggle theme={theme} onToggle={onToggleTheme} />
      </header>
      <main id="gate" className="gate-layout">
        <Card tone="lilac" className="gate-introduction" aria-labelledby="gate-title">
          <h1 id="gate-title">组织门户</h1>
          <p className="gate-lead">{introduction}</p>
          <dl className="gate-guide">
            <div>
              <UsersRound aria-hidden="true" />
              <dt>组织与成员</dt>
              <dd>按角色管理访问，邀请成员或创建团队账号。</dd>
            </div>
            <div>
              <Activity aria-hidden="true" />
              <dt>调用与费用</dt>
              <dd>查看调用记录、用量与计费明细。</dd>
            </div>
            <div>
              <KeyRound aria-hidden="true" />
              <dt>密钥与额度</dt>
              <dd>设置密钥的模型范围，查看适用的预算限制。</dd>
            </div>
          </dl>
        </Card>
        <Card className="gate-panel" aria-labelledby="gate-form-title">
          <div className="gate-panel-heading">
            <h2 id="gate-form-title">{title}</h2>
          </div>
          {notice && <div className="note warn" role="status">{notice}</div>}
          {invite !== null ? (
            <>
              <p className="hint">设置口令后即加入邀请你的组织，之后用受邀的邮箱和这个口令登录。</p>
              <form id={formId} name={formId} method="post" className="gate-form" aria-label="接受邀请" onSubmit={submit}>
                {passwordField}
                {displayNameField}
                <Button kind="primary" block type="submit" busy={pending === "submit"}>
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
                <form key={mode} id={formId} name={formId} method="post" className="gate-form" aria-label={title} onSubmit={submit}>
                  {mode === "signup" && (
                    <Field label="组织名称">
                      <input
                        id={`${formId}-organization`}
                        name="organization"
                        type="text"
                        autoComplete="organization"
                        required
                        value={orgName}
                        onChange={(e) => setOrgName(e.target.value)}
                      />
                    </Field>
                  )}
                  <Field label="邮箱">
                    <input
                      ref={emailField}
                      id={`${formId}-email`}
                      name="email"
                      type="email"
                      autoComplete="username"
                      autoCapitalize="none"
                      spellCheck={false}
                      required
                      value={email}
                      onChange={(e) => {
                        setEmail(e.target.value);
                        setEmailCode("");
                      }}
                    />
                  </Field>
                  {mode === "signup" && options?.email_verification && (
                    <div className="gate-email-code">
                      <Field label="邮箱验证码">
                        <input name="email_code" type="text" autoComplete="one-time-code" inputMode="numeric" pattern="[0-9]{6}" maxLength={6} required
                          value={emailCode} onChange={(event) => setEmailCode(event.target.value)} />
                      </Field>
                      <Button onClick={requestEmailCode} busy={pending === "email_code"} disabled={busy || (sentHere && resendBlocked)}>
                        {sentHere ? "重新发送" : "发送验证码"}
                      </Button>
                      {sentHere && <p role="status" className="hint">验证码已发送，请检查收件箱。{resendBlocked ? "一分钟后可重新发送。" : ""}</p>}
                    </div>
                  )}
                  {passwordField}
                  {mode === "signup" && displayNameField}
                  {siteKey && <div ref={turnstile} className="gate-turnstile" />}
                  <Button kind="primary" block type="submit" busy={pending === "submit" || (!options && !failed)}
                    busyLabel={options ? "正在提交" : "正在读取登录设置"} disabled={busy || !options}>
                    {mode === "login" ? "登录" : "注册组织"}
                  </Button>
                </form>
                {mode === "login" && (canUsePasskey || socialProviders.length > 0) && (
                  <div className="gate-alternative">
                    <h3>其他登录方式</h3>
                    <div className="gate-alternative-actions">
                      {canUsePasskey && <Button onClick={passkeySignIn} disabled={busy} busy={pending === "passkey"}>
                        <Fingerprint aria-hidden /> 使用 Passkey 登录
                      </Button>}
                      {socialProviders.map((provider) => <Button key={provider} onClick={() => socialSignIn(provider)} disabled={busy} busy={pending === provider}>
                        使用 {SOCIAL_NAMES[provider]} 登录
                      </Button>)}
                    </div>
                    <p className="hint">首次使用，请先登录并在「账号设置」中添加登录方式。</p>
                  </div>
                )}
              </div>
              {failed && <div className="note crit" role="alert">无法读取登录设置。 <Button size="sm" onClick={retry}>重试</Button></div>}
              {alert}
              {mode === "signup" && (
                <p className="gate-foot">
                  新注册的组织需要平台审核。审核通过前可以登录查看组织状态、修改口令。
                </p>
              )}
            </>
          )}
        </Card>
      </main>
    </div>
  );
}
