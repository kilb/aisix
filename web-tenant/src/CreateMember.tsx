import { useRef, useState } from "react";
import { ApiError, MIN_PASSWORD_CHARS, type MemberRole } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { createMemberAccount } from "./lib/memberAccounts";
import { Card, CardHeader } from "./components/page";
import { Button } from "./ui/Button";
import { Field, useRefusalFocus } from "./ui/Field";

type AccountField = "email" | "password" | "display_name" | "role";
type FieldErrors = Partial<Record<AccountField, string>>;

export function CreateMember({ roles, labels, onInactive }: {
  roles: MemberRole[];
  labels: Record<MemberRole, string>;
  onInactive: () => void;
}) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [role, setRole] = useState<MemberRole>("member");
  const [errors, setErrors] = useState<FieldErrors>({});
  const [error, setError] = useState<string | null>(null);
  const [created, setCreated] = useState(false);
  const [busy, setBusy] = useState(false);
  const submitting = useRef(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (submitting.current) return;
    setCreated(false);
    setError(null);
    const found: FieldErrors = {};
    if ([...password].length < MIN_PASSWORD_CHARS) found.password = `初始密码至少需要 ${MIN_PASSWORD_CHARS} 个字符`;
    setErrors(found);
    if (Object.keys(found).length > 0) {
      focusRefusal();
      return;
    }
    submitting.current = true;
    setBusy(true);
    try {
      await createMemberAccount({
        email,
        password,
        role,
        ...(displayName.trim() ? { display_name: displayName } : {}),
      });
      setEmail("");
      setPassword("");
      setDisplayName("");
      setRole("member");
      setCreated(true);
    } catch (e) {
      // 邮箱冲突仅表示本次创建失败，不能用来重置已有账号。
      if (e instanceof ApiError && e.code === "email_taken") {
        setErrors({ email: "该邮箱已被使用，请使用其他邮箱" });
        focusRefusal();
      } else if (e instanceof ApiError && e.code === "validation_failed" && e.details.length > 0) {
        const refused: FieldErrors = {};
        const rest: string[] = [];
        for (const detail of e.details) {
          const field = detail.path.slice(1);
          if (field === "email" || field === "password" || field === "display_name" || field === "role") {
            refused[field] = refused[field] ? `${refused[field]}；${detail.message}` : detail.message;
          } else rest.push(detail.message);
        }
        setErrors(refused);
        if (rest.length > 0) setError(rest.join("；"));
        focusRefusal();
      } else {
        setError(e instanceof ApiError ? formMessageFor(e) : String(e));
        if (e instanceof ApiError && e.code === "tenant_not_active") onInactive();
      }
    } finally {
      submitting.current = false;
      setBusy(false);
    }
  }

  return (
    <section role="region" aria-label="创建账号">
      <Card>
        <CardHeader title="创建账号" />
        <p className="hint">新成员可直接使用邮箱和初始密码登录本组织。已有账号的密码不会被修改。</p>
        <form ref={form} className="mt-4 grid gap-4 sm:grid-cols-2" onSubmit={submit}>
          <Field label="显示名" error={errors.display_name}>
            {(aria) => <input autoComplete="off" value={displayName} onChange={(e) => setDisplayName(e.target.value)} disabled={busy} {...aria} />}
          </Field>
          <Field label="邮箱" error={errors.email}>
            {(aria) => <input type="email" autoComplete="off" required value={email} onChange={(e) => setEmail(e.target.value)} disabled={busy} {...aria} />}
          </Field>
          <Field label="初始密码" error={errors.password} hint={<>至少 {MIN_PASSWORD_CHARS} 个字符，请通过安全渠道交给新成员</>}>
            {(aria) => <input type="password" autoComplete="new-password" required value={password} onChange={(e) => setPassword(e.target.value)} disabled={busy} {...aria} />}
          </Field>
          <Field label="角色" error={errors.role}>
            {(aria) => (
              <select aria-label="角色" value={role} onChange={(e) => setRole(e.target.value as MemberRole)} disabled={busy} {...aria}>
                {roles.map((value) => <option key={value} value={value}>{labels[value]}</option>)}
              </select>
            )}
          </Field>
          {error && <div className="note crit col-span-full" role="alert">{error}</div>}
          {created && <div className="note ok col-span-full" role="status">账号已创建，可使用邮箱和初始密码登录</div>}
          <div className="col-span-full flex justify-end">
            <Button kind="primary" type="submit" busy={busy} busyLabel="创建账号中…">创建账号</Button>
          </div>
        </form>
      </Card>
    </section>
  );
}
