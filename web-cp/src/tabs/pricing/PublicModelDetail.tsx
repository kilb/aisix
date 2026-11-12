import { useState } from "react";
import { ArrowLeft } from "lucide-react";
import { ApiError, channelModels, type PublicModel, type ServedBy } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { GroupList } from "../../ui/GroupName";
import { useToast } from "../../ui/Toast";
import { BentoGrid, Card, CardHeader, Metric, PageHeader, span } from "../../components/page";
import { Lamp } from "../../ui/Lamp";
import { LibrarySyncOne } from "./LibrarySync";
import { CardFooter, Count, WithForms } from "./kit";

/**
 * The accounts serving a model, as the routing document orders them: one
 * tier per priority, highest first, and within a tier by weight, heaviest
 * first. A disabled account is listed where it sits and marked, because the
 * operator needs to see what a re-enable would bring back.
 */
function tiers(accounts: ServedBy[]): { priority: number; accounts: ServedBy[] }[] {
  const ordered = [...accounts].sort(
    (a, b) => b.priority - a.priority || b.weight - a.weight || a.display_name.localeCompare(b.display_name),
  );
  const out: { priority: number; accounts: ServedBy[] }[] = [];
  for (const account of ordered) {
    const last = out[out.length - 1];
    if (last && last.priority === account.priority) last.accounts.push(account);
    else out.push({ priority: account.priority, accounts: [account] });
  }
  return out;
}

/**
 * One public model: the accounts that serve it, tier by tier, and the
 * documents' own state.
 *
 * With no enabled account the model is still listed and still granted, but a
 * request for it answers `503` (the routing document has no target), so the
 * page says that in place of a list rather than showing an empty table as if
 * the model had been deleted.
 *
 * **重新生成** re-applies every account's channel row unchanged, which is the
 * control plane's own regenerate path: it rewrites the documents from the
 * table, so a document that diverged (an account renamed after it was
 * ticked, say) is brought back in step without the table moving.
 */
