import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Cloud, Fingerprint, KeyRound, Mail, ShieldCheck } from "lucide-react";
import { Card, CardHeader, CardMessage, PageHeader } from "../components/page";
import { ApiError, type Detail } from "../lib/api";
import { authenticationDraft, readAuthenticationSettings, writeAuthenticationSettings, type AuthenticationDraft, type AuthenticationSettings, type OAuthProvider } from "../lib/authentication";
import { problemAt } from "../lib/errors";
import { errorText } from "../lib/fmt";
import { Button } from "../ui/Button";
import { CopyButton } from "../ui/CopyButton";
import { Field, useRefusalFocus } from "../ui/Field";
import "./Authentication.css";

const GITHUB_DOC = "https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/creating-an-oauth-app";
const GOOGLE_DOC = "https://developers.google.com/identity/protocols/oauth2/web-server";
const TURNSTILE_DOC = "https://developers.cloudflare.com/turnstile/get-started/widget-management/dashboard/";
const TURNSTILE_CSP_DOC = "https://developers.cloudflare.com/turnstile/reference/content-security-policy/";
const PROVIDER_NAMES = { github: "GitHub", google: "Google" } as const;

function GuideLink({ href, children }: { href: string; children: ReactNode }) {
  return <a href={href} target="_blank" rel="noreferrer">{children}</a>;
}

function Guide({ children }: { children: ReactNode }) {
  return <details className="auth-guide">
    <summary>配置指引</summary>
    {children}
  </details>;
}

function MethodSwitch({ label, checked, onChange, error }: {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
  error?: string | null;
}) {
  if (error) {
    return <Field label={label} error={error}>
      {aria => <input {...aria} type="checkbox" checked={checked} onChange={e => onChange(e.target.checked)} />}
    </Field>;
  }
  return <label className="check auth-toggle">
    <input type="checkbox" checked={checked} onChange={e => onChange(e.target.checked)} />
    {label}
  </label>;
}

function SecretField({ label, configured, value, onChange, enabled, error, targetChanged }: {
  label: string;
  configured: boolean;
  value: string | undefined;
  onChange: (value: string | undefined) => void;
  enabled: boolean;
  error: string | null;
  targetChanged: boolean;
}) {
  const clearing = !enabled && value === "";
  let hint = "密钥仅保存在服务端。";
  if (clearing) hint = "保存后会清除当前密钥。";
  else if (configured && targetChanged) hint = "配置目标已更改，请重新填写密钥，或关闭功能后清除原密钥。";
  else if (configured) hint = "已保存密钥。留空沿用，填写新值可更换。";
  return <div className="min-w-0">
    <Field label={label} hint={hint} error={error}>
      {aria => <input {...aria} type="password" autoComplete="new-password" spellCheck={false} disabled={clearing}
        value={value ?? ""} onChange={e => onChange(e.target.value || undefined)} />}
    </Field>
    {configured && !enabled && <label className="check auth-secret-clear">
      <input type="checkbox" checked={clearing} onChange={e => onChange(e.target.checked ? "" : undefined)} />
      清除已保存的 {label}
    </label>}
  </div>;
}

function previewOrigin(value: string): string {
  try {
    const parsed = new URL(value.trim());
    const secure = parsed.protocol === "https:" || (parsed.protocol === "http:" && parsed.hostname === "localhost");
    return secure && parsed.pathname === "/" && !parsed.search && !parsed.hash && !parsed.username && !parsed.password
      ? parsed.origin : "";
  } catch {
    return "";
  }
}

