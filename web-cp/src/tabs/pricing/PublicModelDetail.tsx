import { useState } from "react";
import { ApiError, channelModels, type PublicModel, type ServedBy } from "../../lib/api";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtUtc } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { GroupList } from "../../ui/GroupName";
import { useToast } from "../../ui/Toast";
import "./PublicModels.css";

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

  return (
    <div className="panel" role="region" aria-label={`公共模型 ${model.name}`}>
      <h2>
        <span className="mono">{model.name}</span>
        <Button size="sm" type="button" onClick={onBack}>
          返回列表
        </Button>
      </h2>
      <p className="hint">
        标识 <code>{model.id}</code>，创建于 {fmtUtc(model.created_at)}。授权租户 {model.tenants} 个、密钥{" "}
        {model.keys} 个。
      </p>
      {error && (
        <div className="note crit" role="alert">
          重新生成失败：{error}
        </div>
      )}
      {note && (
        <div className="note" role="status">
          {note}
        </div>
      )}
      {canEdit && (
        <p>
          <Button type="button" busy={busy} busyLabel="重新生成中…" onClick={() => void regenerate()}>
            重新生成
          </Button>{" "}
          <span className="hint">按当前表格重写生成的模型文档，不改变表格本身。</span>
        </p>
      )}
      {served.length === 0 ? (
        <div className="note warn" role="status">
          <strong>无可用账号</strong>：所有服务该模型的账号都已停用或取消接入。模型仍保留在列表中，授权不受影响，但请求会以
          503 失败（无可用上游）。
        </div>
      ) : (
        <DataTable label="服务账号" compact>
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
                <tr key={account.provider_key_id} data-tier={tier.priority} data-enabled={account.enabled}>
                  <td className="num">{at === 0 ? index + 1 : ""}</td>
                  <td>{account.display_name || account.provider_key_id}</td>
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
                  <td>{account.enabled ? "启用" : "已停用"}</td>
                </tr>
              )),
            )}
          </tbody>
        </DataTable>
      )}
      {model.served_by.length > 0 && served.length === 0 && (
        <DataTable label="服务账号（全部停用）" compact>
          <thead>
            <tr>
              <th>上游账号</th>
              <th className="num">优先级</th>
              <th className="num">权重</th>
              <th>状态</th>
            </tr>
          </thead>
          <tbody>
            {model.served_by.map((account) => (
              <tr key={account.provider_key_id} data-enabled="false">
                <td>{account.display_name || account.provider_key_id}</td>
                <td className="num tabular">{account.priority}</td>
                <td className="num tabular">{account.weight}</td>
                <td>已停用</td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </div>
  );
}
