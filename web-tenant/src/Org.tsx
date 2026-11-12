import { useEffect, useState, type ReactNode } from "react";
import {
  ApiError,
  MIN_PASSWORD_CHARS,
  budgets as budgetApi,
  changePassword,
  org as readOrg,
  usageSummary,
  type Budget,
  type MemberRole,
  type Org as OrgView,
  type SessionMember,
  type TenantStatus,
  type UsageSummaryRow,
} from "./lib/api";
import { formulaBalance, type FormulaBalance } from "./lib/formulaApi";
import { exactRatio, summarizeUsage } from "./lib/chartFormat";
import { budgetAmounts, exactOverviewAmount, overviewAccess } from "./lib/overview";
import { fmtMicroCny, todayUtc, utcRange } from "./lib/money";
import { formMessageFor, messageFor } from "./lib/errors";
import { Button } from "./ui/Button";
import { Field } from "./ui/Field";
import { Lamp, type LampState } from "./ui/Lamp";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, MetricSkeleton, PageHeader, span } from "./components/page";
import { BudgetProgress } from "./components/charts";
import { UsageCostCard, UsageModelsCard, UsageRequestCard } from "./components/UsageAnalytics";
import { ApiMetricsPanel } from "./components/ApiMetricsPanel";
import { CreateMember } from "./CreateMember";

export const ROLE_LABEL: Record<MemberRole, string> = {
  owner: "所有者",
  admin: "管理员",
  member: "成员",
  billing: "账单",
};

const STATUS_LABEL: Record<TenantStatus, string> = {
  pending: "等待审核",
  active: "已启用",
  suspended: "已停用",
};

const STATUS_LAMP: Record<TenantStatus, LampState> = {
  pending: "pending",
  active: "applied",
  suspended: "disabled",
};

/** 未配置的上限表示不限，与零不同。 */
function cap(value: number | undefined): string {
  return value === undefined ? "不限" : value.toLocaleString("en-US");
}

function failure(error: unknown): string {
  return error instanceof ApiError ? messageFor(error) : String(error);
}

function Reading({ label, value, title }: { label: string; value: ReactNode; title?: string }) {
  return (
    <div className="min-w-0 border-b border-ink/7 py-3 last:border-b-0">
      <dt className="text-micro text-muted">{label}</dt>
      <dd className="mt-1 break-words text-body" title={title}>{value}</dd>
    </div>
  );
}

