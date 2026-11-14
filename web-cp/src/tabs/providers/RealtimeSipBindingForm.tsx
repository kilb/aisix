import { SecretInput } from "../../components/SecretInput";
import type { RealtimeSipDraft, RealtimeSipField } from "../../lib/realtimeSipBinding";
import { realtimeSipWebhookPath } from "../../lib/realtimeSipBinding";
import { Field } from "../../ui/Field";

export function RealtimeSipBindingForm({
  draft,
  storedSecret,
  disabled,
  errorAt,
  sectionError,
  onChange,
  storedProject,
  projectError,
  onProjectChange,
}: {
  draft: RealtimeSipDraft;
  storedSecret: boolean;
  disabled: boolean;
  errorAt: (field: RealtimeSipField) => string | null;
  sectionError: string | null;
  onChange: (draft: RealtimeSipDraft) => void;
  storedProject: boolean;
  projectError: string | null;
  onProjectChange: (value: string | undefined) => void;
}) {
  const path = realtimeSipWebhookPath(draft.routeId);
  const locked = disabled || !draft.enabled;
  return (
    <fieldset className="tn-section mt-2 border-t border-line pt-5">
      <legend className="text-body font-semibold">Realtime SIP 来电</legend>
      <label className="flex items-center gap-2 text-caption">
        <input type="checkbox" checked={draft.enabled} disabled={disabled} onChange={(event) => onChange({ ...draft, enabled: event.target.checked })} />
        启用 SIP 来电绑定
      </label>
      <p className="hint text-caption">该上游项目的全部 SIP 来电归此绑定租户。SIP 请求头与 To 号码不用于识别租户。</p>
      <div hidden={!draft.enabled}>
        <div className="grid gap-x-4 @sm:grid-cols-2">
          <Field label="SIP 路由标识" hint="1–64 个字母、数字、下划线或连字符。" error={errorAt("route_id")}>
            {(aria) => <input required disabled={locked} value={draft.routeId} onChange={(event) => onChange({ ...draft, routeId: event.target.value })} {...aria} />}
          </Field>
          <Field label="租户 API 密钥 ID" hint="使用此密钥的当前租户、成员、模型和分组权限。" error={errorAt("api_key_id")}>
            {(aria) => <input required disabled={locked} value={draft.apiKeyId} onChange={(event) => onChange({ ...draft, apiKeyId: event.target.value })} {...aria} />}
          </Field>
          <Field label="直连模型 ID" hint="填写此上游账号的直连模型 ID。" error={errorAt("model_id")}>
            {(aria) => <input required disabled={locked} value={draft.modelId} onChange={(event) => onChange({ ...draft, modelId: event.target.value })} {...aria} />}
          </Field>
          <SecretInput
            label="SIP 上游项目 ID"
            stored={storedProject}
            required
            readOnly={locked}
            hint="项目 ID 按账号请求头保存，已保存值不回显。替换项目时，也须重新填写 Webhook 签名密钥。"
            problem={projectError}
            onChange={onProjectChange}
          />
          <SecretInput
            label="Webhook 签名密钥"
            stored={storedSecret}
            required
            readOnly={locked}
            hint="已保存的密钥不会显示。更改路由、API 密钥、模型或上游账号配置时，须重新填写。"
            problem={errorAt("webhook_secret")}
            onChange={(webhookSecret) => onChange({ ...draft, webhookSecret })}
          />
        </div>
        {path && <p className="hint text-caption">将网关公网地址加上 <code>{path}</code> 配置为上游项目的 Webhook 地址，并订阅 <code>realtime.call.incoming</code>。</p>}
      </div>
      {sectionError && <p className="field-problem" role="alert">{sectionError}</p>}
    </fieldset>
  );
}