function OAuthSection({ provider, draft, saved, origin, savedOrigin, change, problem }: {
  provider: OAuthProvider;
  draft: AuthenticationDraft[OAuthProvider];
  saved: AuthenticationSettings[OAuthProvider];
  origin: string;
  savedOrigin: string;
  change: (draft: AuthenticationDraft[OAuthProvider]) => void;
  problem: (path: string) => string | null;
}) {
  const [copyFailed, setCopyFailed] = useState(false);
  const name = PROVIDER_NAMES[provider];
  const title = `${name} 登录`;
  const callback = origin === savedOrigin ? saved.callback_url : origin && `${origin}/api/auth/oauth/${provider}/callback`;
  const base = `/${provider}`;
  return <section className="auth-section" aria-label={title}>
    <CardHeader title={<span className="auth-method-title"><KeyRound aria-hidden="true" />{title}</span>}
      description="用户先登录租户门户并在「账号设置 → 登录方式」中绑定，之后可用该账号登录。"
      actions={<MethodSwitch label={`启用 ${title}`} checked={draft.enabled} error={problem(`${base}/enabled`)}
        onChange={enabled => change({ ...draft, enabled, client_secret: enabled && draft.client_secret === "" ? undefined : draft.client_secret })} />} />
    <div className="auth-fields">
      <Field label={`${name} Client ID`} error={problem(`${base}/client_id`)}>
        {aria => <input {...aria} type="text" autoComplete="off" spellCheck={false} value={draft.client_id}
          onChange={e => change({ ...draft, client_id: e.target.value })} />}
      </Field>
      <SecretField label={`${name} Client Secret`} configured={saved.client_secret_configured} enabled={draft.enabled} targetChanged={draft.client_id.trim() !== saved.client_id}
        value={draft.client_secret} onChange={client_secret => change({ ...draft, client_secret })} error={problem(`${base}/client_secret`)} />
    </div>
    <div className="auth-callback">
      <span className="f-label">{origin && origin !== savedOrigin ? "回调 URL 预览" : "回调 URL"}</span>
      {callback ? <div className="auth-callback-line">
        <code>{callback}</code>
        <CopyButton label="复制回调 URL" text={callback} size="sm" onFailed={() => setCopyFailed(true)} />
      </div> : <p className="hint m-0">填写租户门户地址后生成回调 URL。</p>}
      {origin && origin !== savedOrigin && <p className="hint m-0">保存门户地址后，请以此处显示的最终地址为准。</p>}
      {copyFailed && callback && <p className="hint m-0" role="status">剪贴板不可用，可选中回调 URL 手动复制。</p>}
    </div>
    {provider === "github" ? <Guide>
      <ol>
        <li>打开 GitHub 的 Settings → Developer settings → OAuth Apps，创建 OAuth App。</li>
        <li>Homepage URL 填租户门户地址，Authorization callback URL 填上方的完整回调 URL。</li>
        <li>应用创建后复制 Client ID，生成一个 Client Secret，填入本页后启用并保存。</li>
      </ol>
      <GuideLink href={GITHUB_DOC}>GitHub 官方指引</GuideLink>
    </Guide> : <Guide>
      <ol>
        <li>在 Google Cloud 选择项目，进入 Google Auth Platform，设置应用品牌、同意页与目标用户。</li>
        <li>在 Clients 创建 Web application 类型的 OAuth 客户端，将上方的完整回调 URL 加入 Authorized redirect URIs。</li>
        <li>复制 Client ID 和 Client Secret，填入本页后启用并保存。外部应用处于测试时，在 Audience 按需添加测试用户。</li>
      </ol>
      <GuideLink href={GOOGLE_DOC}>Google 官方指引</GuideLink>
    </Guide>}
  </section>;
}

