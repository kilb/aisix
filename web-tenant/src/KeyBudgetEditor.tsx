import { useEffect, useState } from "react";
import { BudgetDialog } from "./MemberBudgetDialog";
import { Card, CardHeader, CardMessage } from "./components/page";
import { ApiError, billing, budgets, type QuotaFigure, type SessionMember } from "./lib/api";
import { formMessageFor } from "./lib/errors";
import { fmtMicroCny } from "./lib/money";
import { Button } from "./ui/Button";
import { Loading } from "./ui/DataTable";
import { TwoStep } from "./ui/TwoStep";

const SOURCE = { operator: "平台", org_admin: "组织管理员", member: "成员" };
const PERIOD = { day: "每天", week: "每周", month: "每月" };
const failure = (e: unknown) => e instanceof ApiError ? formMessageFor(e) : String(e);

export function KeyBudgetEditor({ keyId, role, onInactive }: {
  keyId: string; role: SessionMember["role"]; onInactive: () => void;
}) {
  const [revision, setRevision] = useState(0);
  const [rows, setRows] = useState<QuotaFigure[] | null>(null);
  const [readError, setReadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<{ row: QuotaFigure | null } | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [notice, setNotice] = useState("");
  useEffect(() => {
    let live = true;
    setRows(null);
    setReadError(null);
    void billing.keyQuota(keyId).then(data => {
      if (live) setRows(data.quotas.filter(row => row.scope === "api_key"));
    }, e => { if (live) setReadError(failure(e)); });
    return () => { live = false; };
  }, [keyId, revision]);

  function changed(message: string) {
    setDialog(null);
    setError(null);
    setNotice(message);
    setRevision(n => n + 1);
  }
  async function remove(id: string) {
    setRemoving(id);
    setError(null);
    try {
      await budgets.remove(id);
      changed("密钥额度已移除");
    } catch (e) {
      if (e instanceof ApiError && e.code === "tenant_not_active") onInactive();
      setError(failure(e));
    } finally {
      setRemoving(null);
    }
  }

  return <Card role="region" aria-label="密钥费用额度" className="mt-4">
    <CardHeader title="密钥费用额度" description="各项费用额度单独保存。组织和成员的额度同时生效。"
      actions={rows !== null && <Button disabled={removing !== null} onClick={() => setDialog({ row: null })}>设置密钥额度</Button>} />
    {notice && <p role="status" className="note">{notice}</p>}
    {error && <p role="alert" className="note crit">{error}</p>}
    {readError ? <CardMessage onRetry={() => setRevision(n => n + 1)}>{readError}</CardMessage> : rows === null ? <Loading /> :
      rows.length === 0 ? <p className="hint">未设置密钥费用额度</p> :
      <ul className="grid gap-3">{rows.map(row => {
        const canManage = row.set_by !== "operator" && (role === "owner" || role === "admin" || (role === "member" && row.set_by === "member"));
        const exact = Number.isSafeInteger(row.limit_micro_cny);
        const period = row.period ? PERIOD[row.period] : "累计";
        return <li key={row.id} className="flex flex-wrap items-center gap-2">
          <span>{period} · {exact ? fmtMicroCny(row.limit_micro_cny) : "金额超出浏览器精确范围"} · {SOURCE[row.set_by]}设置</span>
          {canManage && <>
            <Button size="sm" disabled={removing !== null || !exact} onClick={() => setDialog({ row })}>调整额度</Button>
            <TwoStep size="sm" label="移除额度" disabled={removing !== null} busy={removing === row.id}
              warning="移除此项后，其他额度仍然生效。" onConfirm={() => void remove(row.id)} />
          </>}
        </li>;
      })}</ul>}
    {dialog && <BudgetDialog scope="api_key" targetId={keyId} budget={dialog.row} onClose={() => setDialog(null)}
      onInactive={onInactive} onSaved={() => changed("密钥额度已保存")} />}
  </Card>;
}