/** 统计读取以已取得的组织状态为准，暂停期间只保留既有受限操作。 */
export function Org({ member, status }: { member: SessionMember; status: TenantStatus }) {
  const [org, setOrg] = useState<OrgView | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [rows, setRows] = useState<UsageSummaryRow[] | null>(null);
  const [balance, setBalance] = useState<FormulaBalance | null>(null);
  const [budgets, setBudgets] = useState<Budget[] | null>(null);
  const [usageErr, setUsageErr] = useState<string | null>(null);
  const [balanceErr, setBalanceErr] = useState<string | null>(null);
  const [budgetErr, setBudgetErr] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [range] = useState(() => {
    const now = new Date();
    const start = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - 29));
    return utcRange(todayUtc(start), todayUtc(now));
  });

  useEffect(() => {
    let live = true;
    readOrg().then(
      (value) => { if (live) { setOrg(value); setErr(null); } },
      (error: unknown) => { if (live) setErr(failure(error)); },
    );
    return () => { live = false; };
  }, [refresh]);

  const shown = org?.status ?? status;
  const access = overviewAccess(org?.status ?? "pending", member.role);
  useEffect(() => {
    let live = true;
    setRows(null);
    setBalance(null);
    setBudgets(null);
    setUsageErr(null);
    setBalanceErr(null);
    setBudgetErr(null);
    if (access.usage) usageSummary({ group: "day", from: range.from!, to: range.to! }).then(
      (items) => { if (live) setRows(items); },
      (error: unknown) => { if (live) setUsageErr(failure(error)); },
    );
    if (access.budgets) budgetApi.list().then(
      (items) => { if (live) setBudgets(items); },
      (error: unknown) => { if (live) setBudgetErr(failure(error)); },
    );
    if (access.billing) formulaBalance().then(
      (value) => { if (live) setBalance(value); },
      (error: unknown) => { if (live) setBalanceErr(failure(error)); },
    );
    return () => { live = false; };
  }, [access.usage, access.budgets, access.billing, member.id, member.role, range.from, range.to, refresh]);

  const canCreate = org?.status === "active" && (member.role === "owner" || member.role === "admin");
  const roles: MemberRole[] = member.role === "owner" ? ["owner", "admin", "member", "billing"] : ["admin", "member", "billing"];
  const summary = rows === null ? null : summarizeUsage(rows);
  const rangeLabel = "最近 30 天（UTC）";
  const scopeLabel = member.role === "member" ? "你的密钥" : "全组织";
  const retry = () => setRefresh((value) => value + 1);
  const lead = access.usage && summary && !usageErr && summary.requests !== null
    ? `${scopeLabel}最近 30 天共 ${summary.requests.toLocaleString("en-US")} 次可见调用尝试，费用${summary.chargedMicroCny === null ? "未知" : ` ${fmtMicroCny(summary.chargedMicroCny)}`}。`
    : shown === "active" ? "查看组织身份、真实用量与各项独立额度。" : "查看组织状态并管理你的登录口令。";
  return (
    <>
      <PageHeader title="组织" eyebrow="OVERVIEW · 组织概览" dataLead={lead} actions={<Button kind="secondary" onClick={retry}>刷新</Button>} />
      {shown === "pending" && <div className="note warn mb-3">组织正在等待平台审核，审核通过后即可创建密钥和邀请成员</div>}
      {shown === "suspended" && <div className="note crit mb-3">组织已被停用，请联系平台</div>}
      {err && <div className="note crit mb-3" role="alert">{err}</div>}
      <BentoGrid>
        {access.usage && <>
          <UsageRequestCard rows={rows} error={usageErr} rangeLabel={`${rangeLabel} · ${scopeLabel}`} number={1} className={span.d8} onRetry={retry} />
          <UsageCostCard rows={rows} error={usageErr} rangeLabel={`${rangeLabel} · ${scopeLabel}`} number={2} className={span.d4} onRetry={retry} />
          <OverviewBudgets budgets={budgets} error={budgetErr} balance={balance} balanceError={balanceErr} showBalance={access.billing} onRetry={retry} />
          <UsageModelsCard rows={rows} error={usageErr} rangeLabel={`${rangeLabel} · ${scopeLabel}`} number={4} className={span.d3} onRetry={retry} />
        </>}
        <Card className={access.usage ? span.d4 : span.d6}>
          <CardHeader number={access.usage ? 5 : 1} title="组织资料" />
          {org === null ? err ? <CardMessage onRetry={retry}>组织资料加载失败</CardMessage> : <MetricSkeleton /> : <dl>
            <Reading label="名称" value={org.name} title={org.name} />
            <Reading label="状态" value={STATUS_LAMP[shown] ? <Lamp state={STATUS_LAMP[shown]}>{STATUS_LABEL[shown]}</Lamp> : shown} />
            <Reading label="你的角色" value={ROLE_LABEL[member.role] ?? member.role} />
            <Reading label="单个密钥的并发上限" value={cap(org.limits.key_concurrency_max)} />
            <Reading label="单个密钥的每分钟请求上限" value={cap(org.limits.key_rpm_max)} />
          </dl>}
        </Card>
        <ApiMetricsPanel refreshKey={refresh} scope="tenant" enabled={access.usage} from={range.from} to={range.to} numberStart={6} className={span.d12} />
        {canCreate && <div className={span.d6}>
          <CreateMember number={8} roles={roles} labels={ROLE_LABEL} onInactive={retry} />
        </div>}
        <PasswordForm primary={!canCreate} number={access.usage ? canCreate ? 9 : 8 : 2} className={access.usage && !canCreate ? span.d12 : span.d6} />
      </BentoGrid>
    </>
  );
}

