import { useState, type ReactNode } from "react";
import { ArrowLeft } from "lucide-react";
import { ApiError, channelModels, type PublicModel, type ServedBy, type ModelGroupPriceRow } from "../../lib/api";
import { runPublicModelWrite, type PublicModelWriteGate } from "../../lib/publicModelWriteGate";
import { messageFor } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtBeijing } from "../../lib/time";
import { Button } from "../../ui/Button";
import { DataTable } from "../../ui/DataTable";
import { GroupList } from "../../ui/GroupName";
import { useToast } from "../../ui/Toast";
import { Card, CardHeader, PageHeader } from "../../components/page";
import { Lamp } from "../../ui/Lamp";
import { RoutePolicy } from "../../ui/RoutePolicy";
import { PublicModelBaseline } from "./Baselines";
import { RouteNav } from "../../ui/RouteNav";
import { Count } from "./kit";

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
  routingEditor,
  baselinePrices,
  writeGate,
  loadError,
}: {
  writeGate: PublicModelWriteGate;
  loadError?: string | null;
  routingEditor?: ReactNode;
  baselinePrices: ModelGroupPriceRow | null;
  model: PublicModel;
  canEdit: boolean;
  onBack: () => void;
  onChanged: () => Promise<void>;
}) {
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [view, setView] = useState<"edit" | "routes" | "baseline">(routingEditor ? "edit" : "routes");
  const [baselineVisited, setBaselineVisited] = useState(false);

  const served = model.served_by.filter((s) => s.enabled);
  const ordered = tiers(model.served_by);
  const enabledPriorities = [...new Set(served.map((account) => account.priority))].sort((a, b) => b - a);
  const disabledCount = model.served_by.length - served.length;

  async function regenerate() {
    await runPublicModelWrite(writeGate, async () => {
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
    });
  }

  return (
    <div className="public-model-detail" role="region" aria-label={`公共模型 ${model.name}`}>
      <PageHeader
        eyebrow="公共模型与路由 / 账号与定价"
        title={<span className="pm-model-title">{model.name}</span>}
        dataLead="维护服务账号、路由策略与基准价。账号按优先级分层，同层遵循已保存的选路策略。"
        actions={
          <Button type="button" onClick={onBack}>
            <ArrowLeft aria-hidden />
            返回列表
          </Button>
        }
      />
      {loadError && <div className="note crit" role="alert">完整列表读取失败，请刷新核对：{loadError}</div>}
      <section className="pm-detail-overview" aria-label="公共模型账号概览">
        <div className="pm-detail-identity"><Lamp state={model.retired_at ? "disabled" : "applied"}>{model.retired_at ? "已停用" : "在用"}</Lamp><span>标识 <code>{model.id}</code></span><span>创建于 {fmtBeijing(model.created_at)}</span></div>
        <details className="pm-detail-meta" open={routingEditor ? undefined : true}>
        <summary>模型信息与授权</summary>
        <dl className="pm-detail-facts">
          <div><dt>启用服务账号</dt><dd>{served.length.toLocaleString("zh-CN")}</dd></div>
          <div><dt>启用优先级层</dt><dd>{enabledPriorities.length.toLocaleString("zh-CN")}</dd></div>
          <div><dt>最高启用优先级</dt><dd>{enabledPriorities[0] ?? "—"}</dd></div>
          <div><dt>已停用账号映射</dt><dd>{disabledCount.toLocaleString("zh-CN")}</dd></div>
          <div><dt>授权租户</dt><dd>{model.reachable_tenants.toLocaleString("zh-CN")}</dd></div>
          <div><dt>密钥</dt><dd>{model.reachable_keys.toLocaleString("zh-CN")}</dd></div>
        </dl>
        <p className="hint">完整映射 {model.served_by.length} 个；未启用的映射仍保留。公共模型的稳定标识承接授权与基准价。重新生成只修复模型文档，不修改优先级、权重或启停状态。</p>
        </details>
        {model.retired_at ? <p className="note warn" role="status">模型已停用，恢复后仍需重新接入账号。</p> : served.length === 0 && <p className="note warn" role="status"><strong>无可用账号</strong>：模型仍保留在列表中，授权不受影响，但请求会以 503 失败（无可用上游）。请添加服务账号。</p>}
      </section>
      <RouteNav label="公共模型设置" look="underline" items={[
        ...(routingEditor ? [{ id: "edit" as const, label: "编辑模型" }] : []),
        { id: "routes" as const, label: "路由明细" },
        { id: "baseline" as const, label: "基准价" },
      ]} current={view} onSelect={(next) => { setView(next); if (next === "baseline") setBaselineVisited(true); }} />
      <div className="pm-detail-workspace">
        {routingEditor && <div id="tab-edit" role="tabpanel" aria-labelledby="stop-edit" hidden={view !== "edit"}>{routingEditor}</div>}
        <div id="tab-routes" role="tabpanel" aria-labelledby="stop-routes" hidden={view !== "routes"}>
        <Card className="pm-mapping-card mb-0">
          <CardHeader
            number={3}
            title={<>路由明细<Count n={served.length} /></>}
            description={`完整映射 ${model.served_by.length} 个，启用 ${served.length} 个。优先级高者在前，同层权重高者在前。`}
            actions={canEdit && <Button type="button" icon="refresh" title="按已保存的账号映射重新生成模型文档" disabled={writeGate.busy} busy={busy} busyLabel="重新生成中…" onClick={() => void regenerate()}>重新生成</Button>}
          />
          {error && <div className="note crit" role="alert">重新生成失败：{error}</div>}
          {note && <div className="note" role="status">{note}</div>}
          <div className="mb-4"><RoutePolicy routing={model.effective_routing} /></div>
          {served.length === 0 ? (
            <div className="note warn mb-0 mt-0" role="status">
              <strong>无可用账号</strong>：所有服务该模型的账号都已停用或取消接入。模型仍保留在列表中，授权不受影响，但请求会以
              503 失败（无可用上游）。
            </div>
          ) : null}
          {model.served_by.length > 0 && (
            <DataTable label={served.length === 0 ? "服务账号（全部停用）" : "服务账号"} compact>
              <colgroup>
                <col className="pm-col-tier" />
                <col className="pm-col-account" />
                <col className="pm-col-number" />
                <col className="pm-col-number" />
                <col className="pm-col-upstream" />
                <col className="pm-col-groups" />
                <col className="pm-col-status" />
              </colgroup>
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
                    <tr data-finance-record
                      key={account.provider_key_id}
                      data-tier={tier.priority}
                      data-enabled={account.enabled}
                      className={at === 0 && index > 0 ? "[&>td]:border-t [&>td]:border-t-border" : undefined}
                    >
                      <td data-label="层" className="num font-medium">{at === 0 ? index + 1 : ""}</td>
                      <td data-label="上游账号">
                        <span className="pm-account-name">{account.display_name || account.provider_key_id}</span>
                      </td>
                      <td data-label="优先级" className="num tabular">{account.priority}</td>
                      <td data-label="权重" className="num tabular">{account.weight}</td>
                      <td data-label="上游模型名称" className="pm-upstream-model">
                        {account.upstream_model === null ? (
                          <span className="cell-note">同名（{model.name}）</span>
                        ) : (
                          <span className="mono">{account.upstream_model}</span>
                        )}
                      </td>
                      <td data-label="账号分组">{account.groups.length > 0 ? <GroupList groups={account.groups} /> : "—"}</td>
                      <td data-label="状态">
                        <Lamp state={account.enabled ? "applied" : "disabled"}>{account.enabled ? "启用" : "已停用"}</Lamp>
                      </td>
                    </tr>
                  )),
                )}
              </tbody>
            </DataTable>
          )}
        </Card>
        </div>
        <div id="tab-baseline" role="tabpanel" aria-labelledby="stop-baseline" hidden={view !== "baseline"}>
          {baselineVisited && <PublicModelBaseline key={model.id} modelId={model.id} canEdit={canEdit} prices={baselinePrices} onSaved={onChanged} />}
        </div>
      </div>
    </div>
  );
}
