import { useRef, useState } from "react";
import { ApiError, MIN_PASSWORD_CHARS, type Member, type MemberRole } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { createMemberAccount } from "./lib/memberAccounts";
import { Button } from "./ui/Button";
import { Field, useRefusalFocus } from "./ui/Field";

type AccountField = "email" | "password" | "display_name" | "role";
type FieldErrors = Partial<Record<AccountField, string>>;

const ROLE_HELP: Record<MemberRole, string> = {
  owner: "可管理组织，以及其他所有者。",
  admin: "可管理成员、密钥与组织配置。",
  member: "使用获授权的模型，并管理自己的密钥。",
  billing: "查看账户与账单；不能持有可用密钥。",
};

export function CreateMember({ roles, labels, onInactive, onCreated, onBusyChange, onCancel }: {
  roles: MemberRole[];
  labels: Record<MemberRole, string>;
  onInactive: () => void;
  onCreated: (member: Member) => void;
  onBusyChange: (busy: boolean) => void;
  onCancel: () => void;
}) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [role, setRole] = useState<MemberRole>("member");
  const [errors, setErrors] = useState<FieldErrors>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submitting = useRef(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (submitting.current) return;
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
    onBusyChange(true);
    try {
      const member = await createMemberAccount({
        email,
        password,
        role,
        ...(displayName.trim() ? { display_name: displayName } : {}),
      });
      setEmail("");
      setPassword("");
      setDisplayName("");
      setRole("member");
      onCreated(member);
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
      onBusyChange(false);
    }
  }

  return (
    <form ref={form} className="create-member-form" aria-label="创建账号" aria-busy={busy} onSubmit={submit}>
      <fieldset className="create-member-group" disabled={busy}>
        <legend className="text-body font-medium">账号信息</legend>
        <div className="create-member-fields">
          <Field label="邮箱" error={errors.email} hint="新成员将使用此邮箱登录">
            {(aria) => <input type="email" autoComplete="off" required value={email} onChange={(e) => setEmail(e.target.value)} disabled={busy} {...aria} />}
          </Field>
          <Field label="显示名" error={errors.display_name} hint="选填，便于在成员列表中识别">
            {(aria) => <input autoComplete="off" value={displayName} onChange={(e) => setDisplayName(e.target.value)} disabled={busy} {...aria} />}
          </Field>
        </div>
      </fieldset>
      <fieldset className="create-member-group" disabled={busy}>
        <legend className="text-body font-medium">登录与权限</legend>
        <div className="create-member-fields">
          <Field label="初始密码" error={errors.password} hint={<>至少 {MIN_PASSWORD_CHARS} 个字符，请通过安全渠道交给新成员</>}>
            {(aria) => <input type="password" autoComplete="new-password" required value={password} onChange={(e) => setPassword(e.target.value)} disabled={busy} {...aria} />}
          </Field>
          <Field label="角色" error={errors.role} hint={ROLE_HELP[role]}>
            {(aria) => (
              <select aria-label="角色" value={role} onChange={(e) => setRole(e.target.value as MemberRole)} disabled={busy} {...aria}>
                {roles.map((value) => <option key={value} value={value}>{labels[value]}</option>)}
              </select>
            )}
          </Field>
        </div>
      </fieldset>
      {error && <div className="note crit" role="alert">{error}</div>}
      <div className="create-member-footer">
        {busy && <p className="mr-auto text-caption text-ink-soft" role="status">正在创建账号，请等待结果。</p>}
        <Button type="button" disabled={busy} onClick={onCancel}>取消</Button>
        <Button kind="primary" type="submit" busy={busy} busyLabel="创建账号中…">创建账号</Button>
      </div>
    </form>
  );
}
