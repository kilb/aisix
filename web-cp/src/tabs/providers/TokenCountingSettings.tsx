import { useMemo, useState } from "react";
import { Plus, RefreshCw, Trash2 } from "lucide-react";
import { ApiError, type Detail, type ResourceView, type TokenCounting, type TokenCountTarget } from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { tokenCountBody, tokenCountProblems, tokenCountSources, selectTokenCountSource, tokenCountProviderServes, TOKEN_COUNT_TARGETS, type TokenCountAccountDoc, type TokenCountChanges, type TokenCountKey, type TokenCountSourceDoc, type TokenCountSource } from "../../lib/tokenCounting";
import { useResource } from "../../lib/useResource";
import { ConflictNote } from "../../components/ConflictNote";
import { GatewayTooOld } from "../../components/GatewayTooOld";
import { RolloutBadge } from "../../components/RolloutBadge";
import { Card, CardHeader } from "../../components/page";
import { Loading } from "../../components/OperationalTable";
import { Button } from "../../ui/Button";
import { Field, useRefusalFocus } from "../../ui/Field";
import { Select } from "../../ui/Select";

type Account = ResourceView<TokenCountAccountDoc>;
const LABELS: Record<TokenCountKey, string> = {
  messages_count_tokens: "Messages · count_tokens",
  responses_input_tokens: "Responses · input_tokens",
};

export function TokenCountingSettings({ accounts, accountsError, onAccountsReload, canEdit }: { accounts: Account[] | null; accountsError: ApiError | null; onAccountsReload: () => void; canEdit: boolean }) {
  const [saved, setSaved] = useState(false);
  const config = useResource<TokenCounting>("token_counting");
  const models = useResource<TokenCountSourceDoc>("models");
  const current = config.items?.[0] ?? null;
  const reload = () => { setSaved(false); void config.reload(); void models.reload(); onAccountsReload(); };
  return <Card aria-label="Token计数">
    <CardHeader title="Token计数" description="环境级固定计数账号，仅用于两条免费 Token 计数接口。未启用时沿用原上游。" actions={<>
      {current && <RolloutBadge rollout={current.rollout} />}
      <Button type="button" size="sm" aria-label="重新载入Token计数" onClick={reload}><RefreshCw aria-hidden />重新载入</Button>
    </>} />
    <p className="hint mb-4">计数账号从已有上游账号中选择，无需加入租户的推理分组。此设置不会授予模型权限，也不配置额度或价格。映射绑定实际源直连模型；公共别名请选其背后的账号模型。<span>引用已有文件、会话或加密上下文的计数请求仍沿用原上游。</span></p>
    {saved && <p className="note ok" role="status">Token计数设置已保存。</p>}
    {accountsError && <div className="note crit" role="alert">读取计数账号失败：{messageFor(accountsError)}</div>}
    {config.error && <div className="note crit" role="alert">读取Token计数失败：{messageFor(config.error)}</div>}
    {models.error && <div className="note crit" role="alert">读取源模型失败：{messageFor(models.error)}</div>}
    {config.items === null || models.items === null || accounts === null ? (
      !config.error && !models.error && !accountsError && <Loading />
    ) : <TokenCountingForm key={`${current?.id ?? "new"}:${current?.version ?? 0}`} current={current} accounts={accounts} models={models.items} canEdit={canEdit} create={config.create} save={config.save} onReload={reload} onChange={() => setSaved(false)} onSaved={(view) => { setSaved(true); void config.watch(view.id).catch(() => undefined); }} />}
  </Card>;
}

interface FormProps {
  current: ResourceView<TokenCounting> | null;
  accounts: Account[];
  models: TokenCountSource[];
  canEdit: boolean;
  create: (doc: TokenCounting) => Promise<ResourceView<TokenCounting>>;
  save: (id: string, doc: TokenCounting, version: number) => Promise<ResourceView<TokenCounting>>;
  onReload: () => void;
  onChange: () => void;
  onSaved: (view: ResourceView<TokenCounting>) => void;
}

