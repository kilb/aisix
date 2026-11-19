import { useEffect, useState, type FormEvent } from "react";
import { Fingerprint, Link2, Trash2 } from "lucide-react";
import { ApiError } from "../lib/api";
import {
  browserAuthenticationError,
  disconnectSocial,
  openSocialAuthorization,
  passkeys,
  registerPasskey,
  removePasskey,
  socialConnections,
  SOCIAL_NAMES,
  SOCIAL_PROVIDERS,
  supportsPasskeys,
  useAuthenticationOptions,
  type Passkey,
  type SocialProvider,
} from "../lib/authentication";
import { formMessageFor } from "../lib/errors";
import { Button } from "../ui/Button";
import { Field } from "../ui/Field";
import { FormDialog } from "../ui/FormDialog";
import { useToast } from "../ui/Toast";
import { Card, CardHeader } from "./page";

type Edit = { kind: "add" } | { kind: "remove"; passkey: Passkey } | { kind: "connect" | "disconnect"; provider: SocialProvider };

function dialogTitle(edit: Edit): string {
  switch (edit.kind) {
    case "add": return "添加 Passkey";
    case "remove": return "移除 Passkey";
    case "connect": return `绑定 ${SOCIAL_NAMES[edit.provider]}`;
    case "disconnect": return `解绑 ${SOCIAL_NAMES[edit.provider]}`;
  }
}

const date = new Intl.DateTimeFormat("zh-CN", { dateStyle: "medium", timeStyle: "short" });

