import { useRef, useState } from "react";
import { ApiError, MIN_PASSWORD_CHARS, members } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { Field, useRefusalFocus } from "./ui/Field";
import { FormDialog } from "./ui/FormDialog";
import { TwoStep } from "./ui/TwoStep";

/** 新密码只留在本次表单内；成功与关闭都会卸载并清除，不写存储或日志。 */
export function ResetMemberPassword({ memberId, onClose, onSaved, onInactive }: {
  memberId: string;
  onClose: () => void;
  onSaved: () => void;
  onInactive: () => void;
}) {
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const [passwordError, setPasswordError] = useState<string | null>(null);
  const [confirmationError, setConfirmationError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function reset() {
    if (pending.current) return;
    setPasswordError(null); setConfirmationError(null); setError(null);
    let refused = false;
    if ([...password].length < MIN_PASSWORD_CHARS) { setPasswordError(`新密码至少需要 ${MIN_PASSWORD_CHARS} 个字符`); refused = true; }
    if (confirmation !== password || confirmation === "") { setConfirmationError("两次输入的密码不一致"); refused = true; }
    if (refused) { focusRefusal(); return; }
    pending.current = true;
    setBusy(true);
    try {
      await members.resetPassword(memberId, password);
      setPassword(""); setConfirmation("");
      onSaved();
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onInactive();
      if (e instanceof ApiError && e.code === "validation_failed" && e.details.some((detail) => detail.path === "/new_password")) {
        setPasswordError(e.details.filter((detail) => detail.path === "/new_password").map((detail) => detail.message).join("；"));
        focusRefusal();
      } else setError(e instanceof ApiError ? formMessageFor(e) : String(e));
    } finally { pending.current = false; setBusy(false); }
  }

  return <FormDialog open onOpenChange={(open) => { if (!open) onClose(); }} busy={busy} title="重置密码" description="该成员的现有登录会话将结束。请通过安全渠道交付新密码。">
    <form ref={form} className="grid gap-4" aria-label="重置密码" onSubmit={(event) => event.preventDefault()}>
      <Field label="新密码" error={passwordError} hint={`至少 ${MIN_PASSWORD_CHARS} 个字符`}>{(aria) => <input type="password" autoComplete="new-password" required value={password} onChange={(event) => setPassword(event.target.value)} disabled={busy} {...aria} />}</Field>
      <Field label="再次输入新密码" error={confirmationError}>{(aria) => <input type="password" autoComplete="new-password" required value={confirmation} onChange={(event) => setConfirmation(event.target.value)} disabled={busy} {...aria} />}</Field>
      {error && <div className="note crit" role="alert">{error}</div>}
      <div className="flex flex-wrap justify-end gap-2"><TwoStep label="重置密码" busy={busy} warning="确认后，该成员的旧密码、现有会话、通行密钥和第三方账号绑定立即失效。" onConfirm={() => void reset()} /></div>
    </form>
  </FormDialog>;
}