function TokenCountingForm({ current, accounts, models, canEdit, create, save, onReload, onChange, onSaved }: FormProps) {
  const [changes, setChanges] = useState<TokenCountChanges>({});
  const [details, setDetails] = useState<Detail[]>([]);
  const [failure, setFailure] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  const [formRef, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const choices = useMemo(() => tokenCountSources(models), [models]);
  const accountNames = new Map(accounts.map((account) => [account.id, account.doc.display_name || account.id]));
  const body = tokenCountBody(current?.doc ?? {}, changes);
  const editable = canEdit && current?.managed_by == null;
  const at = (path: string) => problemAt(details, (p) => p === path);

  function change(key: TokenCountKey, target: TokenCountTarget | null) {
    setChanges((before) => ({ ...before, [key]: target }));
    setSaved(false);
    onChange();
  }

  async function submit() {
    setFailure(null);
    setSaved(false);
    onChange();
    const problems = tokenCountProblems(body, models);
    setDetails(problems);
    if (problems.length > 0) { focusRefusal(); return; }
    // 空设置不创建有效计数目标，也无需创建空单例。
    if (current === null && TOKEN_COUNT_TARGETS.every((key) => body[key] == null)) {
      setChanges({});
      setSaved(true);
      return;
    }
    setBusy(true);
    try {
      const view = current ? await save(current.id, body, current.version) : await create(body);
      setChanges({});
      setSaved(true);
      onSaved(view);
    } catch (error) {
      const apiError = error instanceof ApiError ? error : new ApiError(error instanceof Error ? error.message : String(error), 0, "client", [], [], null);
      setFailure(apiError);
      setDetails(apiError.details);
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  return <form ref={formRef} onSubmit={(event) => { event.preventDefault(); void submit(); }}>
    <div className="grid gap-5">
      {TOKEN_COUNT_TARGETS.map((key) => {
        const target = body[key] ?? null;
        const accountMissing = target && !accountNames.has(target.provider_key_id);
        const selectedAccount = accounts.find((account) => account.id === target?.provider_key_id);
        const accountInvalid = selectedAccount && !tokenCountProviderServes(selectedAccount.doc, key);
        return <fieldset key={key} disabled={!editable || busy} className="min-w-0 rounded-field border border-hairline p-4">
          <legend className="px-2 text-body font-semibold">{LABELS[key]}</legend>
          <Field label="计数账号" hint={`仅用于 /v1/${key === "messages_count_tokens" ? "messages/count_tokens" : "responses/input_tokens"}`} error={at(`/${key}/provider_key_id`) ?? (accountMissing ? "账号已不存在，请重新选择或关闭此计数接口" : accountInvalid ? "当前账号不支持此计数协议，请重新选择或关闭此计数接口" : undefined)}>
            {(aria) => <Select {...aria} aria-label="计数账号" value={target?.provider_key_id ?? ""} onValueChange={(id) => change(key, id === "" ? null : { ...(target ?? { model_mappings: [] }), provider_key_id: id })}>
              <option value="">未启用（沿用原上游）</option>
              {accountMissing && <option value={target.provider_key_id}>账号已不存在 · {target.provider_key_id}</option>}
              {accounts.filter((account) => account.id === target?.provider_key_id || tokenCountProviderServes(account.doc, key)).map((account) => <option key={account.id} value={account.id} disabled={!tokenCountProviderServes(account.doc, key)}>{account.doc.display_name || account.id} · {account.doc.adapter || account.doc.provider || "未声明协议族"} ({account.id}){!tokenCountProviderServes(account.doc, key) ? " · 不支持此计数协议" : ""}</option>)}
            </Select>}
          </Field>
          {target && <div className="mt-4 grid gap-4">
            <p className="hint">可选：将实际源模型版本映射为计数账号的上游模型。按 Model ID 区分同名模型，不支持通配符。</p>
            {(target.model_mappings ?? []).map((mapping, index) => {
              const prefix = `/${key}/model_mappings/${index}`;
              const source = models.find((model) => model.id === mapping.model_id);
              const available = choices.some((model) => model.id === mapping.model_id);
              const used = new Set((target.model_mappings ?? []).filter((_, other) => other !== index).map((row) => row.model_id));
              const replace = (next: typeof mapping) => {
                change(key, { ...target, model_mappings: (target.model_mappings ?? []).map((row, i) => i === index ? next : row) });
              };
              return <div key={index} className="grid min-w-0 gap-3 rounded-field border border-hairline p-3">
                <Field label="源模型" error={at(`${prefix}/model_id`)}>
                  {(aria) => <Select {...aria} aria-label="源模型" value={mapping.model_id} onValueChange={(id) => {
                    const selected = choices.find((model) => model.id === id);
                    if (selected) replace(selectTokenCountSource(mapping, selected));
                  }}>
                    <option value="">请选择实际源模型</option>
                    {mapping.model_id && !available && <option value={mapping.model_id}>源模型不可用 · {mapping.model_id}</option>}
                    {choices.map((model) => <option key={model.id} value={model.id} disabled={used.has(model.id)}>{model.doc.display_name || model.id} · {accountNames.get(model.doc.provider_key_id ?? "") ?? model.doc.provider_key_id ?? "账号不存在"} · {model.doc.model_name} ({model.id})</option>)}
                  </Select>}
                </Field>
                {source && source.doc.model_name !== mapping.source_model_name && editable && <div><Button type="button" size="sm" onClick={() => replace(selectTokenCountSource(mapping, source))}>确认当前源版本</Button></div>}
                <p className="hint break-all">源账号：{source ? accountNames.get(source.doc.provider_key_id ?? "") ?? source.doc.provider_key_id ?? "—" : "—"} · Model ID：{mapping.model_id || "—"}</p>
                <div className="grid gap-3 md:grid-cols-2">
                  <Field label="源上游模型（只读）" error={at(`${prefix}/source_model_name`)} hint={source && source.doc.model_name !== mapping.source_model_name ? `当前源模型已变化为 ${source.doc.model_name}，请确认当前源版本或重新选择。` : "选择源模型时自动记录；保存时校验源版本未变化。"}>
                    {(aria) => <input {...aria} aria-label="源上游模型（只读）" readOnly value={mapping.source_model_name} />}
                  </Field>
                  <Field label="计数上游模型" error={at(`${prefix}/model_name`)}>
                    {(aria) => <input {...aria} aria-label="计数上游模型" required value={mapping.model_name} onChange={(event) => replace({ ...mapping, model_name: event.target.value })} />}
                  </Field>
                </div>
                {editable && <div className="flex justify-end"><Button type="button" size="sm" aria-label="移除映射" onClick={() => change(key, { ...target, model_mappings: (target.model_mappings ?? []).filter((_, i) => i !== index) })}><Trash2 aria-hidden />移除映射</Button></div>}
              </div>;
            })}
            {editable && <div><Button type="button" size="sm" onClick={() => change(key, { ...target, model_mappings: [...(target.model_mappings ?? []), { model_id: "", source_model_name: "", model_name: "" }] })}><Plus aria-hidden />添加模型映射</Button></div>}
          </div>}
        </fieldset>;
      })}
    </div>
    {failure?.code === "version_mismatch" ? <ConflictNote onReload={onReload} /> : failure?.code === "gateway_too_old" ? <GatewayTooOld error={failure} allowRetirement /> : failure && <div className="note crit" role="alert">无法保存Token计数：{messageFor(failure)}{failure.status ? `（HTTP ${failure.status}）` : ""}</div>}
    {details.length > 0 && <div className="note crit" role="alert"><ul>{details.map((detail, i) => <li key={i}><code>{detail.path}</code>：{detail.message}</li>)}</ul></div>}
    {saved && <p className="note ok" role="status">Token计数设置已保存。</p>}
    {current?.managed_by != null && <p className="hint">此配置由 {current.managed_by} 管理，请在对应来源修改。</p>}
    {editable && <div className="mt-4 flex justify-end"><Button kind="primary" type="submit" busy={busy} busyLabel="正在保存Token计数" disabled={Object.keys(changes).length === 0}>保存Token计数</Button></div>}
  </form>;
}
