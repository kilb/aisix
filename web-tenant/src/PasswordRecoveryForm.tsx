import { useEffect, useRef, useState, type FormEvent } from "react";
import { Eye, EyeOff, LockKeyhole, Mail } from "lucide-react";
import { ApiError, MIN_PASSWORD_CHARS } from "./lib/api";
import { browserAuthenticationError, completePasswordRecovery, requestPasswordRecovery } from "./lib/authentication";
import { formMessageFor } from "./lib/errors";
import { turnstileToken } from "./lib/turnstile";
import type { Theme } from "./lib/useTheme";
import { Button } from "./ui/Button";
import { Field } from "./ui/Field";

export function PasswordRecoveryForm({
  initialEmail, theme, siteKey, available, onRefresh, onBack, onComplete,
}: {
  initialEmail: string;
  theme: Theme;
  siteKey: string | null;
  available: boolean;
  onRefresh: () => void;
  onBack: (email: string) => void;
  onComplete: (email: string) => void;
}) {
  const [email, setEmail] = useState(initialEmail);
  const [sent, setSent] = useState(false);
  const [resendBlocked, setResendBlocked] = useState(false);
  const [passwordVisible, setPasswordVisible] = useState(false);
  const [pending, setPending] = useState<"send" | "reset" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const emailField = useRef<HTMLInputElement>(null);
  const codeField = useRef<HTMLInputElement>(null);
  const turnstile = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!available) return;
    (sent ? codeField : emailField).current?.focus();
  }, [available, sent]);

  useEffect(() => {
    if (!resendBlocked) return;
    const timeout = window.setTimeout(() => setResendBlocked(false), 60_000);
    return () => window.clearTimeout(timeout);
  }, [resendBlocked]);

  async function submit(
    kind: "send" | "reset",
    run: () => Promise<void>,
  ) {
    setPending(kind);
    setError(null);
    try {
      await run();
    } catch (err) {
      if (!(err instanceof ApiError)) {
        setError(browserAuthenticationError(err));
      } else if (err.code === "not_found" || err.code === "authentication_settings_changed"
        || err.details.some(({ path }) => path === "/turnstile_token")) {
        onRefresh();
        setError(err.code === "not_found" ? "当前无法找回口令。" : formMessageFor(err));
      } else {
        setError(formMessageFor(err));
      }
    } finally {
      setPending(null);
    }
  }

  function send(address: string) {
    if (pending || resendBlocked || !available) return;
    setEmail(address);
    void submit("send", async () => {
      await requestPasswordRecovery(address, await turnstileToken(turnstile.current, siteKey, "password_recovery", theme));
      setSent(true);
      setResendBlocked(true);
    });
  }

  function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (pending || !available) return;
    const data = new FormData(event.currentTarget);
    const address = String(data.get("email") ?? "").trim();
    if (!sent) {
      send(address);
      return;
    }
    const password = String(data.get("new_password") ?? "");
    if ([...password].length < MIN_PASSWORD_CHARS) {
      setError(`口令至少需要 ${MIN_PASSWORD_CHARS} 个字符`);
      return;
    }
    void submit("reset", async () => {
      await completePasswordRecovery(address, String(data.get("email_code") ?? "").trim(), password);
      onComplete(address);
    });
  }

  return (
    <>
      {available && (
        <form className="portal-auth__form" method="post" aria-label="找回口令" onSubmit={onSubmit}>
          <Field label="邮箱">
            <span className="portal-auth__control">
              <Mail aria-hidden />
              <input ref={emailField} name="email" type="email" autoComplete="username" autoCapitalize="none"
                spellCheck={false} placeholder="请输入登录邮箱" required value={email} readOnly={sent || pending !== null}
                onChange={(event) => setEmail(event.target.value)} />
            </span>
          </Field>
          {sent && (
            <>
              <p className="hint" role="status">如果该邮箱可以找回口令，验证码将发送到你的收件箱。</p>
              <div className="portal-auth__email-code">
                <Field label="邮箱验证码">
                  <input ref={codeField} name="email_code" type="text" autoComplete="one-time-code" inputMode="numeric"
                    pattern="[0-9]{6}" maxLength={6} required placeholder="六位验证码" />
                </Field>
                <Button className="portal-auth__secondary" busy={pending === "send"}
                  disabled={pending !== null || resendBlocked} onClick={() => send(email)}>
                  重新发送
                </Button>
              </div>
              <div className="portal-auth__password">
                <Field label="新口令" hint={`至少 ${MIN_PASSWORD_CHARS} 个字符`}>
                  {(aria) => (
                    <span className="portal-auth__control">
                      <LockKeyhole aria-hidden />
                      <input id="password-recovery-new" name="new_password" type={passwordVisible ? "text" : "password"}
                        autoComplete="new-password" placeholder="设置新口令" required {...aria} />
                    </span>
                  )}
                </Field>
                <Button kind="quiet" iconOnly className="portal-auth__reveal" aria-controls="password-recovery-new"
                  aria-label={passwordVisible ? "隐藏口令" : "显示口令"} onClick={() => setPasswordVisible((value) => !value)}>
                  {passwordVisible ? <EyeOff aria-hidden /> : <Eye aria-hidden />}
                </Button>
              </div>
            </>
          )}
          {siteKey && <div ref={turnstile} className="portal-auth__turnstile" />}
          <Button kind="primary" block type="submit" busy={pending === (sent ? "reset" : "send")} disabled={pending !== null}>
            {sent ? "重置口令" : "发送验证码"}
          </Button>
        </form>
      )}
      {error && <div className="note crit" role="alert">{error}</div>}
      <Button className="portal-auth__secondary" block disabled={pending !== null} onClick={() => onBack(email)}>
        返回登录
      </Button>
    </>
  );
}
