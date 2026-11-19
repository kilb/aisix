import { useEffect, useRef, useState } from "react";
import { ArrowRight, Building, Eye, EyeOff, Fingerprint, GitFork, Globe, LockKeyhole, Mail, UserRound } from "lucide-react";
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
import { PasswordRecoveryForm } from "./PasswordRecoveryForm";
import { Button } from "./ui/Button";
import { Field } from "./ui/Field";
import { RouteNav } from "./ui/RouteNav";
import { ThemeToggle } from "./ui/ThemeToggle";
import "./PublicTheme.css";
import "./Gate.css";

type Mode = "login" | "signup";
type Pending = "submit" | "email_code" | "passkey" | SocialProvider;

const MODES = [
  { id: "login", label: "登录" },
  { id: "signup", label: "注册组织" },
] as const satisfies readonly { id: Mode; label: string }[];

const TOO_SHORT = `口令至少需要 ${MIN_PASSWORD_CHARS} 个字符`;
const HOME_URL = import.meta.env.BASE_URL;
const SOCIAL_ICONS = { github: GitFork, google: Globe };

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
  mode,
  onModeChange,
  invite,
  signedIn,
  notice,
  onIn,
  onLeaveInvite,
}: {
  theme: Theme;
  onToggleTheme: () => void;
  mode: Mode;
  onModeChange: (mode: Mode) => void;
  /** The invitation token, when the page was opened from an invitation link. */
  invite: string | null;
  /** Whether a session is live behind an invitation form opened over it. */
  signedIn: boolean;
  /** Why the member is here again, such as a session that ended. */
  notice: string | null;
  onIn: (session: Session) => void;
  onLeaveInvite: () => void;
}) {
  const formId = `tenant-${invite !== null ? "invite" : mode}`;
  const [previousForm, setPreviousForm] = useState(formId);
  const [orgName, setOrgName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [passwordVisible, setPasswordVisible] = useState(false);
  const [displayName, setDisplayName] = useState("");
  const [emailCode, setEmailCode] = useState("");
  const [sentTo, setSentTo] = useState<string | null>(null);
  const [resendBlocked, setResendBlocked] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [recovering, setRecovering] = useState(false);
  const [recovered, setRecovered] = useState(false);
  const emailField = useRef<HTMLInputElement>(null);
  const turnstile = useRef<HTMLDivElement>(null);
  const { options, failed, retry } = useAuthenticationOptions(invite === null);
  const siteKey = options?.turnstile_site_key ?? null;
  const busy = pending !== null;

  // 前进和后退也会切换表单。邮箱等草稿保留，口令和提示随表单清除。
  if (previousForm !== formId) {
    setPreviousForm(formId);
    setPassword("");
    setPasswordVisible(false);
    setErr(null);
    setRecovering(false);
    setRecovered(false);
  }

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
    onModeChange(next);
  }

  function openRecovery() {
    setEmail(emailField.current?.value.trim() ?? email);
    setPassword("");
    setPasswordVisible(false);
    setErr(null);
    setRecovered(false);
    setRecovering(true);
  }

  function closeRecovery(address: string, completed: boolean) {
    setEmail(address);
    setRecovering(false);
    setRecovered(completed);
  }

  function leaveInvite() {
    setErr(null);
    setPassword("");
    setPasswordVisible(false);
    onLeaveInvite();
  }

  const hinted = invite !== null || mode === "signup";
  const canUsePasskey = options?.passkey && supportsPasskeys();
  const socialProviders = SOCIAL_PROVIDERS.filter((provider) => options?.[provider]);
  const sentHere = sentTo !== null && sentTo === email.trim().toLowerCase();
  const passwordField = (
    <div className="portal-auth__password">
      <Field label="口令" hint={hinted ? <>至少 {MIN_PASSWORD_CHARS} 个字符</> : undefined}>
        {(aria) => (
          <span className="portal-auth__control">
            <LockKeyhole aria-hidden />
            <input
              id={`${formId}-password`}
              name="password"
              type={passwordVisible ? "text" : "password"}
              autoComplete={invite === null && mode === "login" ? "current-password" : "new-password"}
              placeholder={hinted ? "设置口令" : "请输入口令"}
              required
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              {...aria}
            />
          </span>
        )}
      </Field>
      <Button
        className="portal-auth__reveal"
        kind="quiet"
        iconOnly
        aria-controls={`${formId}-password`}
        aria-label={passwordVisible ? "隐藏口令" : "显示口令"}
        onClick={() => setPasswordVisible((visible) => !visible)}
      >
        {passwordVisible ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
      </Button>
    </div>
  );

  const displayNameField = (
    <Field label="显示名">
      <span className="portal-auth__control">
        <UserRound aria-hidden />
        <input
          id={`${formId}-display-name`}
          name="display_name"
          type="text"
          autoComplete="name"
          placeholder="选填"
          value={displayName}
          onChange={(e) => setDisplayName(e.target.value)}
        />
      </span>
    </Field>
  );

  const alert = err && (
    <div className="note crit" role="alert">
      {err}
    </div>
  );

  const title = invite !== null ? "接受邀请" : recovering ? "找回口令" : mode === "login" ? "登录" : "注册组织";

  return (
    <div className="portal-auth">
      <header className="portal-auth__header">
        <div className="portal-auth__topbar">
          <a href={HOME_URL} className="portal-auth__brand" aria-label="Routio 首页">
            <span className="brand-mark" aria-hidden />
            Routio
          </a>
          <div className="portal-auth__header-actions">
            <a href={HOME_URL}>返回首页</a>
            <ThemeToggle theme={theme} onToggle={onToggleTheme} />
          </div>
        </div>
      </header>
      <main id="gate" className="portal-auth__layout">
        <aside className="portal-auth__art">
          <p className="portal-auth__wordmark">Routio</p>
          <p className="portal-auth__descriptor">AI 模型网关</p>
          <a href={`${HOME_URL}#pricing`}>查看模型价格 <ArrowRight aria-hidden /></a>
        </aside>
        <section className="portal-auth__panel" aria-labelledby="gate-form-title">
          <h1 id="gate-form-title">{title}{title === "登录" ? " Routio" : ""}</h1>
          {notice && <div className="note warn" role="status">{notice}</div>}
          {recovered && <div className="note" role="status">口令已重置，请用新口令登录。</div>}
          {invite !== null ? (
            <>
              <p className="hint">设置口令后即加入邀请你的组织，之后用受邀的邮箱和这个口令登录。</p>
              <form id={formId} name={formId} method="post" className="portal-auth__form" aria-label="接受邀请" onSubmit={submit}>
                {passwordField}
                {displayNameField}
                <Button kind="primary" block type="submit" busy={pending === "submit"}>
                  接受邀请
                </Button>
                {alert}
              </form>
              <Button className="portal-auth__secondary" block onClick={leaveInvite}>
                {signedIn ? "取消" : "返回登录"}
              </Button>
            </>
          ) : recovering ? (
            <PasswordRecoveryForm initialEmail={email} theme={theme} siteKey={siteKey}
              available={options?.password_recovery === true} onRefresh={retry}
              onBack={(address) => closeRecovery(address, false)} onComplete={(address) => closeRecovery(address, true)} />
          ) : (
            <>
              <RouteNav
                label="登录或注册"
                items={MODES}
                current={mode}
                onSelect={switchMode}
                look="underline"
                className="portal-auth__modes"
              />
              <div className="portal-auth__tab" id={`tab-${mode}`} role="tabpanel" aria-labelledby={`stop-${mode}`}>
                <form key={mode} id={formId} name={formId} method="post" className="portal-auth__form" aria-label={title} onSubmit={submit}>
                  {mode === "signup" && (
                    <Field label="组织名称">
                      <span className="portal-auth__control">
                        <Building aria-hidden />
                        <input
                          id={`${formId}-organization`}
                          name="organization"
                          type="text"
                          autoComplete="organization"
                          placeholder="请输入组织名称"
                          required
                          value={orgName}
                          onChange={(e) => setOrgName(e.target.value)}
                        />
                      </span>
                    </Field>
                  )}
                  <Field label="邮箱">
                    <span className="portal-auth__control">
                      <Mail aria-hidden />
                      <input
                        ref={emailField}
                        id={`${formId}-email`}
                        name="email"
                        type="email"
                        autoComplete="username"
                        autoCapitalize="none"
                        spellCheck={false}
                        placeholder="请输入邮箱"
                        required
                        value={email}
                        onChange={(e) => {
                          setEmail(e.target.value);
                          setEmailCode("");
                        }}
                      />
                    </span>
                  </Field>
                  {mode === "signup" && options?.email_verification && (
                    <div className="portal-auth__email-code">
                      <Field label="邮箱验证码">
                        <input name="email_code" type="text" autoComplete="one-time-code" inputMode="numeric" pattern="[0-9]{6}" maxLength={6} required
                          placeholder="六位验证码"
                          value={emailCode} onChange={(event) => setEmailCode(event.target.value)} />
                      </Field>
                      <Button className="portal-auth__secondary" onClick={requestEmailCode} busy={pending === "email_code"} disabled={busy || (sentHere && resendBlocked)}>
                        {sentHere ? "重新发送" : "发送验证码"}
                      </Button>
                      {sentHere && <p role="status" className="hint">验证码已发送，请检查收件箱。{resendBlocked ? "一分钟后可重新发送。" : ""}</p>}
                    </div>
                  )}
                  {passwordField}
                  {mode === "login" && options?.password_recovery && (
                    <Button kind="quiet" className="portal-auth__forgot" disabled={busy} onClick={openRecovery}>
                      忘记口令？
                    </Button>
                  )}
                  {mode === "signup" && displayNameField}
                  {siteKey && <div ref={turnstile} className="portal-auth__turnstile" />}
                  <Button kind="primary" block type="submit" busy={pending === "submit" || (!options && !failed)}
                    busyLabel={options ? "正在提交" : "正在读取登录设置"} disabled={busy || !options}>
                    {mode === "login" ? "登录" : "注册组织"}
                  </Button>
                  {failed && <div className="note crit" role="alert">无法读取登录设置。 <Button size="sm" onClick={retry}>重试</Button></div>}
                  {alert}
                </form>
                {mode === "login" && (canUsePasskey || socialProviders.length > 0) && (
                  <section className="portal-auth__alternative" aria-labelledby="gate-alternative-title">
                    <h2 id="gate-alternative-title">其他登录方式</h2>
                    <div className="portal-auth__alternative-actions">
                      {canUsePasskey && <Button className="portal-auth__secondary" onClick={passkeySignIn} disabled={busy} busy={pending === "passkey"}>
                        <Fingerprint aria-hidden /> 使用 Passkey 登录
                      </Button>}
                      {socialProviders.map((provider) => {
                        const Icon = SOCIAL_ICONS[provider];
                        return <Button key={provider} className="portal-auth__secondary" onClick={() => socialSignIn(provider)} disabled={busy} busy={pending === provider}>
                          <Icon aria-hidden /> 使用 {SOCIAL_NAMES[provider]} 登录
                        </Button>;
                      })}
                    </div>
                    <p className="hint">首次使用，请先登录并在「账号设置」中添加登录方式。</p>
                  </section>
                )}
              </div>
              {mode === "signup" && (
                <p className="hint">
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