export function Authentication() {
  const [settings, setSettings] = useState<AuthenticationSettings | null>(null);
  const [draft, setDraft] = useState<AuthenticationDraft | null>(null);
  const [reload, setReload] = useState(0);
  const [failure, setFailure] = useState<string | null>(null);
  const [problems, setProblems] = useState<Detail[]>([]);
  const [busy, setBusy] = useState(false);
  const [stale, setStale] = useState(false);
  const [saved, setSaved] = useState(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  useEffect(() => {
    const controller = new AbortController();
    void readAuthenticationSettings(controller.signal).then(value => {
      setSettings(value);
      setDraft(authenticationDraft(value));
    }).catch(cause => {
      if (!controller.signal.aborted) setFailure(errorText(cause));
    });
    return () => controller.abort();
  }, [reload]);

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!draft || busy || stale) return;
    setBusy(true);
    setFailure(null);
    setProblems([]);
    setSaved(false);
    try {
      const value = await writeAuthenticationSettings(draft);
      setSettings(value);
      setDraft(authenticationDraft(value));
      setSaved(true);
    } catch (cause) {
      setFailure(errorText(cause));
      if (cause instanceof ApiError) {
        setStale(cause.code === "version_mismatch");
        setProblems(cause.details);
        focusRefusal();
      }
    } finally {
      setBusy(false);
    }
  }

  function change<K extends keyof AuthenticationDraft>(name: K, value: AuthenticationDraft[K]) {
    setDraft(current => current && { ...current, [name]: value });
    setSaved(false);
  }

  function reloadSettings() {
    setDraft(null);
    setSettings(null);
    setStale(false);
    setFailure(null);
    setProblems([]);
    setReload(count => count + 1);
  }

  const problem = (path: string) => problemAt(problems, reported => reported === path);
  const origin = draft ? previewOrigin(draft.portal_origin) : "";
  const dirty = draft && settings && JSON.stringify(draft) !== JSON.stringify(authenticationDraft(settings));
  return <div className="cp-auth-settings">
    <PageHeader title="认证设置" description="为租户门户配置注册邮箱验证、人机验证和其他登录方式。" />
    {!settings || !draft ? <Card>
      <CardMessage onRetry={failure ? reloadSettings : undefined}>
        {failure || "载入中…"}
      </CardMessage>
    </Card> : <form ref={form} onSubmit={event => void save(event)} aria-label="认证设置">
      <fieldset disabled={busy} className="min-w-0 border-0 p-0">
        <Card className="auth-sheet">
          <section className="auth-section" aria-label="门户地址">
            <CardHeader title={<span className="auth-method-title"><ShieldCheck aria-hidden="true" />门户地址</span>} />
            <div className="auth-fields">
              <Field label="租户门户地址" hint="填写用户打开租户门户时看到的 HTTPS 地址，包含域名和可选端口，无需填写路径。" error={problem("/portal_origin")}>
                {aria => <input {...aria} type="url" autoComplete="off" spellCheck={false} placeholder="https://portal.example.com"
                  value={draft.portal_origin} onChange={e => change("portal_origin", e.target.value)} />}
              </Field>
            </div>
          </section>
          <section className="auth-section" aria-label="邮件验证码">
            <CardHeader title={<span className="auth-method-title"><Mail aria-hidden="true" />邮件验证码</span>}
              description="启用后，新用户注册时需先收取并输入邮箱验证码。"
              actions={<MethodSwitch label="启用邮件验证码" checked={draft.email.enabled} error={problem("/email/enabled")}
                onChange={enabled => change("email", { ...draft.email, enabled, api_key: enabled && draft.email.api_key === "" ? undefined : draft.email.api_key })} />} />
            <div className="auth-fields">
              <Field label="邮件接口 URL" hint="填写公网 HTTPS 发信地址，省略查询参数与片段。密钥填在发信 API Key 中。" error={problem("/email/endpoint")}>
                {aria => <input {...aria} type="url" placeholder="https://mail.example.com/send" value={draft.email.endpoint}
                  onChange={e => change("email", { ...draft.email, endpoint: e.target.value })} />}
              </Field>
              <Field label="发件邮箱" hint="填写发信接口已允许使用的发件地址。" error={problem("/email/from")}>
                {aria => <input {...aria} type="email" placeholder="verify@example.com" value={draft.email.from}
                  onChange={e => change("email", { ...draft.email, from: e.target.value })} />}
              </Field>
              <SecretField label="发信 API Key" configured={settings.email.api_key_configured} value={draft.email.api_key}
                enabled={draft.email.enabled} error={problem("/email/api_key")} targetChanged={draft.email.endpoint.trim() !== settings.email.endpoint}
                onChange={api_key => change("email", { ...draft.email, api_key })} />
            </div>
            <Guide>
              <p>控制面向所填 URL 发起 POST 请求，使用 <code>Authorization: Bearer &lt;API Key&gt;</code>。接口用 HTTP 2xx 表示已接收邮件。</p>
              <p>请求体为 JSON，包含 <code>from</code>（发件邮箱）、<code>to</code>（一个收件邮箱字符串）、<code>subject</code>（主题）和 <code>text</code>（正文）。接口使用其他字段或认证方式时，可通过自有 HTTP 转接服务转换。</p>
            </Guide>
          </section>
          <section className="auth-section" aria-label="Cloudflare Turnstile">
            <CardHeader title={<span className="auth-method-title"><Cloud aria-hidden="true" />Cloudflare Turnstile</span>}
              description="在租户门户登录、注册和发送验证码时进行人机验证。"
              actions={<MethodSwitch label="启用 Turnstile" checked={draft.turnstile.enabled} error={problem("/turnstile/enabled")}
                onChange={enabled => change("turnstile", { ...draft.turnstile, enabled, secret_key: enabled && draft.turnstile.secret_key === "" ? undefined : draft.turnstile.secret_key })} />} />
            <div className="auth-fields">
              <Field label="Site Key" hint="Turnstile 小组件的公开站点密钥。" error={problem("/turnstile/site_key")}>
                {aria => <input {...aria} type="text" autoComplete="off" spellCheck={false} value={draft.turnstile.site_key}
                  onChange={e => change("turnstile", { ...draft.turnstile, site_key: e.target.value })} />}
              </Field>
              <SecretField label="Secret Key" configured={settings.turnstile.secret_key_configured} value={draft.turnstile.secret_key}
                enabled={draft.turnstile.enabled} error={problem("/turnstile/secret_key")} targetChanged={draft.turnstile.site_key.trim() !== settings.turnstile.site_key}
                onChange={secret_key => change("turnstile", { ...draft.turnstile, secret_key })} />
            </div>
            <Guide>
              <ol>
                <li>在 Cloudflare 控制台进入 Turnstile，创建小组件并选择 Managed 模式。</li>
                <li>在 Hostname Management 添加租户门户的域名，仅填写主机名。</li>
                <li>将这个小组件的 Site Key 和 Secret Key 填入相应字段。Secret Key 供控制面在服务端校验。Cloudflare 测试密钥仅在 localhost 上使用。</li>
                <li>先部署支持 Turnstile 的门户前端和控制面。如果 CDN 或反向代理设置了内容安全策略，在 <code>script-src</code> 和 <code>frame-src</code> 中放行 <code>https://challenges.cloudflare.com</code>，再启用并保存。</li>
              </ol>
              <div className="flex flex-wrap gap-x-4">
                <GuideLink href={TURNSTILE_DOC}>Cloudflare 官方指引</GuideLink>
                <GuideLink href={TURNSTILE_CSP_DOC}>内容安全策略配置</GuideLink>
              </div>
            </Guide>
          </section>
          <section className="auth-section" aria-label="Passkey">
            <CardHeader title={<span className="auth-method-title"><Fingerprint aria-hidden="true" />Passkey</span>}
              description="用户登录后可在「账号设置 → 登录方式」中添加通行密钥，下次使用设备解锁或安全密钥登录。"
              actions={<MethodSwitch label="启用 Passkey" checked={draft.passkey.enabled} error={problem("/passkey/enabled")}
                onChange={enabled => change("passkey", { enabled })} />} />
            <Guide>
              <ol>
                <li>先确定租户门户的 HTTPS 地址。Passkey 关联门户域名，换域名后用户需要在新域名上重新添加。</li>
                <li>启用并保存后，使用支持 Passkey 的浏览器登录门户，在「账号设置 → 登录方式」中添加一个通行密钥。</li>
                <li>退出后使用该通行密钥验证一次。原口令仍可用于登录和重新添加凭据。</li>
              </ol>
            </Guide>
          </section>
          {(["github", "google"] as const).map(provider => <OAuthSection key={provider} provider={provider} draft={draft[provider]}
            saved={settings[provider]} origin={origin} savedOrigin={settings.portal_origin} change={value => change(provider, value)} problem={problem} />)}
        </Card>
      </fieldset>
      {failure && <div className="note crit" role="alert">
        <p>{failure}</p>
        {stale && <Button size="sm" onClick={reloadSettings}>放弃草稿并重新载入</Button>}
      </div>}
      <div className="auth-save-bar">
        <p className="hint m-0" role="status">{saved ? "认证设置已保存" : dirty ? "有未保存的更改" : ""}</p>
        <Button type="submit" kind="primary" busy={busy} busyLabel="保存中…" disabled={!dirty || stale}>保存认证设置</Button>
      </div>
    </form>}
  </div>;
}