export function AccountAuthentication({ email }: { email: string }) {
  const { options, available, failed, retry } = useAuthenticationOptions();
  const [keys, setKeys] = useState<Passkey[] | null>(null);
  const [connections, setConnections] = useState<Record<SocialProvider, boolean> | null>(null);
  const [listError, setListError] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [edit, setEdit] = useState<Edit | null>(null);
  const [editError, setEditError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  useEffect(() => {
    if (!available) return;
    let live = true;
    const failed = () => { if (live) setListError(true); };
    void passkeys().then((value) => { if (live) setKeys(value); }, failed);
    void socialConnections().then((value) => { if (live) setConnections(value); }, failed);
    return () => { live = false; };
  }, [available, refresh]);

  function show(next: Edit) {
    setEditError(null);
    setEdit(next);
  }

  function reload() {
    setListError(false);
    setRefresh((value) => value + 1);
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!edit || busy) return;
    const data = new FormData(event.currentTarget);
    const password = String(data.get("password") ?? "");
    setEditError(null);
    setBusy(true);
    try {
      switch (edit.kind) {
        case "add":
          await registerPasskey(String(data.get("name") ?? "").trim(), password);
          toast("Passkey 已添加");
          break;
        case "remove":
          await removePasskey(edit.passkey.id, password);
          setKeys((current) => current?.filter((value) => value.id !== edit.passkey.id) ?? null);
          toast("Passkey 已移除");
          break;
        case "connect":
          await openSocialAuthorization(edit.provider, { mode: "link", password });
          return;
        case "disconnect":
          await disconnectSocial(edit.provider, password);
          setConnections((current) => current ? { ...current, [edit.provider]: false } : null);
          toast(`${SOCIAL_NAMES[edit.provider]} 已解绑`);
          break;
      }
      setEdit(null);
      reload();
    } catch (error) {
      setEditError(error instanceof ApiError
        ? error.code === "invalid_credentials" ? "当前口令不正确" : formMessageFor(error)
        : browserAuthenticationError(error));
    } finally {
      setBusy(false);
    }
  }

  const showPasskeys = options?.passkey || (keys?.length ?? 0) > 0;
  const socialProviders = SOCIAL_PROVIDERS.filter((provider) => options?.[provider] || connections?.[provider]);
  if (!showPasskeys && socialProviders.length === 0 && !failed && !listError) return null;

  const supported = supportsPasskeys();
  const confirm = edit?.kind === "remove" || edit?.kind === "disconnect";
  return (
    <Card className="account-authentication" aria-labelledby="account-authentication-title">
      <CardHeader id="account-authentication-title" title="登录方式" description="添加备用登录方式，绑定和移除时需要验证当前口令。" />
      {failed && <div className="note crit" role="alert">无法读取登录设置。 <Button size="sm" onClick={retry}>重试</Button></div>}
      {listError && <div className="note crit" role="alert">无法读取已绑定的登录方式。 <Button size="sm" onClick={reload}>重试</Button></div>}
      {showPasskeys && <section className="account-auth-group" aria-labelledby="account-passkey-title">
        <div className="account-auth-heading">
          <div>
            <h3 id="account-passkey-title"><Fingerprint aria-hidden /> Passkey</h3>
            <p className="hint">{options?.passkey ? "可用指纹、人脸、设备 PIN 或安全密钥登录。" : "平台已暂停 Passkey 登录。你仍可移除已添加的通行密钥。"}</p>
          </div>
          {options?.passkey && <Button disabled={!supported || keys === null} onClick={() => show({ kind: "add" })}>添加 Passkey</Button>}
        </div>
        {options?.passkey && !supported && <p className="hint">当前浏览器或连接不支持添加 Passkey。</p>}
        {keys === null ? listError ? null : <p role="status" className="hint">正在读取 Passkey…</p>
          : keys.length === 0 ? <p className="hint">尚未添加 Passkey。</p>
          : <ul className="account-auth-list">
            {keys.map((key) => <li key={key.id}>
              <div>
                <p className="account-auth-label">{key.name}</p>
                <p className="hint">添加于 {date.format(new Date(key.created_at))} · {key.last_used_at ? `最近使用于 ${date.format(new Date(key.last_used_at))}` : "尚未使用"}</p>
              </div>
              <Button kind="quiet" onClick={() => show({ kind: "remove", passkey: key })} aria-label={`移除 Passkey ${key.name}`} title="移除 Passkey">
                <Trash2 aria-hidden />
              </Button>
            </li>)}
          </ul>}
      </section>}
      {socialProviders.length > 0 && <section className="account-auth-group" aria-labelledby="account-social-title">
        <div>
          <h3 id="account-social-title"><Link2 aria-hidden /> 第三方账号</h3>
          <p className="hint">绑定后即可用该账号登录当前组织。</p>
        </div>
        {connections === null ? listError ? null : <p role="status" className="hint">正在读取绑定状态…</p> :
          <ul className="account-auth-list">
            {socialProviders.map((provider) => <li key={provider}>
              <div>
                <p className="account-auth-label">{SOCIAL_NAMES[provider]}</p>
                <p className="hint">{connections[provider] ? "已绑定" : "未绑定"}{!options?.[provider] ? " · 平台已暂停此登录方式" : ""}</p>
              </div>
              <Button onClick={() => show({ kind: connections[provider] ? "disconnect" : "connect", provider })}>
                {connections[provider] ? "解绑" : "绑定"} {SOCIAL_NAMES[provider]}
              </Button>
            </li>)}
          </ul>}
      </section>}
      {edit && <FormDialog open onOpenChange={(open) => { if (!open) setEdit(null); }} title={dialogTitle(edit)}
        description={edit.kind === "remove" ? `移除后，「${edit.passkey.name}」将无法用于登录。` : "输入当前账号的口令继续。"} busy={busy}>
        <form method="post" className="tenant-form" onSubmit={(event) => void submit(event)}>
          <input type="hidden" name="username" value={email} autoComplete="username" />
          {edit.kind === "add" && <Field label="Passkey 名称">
            <input name="name" required maxLength={64} placeholder="例如 工作笔记本" />
          </Field>}
          <Field label="当前口令">
            <input name="password" type="password" autoComplete="current-password" required />
          </Field>
          {editError && <div className="note crit" role="alert">{editError}</div>}
          <div className="account-form-actions">
            <Button onClick={() => setEdit(null)} disabled={busy}>取消</Button>
            <Button kind={confirm ? "danger" : "primary"} type="submit" busy={busy}>
              {edit.kind === "add" ? "创建 Passkey" : edit.kind === "remove" ? "确认移除" : edit.kind === "connect" ? "继续授权" : "确认解绑"}
            </Button>
          </div>
        </form>
      </FormDialog>}
    </Card>
  );
}