export function OverviewBudgets({ budgets, error, balance, balanceError, showBalance, onRetry }: {
  budgets: Budget[] | null;
  error: string | null;
  balance: FormulaBalance | null;
  balanceError: string | null;
  showBalance: boolean;
  onRetry: () => void;
}) {
  const available = balance === null ? null : exactOverviewAmount(balance, "available_micro_cny");
  const cash = balance === null ? null : exactOverviewAmount(balance, "balance_micro_cny");
  const credit = balance === null ? null : exactOverviewAmount(balance, "credit_limit_micro_cny");
  const shown = [...(budgets ?? [])].filter((budget) => budget.target_live).sort((a, b) => Number(b.exhausted) - Number(a.exhausted)).slice(0, 2);
  return <Card className={span.d5}>
    <CardHeader number={3} title={showBalance ? "账户与预算" : "我的预算"} actions={<a className="inline-flex min-h-11 items-center text-caption underline underline-offset-4" href="#budgets">全部预算 ↗</a>} />
    {showBalance && <div className="mb-6">
      <p className="text-micro text-muted">账户可用额度</p>
      {balanceError ? <CardMessage onRetry={onRetry}>{balanceError}</CardMessage> : balance === null ? <MetricSkeleton /> : <>
        <Metric size="value" className="mt-2 break-all">{available === null ? "未知" : fmtMicroCny(available)}</Metric>
        <dl className="mt-3 flex flex-wrap gap-x-5 gap-y-2 text-caption">
          <div><dt className="text-muted">余额</dt><dd className="mt-1 tabular-nums">{cash === null ? "未知" : fmtMicroCny(cash)}</dd></div>
          <div><dt className="text-muted">信用额度</dt><dd className="mt-1 tabular-nums">{credit === null ? "未知" : fmtMicroCny(credit)}</dd></div>
        </dl>
        <p className="mt-3 text-caption text-muted">可用额度为余额与信用额度之和，预算另行独立约束。{balance.exhausted ? "账户额度已耗尽。" : ""}</p>
        {balance.calculation?.available_overflow && <p className="mt-3 text-caption text-muted">可用额度超出存储范围，显示值为饱和值。请在账务页查看完整计算说明。</p>}
      </>}
    </div>}
    {error ? <CardMessage onRetry={onRetry}>{error}</CardMessage> : budgets === null ? <MetricSkeleton /> : shown.length === 0 ? <CardMessage>没有生效对象的额外预算。账户余额与组织状态仍单独约束调用。</CardMessage> : <div className="grid gap-5">
      {shown.map((budget) => {
        const amount = budgetAmounts(budget);
        const scope = { team: "组织", member: "成员", api_key: "密钥" }[budget.scope];
        const period = budget.kind === "cumulative" ? "累计" : { day: "本日", week: "本周", month: "本月" }[budget.period ?? "month"];
        const spent = amount.spent === null ? "未知" : fmtMicroCny(amount.spent);
        const limit = amount.limit === null ? "未知" : fmtMicroCny(amount.limit);
        return <div key={budget.id}>
          <p className="mb-2 break-words text-body">{scope} · {budget.target.label} <span className="text-caption text-muted">{period}</span></p>
          <BudgetProgress value={exactRatio(amount.spent, amount.limit)} accessibleLabel={`${scope}${budget.target.label}预算，已用${spent}，限额${limit}${budget.exhausted ? "，已耗尽" : ""}`} label={`已用 ${spent}`} valueLabel={`限额 ${limit}`} />
          <p className="mt-2 text-caption text-muted">剩余 {amount.remaining === null ? "未知" : fmtMicroCny(amount.remaining)}{budget.exhausted ? " · 已耗尽" : ""}</p>
        </div>;
      })}
    </div>}
    <CardFooter><p className="text-caption text-muted">每项预算独立约束，不能相加为总可用额度。{budgets && shown.length > 0 ? `展示 ${shown.length} 项，共 ${budgets.length} 项可见预算。` : ""}</p></CardFooter>
  </Card>;
}

/** The caller's own password. A change ends their other sessions. */
function PasswordForm({ primary, number, className }: { primary: boolean; number: number; className: string }) {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setMsg(null);
    if ([...next].length < MIN_PASSWORD_CHARS) {
      setMsg({ ok: false, text: `新口令至少需要 ${MIN_PASSWORD_CHARS} 个字符` });
      return;
    }
    setBusy(true);
    try {
      await changePassword(current, next);
      setCurrent("");
      setNext("");
      setMsg({ ok: true, text: "口令已修改，其他设备上的登录已退出" });
    } catch (e) {
      const text =
        e instanceof ApiError
          ? e.code === "invalid_credentials"
            ? "当前口令不正确"
            : formMessageFor(e)
          : String(e);
      setMsg({ ok: false, text });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card className={className}>
      <CardHeader number={number} title="修改口令" />
      <form className="grid gap-4" onSubmit={submit}>
        <Field label="当前口令">
          <input
            type="password"
            autoComplete="current-password"
            required
            value={current}
            onChange={(e) => setCurrent(e.target.value)}
          />
        </Field>
        <Field label="新口令" hint={<>至少 {MIN_PASSWORD_CHARS} 个字符</>}>
          {(aria) => (
            <input
              type="password"
              autoComplete="new-password"
              required
              value={next}
              onChange={(e) => setNext(e.target.value)}
              {...aria}
            />
          )}
        </Field>
        <div className="flex justify-end">
          <Button kind={primary ? "primary" : "secondary"} type="submit" busy={busy}>
            修改口令
          </Button>
        </div>
      </form>
      {msg &&
        (msg.ok ? (
          <div className="note ok" role="status">
            {msg.text}
          </div>
        ) : (
          <div className="note crit" role="alert">
            {msg.text}
          </div>
        ))}
    </Card>
  );
}