export function PublicModelDetail({
  model,
  canEdit,
  onBack,
  onChanged,
}: {
  model: PublicModel;
  canEdit: boolean;
  onBack: () => void;
  onChanged: () => Promise<void>;
}) {
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  const served = model.served_by.filter((s) => s.enabled);
  const ordered = tiers(model.served_by);
  const enabledPriorities = [...new Set(served.map((account) => account.priority))].sort((a, b) => b - a);
  const disabledCount = model.served_by.length - served.length;

  async function regenerate() {
    setBusy(true);
    setError(null);
    setNote(null);
    // Every served row, re-applied with its own values: the write path
    // regenerates the documents from the table in the same transaction.
    const rows = model.served_by;
    try {
      for (const row of rows) {
        await channelModels.put(row.provider_key_id, model.id, {
          upstream_model: row.upstream_model,
          priority: row.priority,
          weight: row.weight,
          enabled: row.enabled,
        });
      }
      setNote(
        rows.length === 0
          ? "该公共模型还没有接入任何账号，没有文档可重写。"
          : `已按当前表格重新应用 ${rows.length} 个账号的接入行，模型文档与表格一致。`,
      );
      toast("已重新生成");
      await onChanged();
    } catch (e) {
      setError(e instanceof ApiError ? messageFor(e) : errorText(e));
    } finally {
      setBusy(false);
    }
  }

  const aside =
    canEdit || model.served_by.length > 0 ? (
      <>
        {canEdit && (
          <Card className="mb-0">
            <CardHeader number={4} title="重新生成" description="按当前表格重写生成的模型文档，不改变表格本身。" />
            {error && (
              <div className="note crit mt-0" role="alert">
                重新生成失败：{error}
              </div>
            )}
            {note && (
              <div className="note mt-0" role="status">
                {note}
              </div>
            )}
            <CardFooter className="mt-2">
              <Button type="button" busy={busy} busyLabel="重新生成中…" onClick={() => void regenerate()}>
                重新生成
              </Button>
            </CardFooter>
          </Card>
        )}
        {/* 账号接入后生成的路由文档才可绑定基准价。 */}
        {model.served_by.length > 0 && (
          <Card className="mb-0">
            <CardHeader number={canEdit ? 5 : 4} title="基准价联动" description="从价格库核对并同步当前模型的完整价格。" />
            <LibrarySyncOne modelId={model.id} canEdit={canEdit} current={null} onSynced={onChanged} />
          </Card>
        )}
      </>
    ) : undefined;

  return (
    <div className="public-model-detail" role="region" aria-label={`公共模型 ${model.name}`}>
      <PageHeader
        eyebrow="公共模型 / 账号与定价"
        title={<span className="pm-model-title">{model.name}</span>}
        dataLead={served.length > 0 ? <>由 {served.length.toLocaleString("zh-CN")} 个启用账号服务，分布在 {enabledPriorities.length.toLocaleString("zh-CN")} 个优先级层。优先级从高到低、同层按权重排序。</> : "当前没有启用的账号映射，请先到上游账号重新接入。"}
        actions={
          <Button type="button" onClick={onBack}>
            <ArrowLeft aria-hidden />
            返回列表
          </Button>
        }
      />
      <p className="mb-6 text-body text-ink-soft">
        标识 <code className="break-all">{model.id}</code>，创建于 {fmtUtc(model.created_at)}。授权租户 {model.reachable_tenants} 个、密钥 {model.reachable_keys} 个。
      </p>
      <BentoGrid className="mb-3" aria-label="公共模型账号概览">
        <Card className={span.d7}>
          <CardHeader number={1} title="启用服务账号" description="当前模型 · 仅统计启用的接入映射" />
          <Metric size="hero">{served.length.toLocaleString("zh-CN")}</Metric>
          <dl className="pm-routing-summary mt-6">
            <div><dt>启用优先级层</dt><dd>{enabledPriorities.length.toLocaleString("zh-CN")}</dd></div>
            <div><dt>最高启用优先级</dt><dd>{enabledPriorities[0] ?? "—"}</dd></div>
            <div><dt>全部账号映射</dt><dd>{model.served_by.length.toLocaleString("zh-CN")}</dd></div>
          </dl>
          <CardFooter className="justify-start text-caption text-muted">账号和优先级以完整映射表为准；未启用的映射保留在表中。</CardFooter>
        </Card>
        <Card tone="inverse" className={span.d5Wide}>
          <CardHeader number={2} title="已停用账号映射" description="当前模型 · 恢复前核对上游模型与分组" />
          <Metric size="display">{disabledCount.toLocaleString("zh-CN")}</Metric>
          <p className="mt-4 text-body text-on-inverse-muted">公共模型的稳定标识承接授权与基准价。重新生成只修复模型文档，不会修改优先级、权重或启停状态。</p>
          <CardFooter className="justify-start border-surface/12 text-caption text-on-inverse-muted">{model.retired_at ? "模型已停用，恢复后仍需重新接入账号。" : `模型在用 · 授权租户 ${model.reachable_tenants} 个 · 密钥 ${model.reachable_keys} 个`}</CardFooter>
        </Card>
      </BentoGrid>
      <WithForms forms={aside}>
        <Card className="mb-0">
          <CardHeader
            number={3}
            title={<>服务账号<Count n={served.length} /></>}
            description={`完整映射 ${model.served_by.length} 个，启用 ${served.length} 个。优先级高者在前，同层权重高者在前。`}
          />
          {served.length === 0 ? (
            <div className="note warn mb-0 mt-0" role="status">
              <strong>无可用账号</strong>：所有服务该模型的账号都已停用或取消接入。模型仍保留在列表中，授权不受影响，但请求会以
              503 失败（无可用上游）。
            </div>
          ) : null}
          {model.served_by.length > 0 && (
            <DataTable label={served.length === 0 ? "服务账号（全部停用）" : "服务账号"} compact scroll>
              <thead>
                <tr>
                  <th className="num">层</th>
                  <th>上游账号</th>
                  <th className="num">优先级</th>
                  <th className="num">权重</th>
                  <th>上游模型名称</th>
                  <th>账号分组</th>
                  <th>状态</th>
                </tr>
              </thead>
              <tbody>
                {ordered.map((tier, index) =>
                  tier.accounts.map((account, at) => (
                    <tr
                      key={account.provider_key_id}
                      data-tier={tier.priority}
                      data-enabled={account.enabled}
                      className={at === 0 && index > 0 ? "[&>td]:border-t [&>td]:border-t-border" : undefined}
                    >
                      <td className="num font-medium">{at === 0 ? index + 1 : ""}</td>
                      <td className="font-medium">{account.display_name || account.provider_key_id}</td>
                      <td className="num tabular">{account.priority}</td>
                      <td className="num tabular">{account.weight}</td>
                      <td>
                        {account.upstream_model === null ? (
                          <span className="cell-note">同名（{model.name}）</span>
                        ) : (
                          <span className="mono">{account.upstream_model}</span>
                        )}
                      </td>
                      <td>{account.groups.length > 0 ? <GroupList groups={account.groups} /> : "—"}</td>
                      <td>
                        <Lamp state={account.enabled ? "applied" : "disabled"}>{account.enabled ? "启用" : "已停用"}</Lamp>
                      </td>
                    </tr>
                  )),
                )}
              </tbody>
            </DataTable>
          )}
        </Card>
      </WithForms>
    </div>
  );
}
