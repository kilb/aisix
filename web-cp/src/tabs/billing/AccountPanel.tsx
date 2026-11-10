import { useEffect, useState, type ComponentType, type ReactNode } from "react";
import type { TabProps } from "../../App";
import { Msg, type Note } from "../../components/Msg";
import {
  ApiError,
  billing,
  type Budget,
  type BillingAccount,
  type LedgerItem,
  type LedgerSummaryRow,
  tenants,
  type UsageSummaryRow,
} from "../../lib/api";
import { errorText } from "../../lib/fmt";
import { fmtMicroCny, fmtUtc, isCnyAmount, microCnyToInput, periodLabel, todayUtc, utcRange } from "../../lib/money";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { EmptyState } from "../../ui/EmptyState";
import { Field, useRefusalFocus } from "../../ui/Field";
import { Lamp } from "../../ui/Lamp";
import { SidePanel } from "../../ui/SidePanel";
import { useToast } from "../../ui/Toast";
import { TwoStep } from "../../ui/TwoStep";
import { refusal, useLoad } from "../Tenants";
import { FailModeNote } from "../../components/FailModeNote";
import { CreditForm } from "./CreditForm";
import { OrgQuota } from "./OrgQuota";
import "./billing.css";

/** What every region of the panel is given. */
export interface RegionProps {
  account: BillingAccount;
  /** False for a viewer: the region renders no edit controls. */
  canEdit: boolean;
  /** An admin operator: the one role that may credit a tenant directly. */
  isAdmin: boolean;
  /** An action settled `account`'s new state: the list and the panel take it. */
  onChanged: (account: BillingAccount) => void;
  /** Opens another tab, as the shell's `onGoto` does. */
  onGoto?: TabProps["onGoto"];
  /** Bumped whenever an organization quota is written, so the read-only 额度 region reloads. */
  quotaRev?: number;
  onQuotasChanged?: () => void;
}

export const MONEY_HINT = "请输入金额，最多两位小数";

/** A region of the panel under its own heading, a `region` named by it. */
export function Region({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section className="bl-region" role="region" aria-label={label}>
      <h3>{label}</h3>
      {children}
    </section>
  );
}

export const MODE_LABEL: Record<BillingAccount["billing_mode"], string> = { prepaid: "预付", postpaid: "后付" };

/** The state an account's lamp and `data-state` show. */
export function AccountState({ account }: { account: BillingAccount }) {
  return (
    <span data-state={account.exhausted ? "exhausted" : "ok"}>
      <Lamp state={account.exhausted ? "rejected" : "applied"}>{account.exhausted ? "已停止" : "正常"}</Lamp>
    </span>
  );
}

function BalanceCard({ account }: RegionProps) {
  return (
    <Region label="余额">
      <dl className="bl-facts">
        <dt>计费方式</dt>
        <dd>{MODE_LABEL[account.billing_mode] ?? account.billing_mode}</dd>
        <dt>余额</dt>
        <dd className="money">{fmtMicroCny(account.balance_micro_cny)}</dd>
        <dt>信用额度</dt>
        <dd className="money">{fmtMicroCny(account.credit_limit_micro_cny)}</dd>
        <dt>可用</dt>
        <dd className="money">{fmtMicroCny(account.available_micro_cny)}</dd>
        {!account.is_internal && (
          <>
            <dt>状态</dt>
            <dd>
              <AccountState account={account} />
            </dd>
          </>
        )}
        <dt>待审批充值</dt>
        <dd>{account.pending_topups}</dd>
      </dl>
    </Region>
  );
}

function CreditLimit({ account, canEdit, onChanged }: RegionProps) {
  const toast = useToast();
  const stored = microCnyToInput(account.credit_limit_micro_cny);
  const [draft, setDraft] = useState(stored);
  const [busy, setBusy] = useState(false);
  const [refused, setRefused] = useState<ReturnType<typeof refusal> | null>(null);
  const [localError, setLocalError] = useState<string | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  // The stored value moved (a save, another tenant): the draft starts over from it.
  useEffect(() => {
    setDraft(stored);
    setRefused(null);
    setLocalError(null);
  }, [stored, account.tenant_id]);

  async function save(e: React.FormEvent) {
    e.preventDefault();
    const typed = draft.trim();
    if (!isCnyAmount(typed, { allowZero: true })) {
      setLocalError(MONEY_HINT);
      setRefused(null);
      focusRefusal();
      return;
    }
    setLocalError(null);
    setRefused(null);
    setBusy(true);
    try {
      onChanged(await billing.setCreditLimit(account.tenant_id, typed));
      toast("已保存");
    } catch (err) {
      setRefused(refusal(err, (p) => p === "/credit_limit_cny"));
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  // A tenant with no credit limit has only its balance to spend. Postpaid
  // says so up front, because that is also why every call is refused while
  // the balance is not positive; prepaid says which of the two cases it is
  // in, and a tenant still holding money sees no warning at all.
  const postpaidUnset = account.billing_mode === "postpaid" && !account.is_internal && account.credit_limit_micro_cny === 0;
  const prepaidOut = account.billing_mode === "prepaid" && !account.credit_limit_micro_cny && !account.is_internal && account.exhausted;
  const problem = localError ?? refused?.at.get("/credit_limit_cny") ?? null;
  return (
    <Region label="信用额度">
      {postpaidUnset && (
        <div className="note warn" role="status">
          后付租户尚未设置信用额度，所有调用都会被拒绝
        </div>
      )}
      {prepaidOut && (
        <div className="note warn" role="status">
          余额与信用额度都用尽，调用会被拒绝
        </div>
      )}
      {account.billing_mode === "prepaid" && !account.is_internal && !account.credit_limit_micro_cny && !account.exhausted && (
        <p className="hint">尚无信用额度：余额用尽即停止调用</p>
      )}
      {account.is_internal ? (
        <p className="hint">平台内部租户不受余额与信用额度限制。</p>
      ) : !canEdit ? (
        <p className="money">{fmtMicroCny(account.credit_limit_micro_cny)}</p>
      ) : (
        <form ref={form} onSubmit={(e) => void save(e)} noValidate>
          <div className="bl-controls">
            <Field label="信用额度（元）" hint={CREDIT_HINT} error={problem}>
              {(aria) => <input inputMode="decimal" value={draft} onChange={(e) => setDraft(e.target.value)} {...aria} />}
            </Field>
            <Button kind="primary" type="submit" busy={busy} busyLabel="保存中…">
              保存
            </Button>
          </div>
          {refused?.general && (
            <div className="note crit" role="alert">
              {refused.general}
            </div>
          )}
        </form>
      )}
    </Region>
  );
}

/** What the credit limit means, for either mode. */
export const CREDIT_HINT = "可用额度 = 余额 + 信用额度；预付租户透支到 0 即停止调用";

/**
 * The billing mode, changed on the tenant row and saved with `If-Match`.
 * The account carries no version, so the tenant is read first: the edit
 * starts from the version the control plane holds now, and a 412 means
 * someone else moved the tenant meanwhile.
 */
function BillingMode({ account, canEdit, onChanged }: RegionProps) {
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<Note | null>(null);
  const next: BillingAccount["billing_mode"] = account.billing_mode === "prepaid" ? "postpaid" : "prepaid";

  async function change() {
    setBusy(true);
    setNote(null);
    try {
      const tenant = await tenants.get(account.tenant_id);
      await tenants.update(account.tenant_id, { billing_mode: next }, tenant.version);
      onChanged(await billing.account(account.tenant_id));
      toast(`已改为${MODE_LABEL[next]}`);
    } catch (e) {
      setNote({ text: errorText(e), kind: "crit" });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Region label="计费方式">
      <dl className="bl-facts">
        <dt>计费方式</dt>
        <dd>{MODE_LABEL[account.billing_mode] ?? account.billing_mode}</dd>
      </dl>
      <p className="hint">
        {account.is_internal
          ? "平台内部租户不受余额与信用额度限制，计费方式不影响它是否被停止。"
          : "预付 = 余额 + 信用额度；后付 = 同上，月底结算。"}
      </p>
      {!account.is_internal && canEdit && (
        <>
          <div className="bl-controls">
            <TwoStep
              label={`改为${MODE_LABEL[next]}`}
              kind="secondary"
              busy={busy}
              busyLabel="保存中…"
              warning={`切换后立即按${MODE_LABEL[next]}判断是否停止调用，余额与信用额度不变。`}
              onConfirm={() => void change()}
            />
          </div>
          <Msg note={note} />
        </>
      )}
    </Region>
  );
}

// ── date range and period controls ──────────────────────────────────────────

/** The longest span the backend sums, by period (billing/range.rs). */
const MAX_SUMMARY_DAYS = { day: 92, month: 731 } as const;

/**
 * The two date fields: whole UTC days, inclusive at both ends. `group` is set for a summary,
 * whose span the backend caps: a range it would refuse is refused here, with its reason, so
 * 重试 never resends the same refused query.
 */
function useDays(group?: "day" | "month") {
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  let problem: string | null = null;
  if (from !== "" && to !== "" && to < from) problem = "结束日期不能早于开始日期";
  else if (from !== "" && to === "" && from > todayUtc()) problem = "开始日期不能晚于今天";
  else if (group) {
    const range = summaryRange(group, from, to);
    const span = (Date.parse(range.to) - Date.parse(range.from)) / 86_400_000;
    if (span <= 0) problem = "结束日期须晚于开始日期";
    else if (span > MAX_SUMMARY_DAYS[group]) problem = "按天最多汇总 92 天，按月最多 731 天，请缩短日期范围。";
  }
  const backwards = problem !== null;
  const fields = (
    <>
      <Field label="开始日期（UTC）">
        <input type="date" value={from} onChange={(e) => setFrom(e.target.value)} />
      </Field>
      <Field label="结束日期（UTC）" error={problem}>
        {(aria) => <input type="date" value={to} onChange={(e) => setTo(e.target.value)} {...aria} />}
      </Field>
    </>
  );
  return { from, to, backwards, fields };
}

function utcDay(daysBack: number): string {
  const now = new Date();
  return new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - daysBack)).toISOString().slice(0, 10);
}

/** The range a summary covers when no dates are chosen: 30 days by day, 12 months by month, up to today (UTC). */
function summaryRange(group: "day" | "month", from: string, to: string): { from: string; to: string } {
  const now = new Date();
  const start =
    group === "day"
      ? utcDay(29)
      : new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - 11, 1)).toISOString().slice(0, 10);
  return utcRange(from || start, to || todayUtc());
}

function Failed({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div className="note crit" role="alert">
      {message}{" "}
      <Button type="button" size="sm" onClick={onRetry}>
        重试
      </Button>
    </div>
  );
}

// ── ledger ──────────────────────────────────────────────────────────────────

const KIND_LABEL: Record<LedgerItem["kind"], string> = { usage: "扣费", topup: "充值", adjustment: "调整" };

function entryNote(item: LedgerItem): string {
  if (item.kind === "usage") return item.model?.name ?? "";
  if (item.kind === "topup") return "充值";
  if (item.kind === "adjustment") return "重新计价调整";
  return "";
}

function Ledger({ account }: RegionProps) {
  const days = useDays();
  const [kind, setKind] = useState<LedgerItem["kind"] | "">("");
  const [more, setMore] = useState<{ items: LedgerItem[]; next: number | null } | null>(null);
  const [moreBusy, setMoreBusy] = useState(false);
  const [moreError, setMoreError] = useState<string | null>(null);
  const range = days.from || days.to ? utcRange(days.from || "1970-01-01", days.to || todayUtc()) : {};
  const first = useLoad(
    () =>
      days.backwards
        ? Promise.resolve(null)
        : billing.ledger(account.tenant_id, { ...(days.from || days.to ? range : {}), ...(kind ? { kind } : {}) }),
    [account.tenant_id, account.balance_micro_cny, days.from, days.to, days.backwards, kind],
  );
  // A new first page replaces whatever was loaded after the old one.
  useEffect(() => {
    setMore(null);
    setMoreError(null);
  }, [first.data]);

  const items = first.data ? [...first.data.items, ...(more?.items ?? [])] : null;
  const next = more ? more.next : (first.data?.next_before ?? null);

  async function loadMore() {
    if (next === null) return;
    setMoreBusy(true);
    setMoreError(null);
    try {
      const page = await billing.ledger(account.tenant_id, {
        ...(days.from || days.to ? range : {}),
        ...(kind ? { kind } : {}),
        before: next,
      });
      setMore((m) => ({ items: [...(m?.items ?? []), ...page.items], next: page.next_before }));
    } catch (e) {
      setMoreError(errorText(e));
    } finally {
      setMoreBusy(false);
    }
  }

  return (
    <Region label="余额流水">
      <div className="bl-controls">
        {days.fields}
        <Field label="类型">
          <select value={kind} onChange={(e) => setKind(e.target.value as LedgerItem["kind"] | "")}>
            <option value="">全部</option>
            <option value="usage">扣费</option>
            <option value="topup">充值</option>
            <option value="adjustment">调整</option>
          </select>
        </Field>
      </div>
      {first.error ? (
        <Failed message={first.error} onRetry={() => void first.reload()} />
      ) : items === null ? (
        !days.backwards && <Loading />
      ) : items.length === 0 ? (
        <EmptyState text="这个范围内没有流水。" />
      ) : (
        <DataTable label="余额流水" compact>
          <thead>
            <tr>
              <th>时间（UTC）</th>
              <th>类型</th>
              <th className="num">金额</th>
              <th className="num">余额</th>
              <th>说明</th>
            </tr>
          </thead>
          <tbody>
            {items.map((i) => (
              <tr key={i.seq}>
                <td className="tabular">{fmtUtc(i.posted_at)}</td>
                <td>{KIND_LABEL[i.kind] ?? i.kind}</td>
                <td className="num">{fmtMicroCny(i.amount_micro_cny)}</td>
                <td className="num">{fmtMicroCny(i.balance_after_micro_cny)}</td>
                <td>{entryNote(i)}</td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
      {moreError && (
        <div className="note crit" role="alert">
          {moreError}
        </div>
      )}
      {next !== null && (
        <Button type="button" busy={moreBusy} busyLabel="加载更多" onClick={() => void loadMore()}>
          加载更多
        </Button>
      )}
    </Region>
  );
}

// ── per-period summaries ────────────────────────────────────────────────────

/** A summary region: its own group and dates, `load` for the rows, `children` the table over them. */
function Summary<T>({
  label,
  tenantId,
  stamp,
  load,
  render,
}: {
  label: string;
  tenantId: string;
  /** Something that changes when the rows may have: the balance. */
  stamp: number;
  load: (q: { group: "day" | "month"; from: string; to: string }) => Promise<T[]>;
  render: (rows: T[], group: "day" | "month") => ReactNode;
}) {
  const [group, setGroup] = useState<"day" | "month">("day");
  const days = useDays(group);
  const rows = useLoad(
    () => (days.backwards ? Promise.resolve(null) : load({ group, ...summaryRange(group, days.from, days.to) })),
    [tenantId, stamp, group, days.from, days.to, days.backwards],
  );
  return (
    <Region label={label}>
      <div className="bl-controls">
        <Field label="汇总方式">
          <select value={group} onChange={(e) => setGroup(e.target.value as "day" | "month")}>
            <option value="day">按天</option>
            <option value="month">按月</option>
          </select>
        </Field>
        {days.fields}
      </div>
      <p className="hint">未选日期时，按天汇总最近 30 天，按月汇总最近 12 个月（UTC）。</p>
      {rows.error ? (
        <Failed message={rows.error} onRetry={() => void rows.reload()} />
      ) : rows.data === null ? (
        !days.backwards && <Loading />
      ) : rows.data.length === 0 ? (
        <EmptyState text="这个范围内没有数据。" />
      ) : (
        render(rows.data, group)
      )}
    </Region>
  );
}

function PeriodSummary({ account }: RegionProps) {
  return (
    <Summary<LedgerSummaryRow>
      label="按期汇总"
      tenantId={account.tenant_id}
      stamp={account.balance_micro_cny}
      load={(q) => billing.ledgerSummary(account.tenant_id, q)}
      render={(rows, group) => (
        <DataTable label="按期汇总" compact>
          <thead>
            <tr>
              <th>期间</th>
              <th className="num">扣费</th>
              <th className="num">充值</th>
              <th className="num">调整</th>
              <th className="num">期末余额</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.period_start}>
                <td className="tabular">{periodLabel(r.period_start, group)}</td>
                <td className="num">{fmtMicroCny(r.usage_micro_cny)}</td>
                <td className="num">{fmtMicroCny(r.topups_micro_cny)}</td>
                <td className="num">{fmtMicroCny(r.adjustments_micro_cny)}</td>
                <td className="num">{fmtMicroCny(r.closing_balance_micro_cny)}</td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
    />
  );
}

const DIMENSION_LABEL: Record<string, string> = {
  input: "输入",
  output: "输出",
  cache_read: "缓存读",
  cache_write: "缓存写",
  cache_write_1h: "1 小时缓存写",
  reasoning: "推理",
  audio_input: "音频输入",
  audio_output: "音频输出",
  image_input: "图片输入",
  image_output: "图片输出",
  web_search: "联网搜索",
};

/** The model of a usage row: the row that rolls up the rows past the cap is 其他; one outside the tenant's grants has no name. */
function usageModel(m: UsageSummaryRow["model"]): string {
  if (m.id === null && m.name === "other") return "其他";
  return m.name === "" ? "未授权或未知模型" : m.name;
}

function UsageSummary({ account }: RegionProps) {
  return (
    <Summary<UsageSummaryRow>
      label="用量汇总"
      tenantId={account.tenant_id}
      stamp={account.balance_micro_cny}
      load={(q) => billing.usageSummary(account.tenant_id, q)}
      render={(rows, group) => (
        <DataTable label="用量汇总" compact>
          <thead>
            <tr>
              <th>期间</th>
              <th>模型</th>
              <th className="num">调用次数</th>
              <th className="num">费用</th>
              <th>计量</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={`${r.period_start}/${r.model.id ?? r.model.name}`}>
                <td className="tabular">{periodLabel(r.period_start, group)}</td>
                <td>{usageModel(r.model)}</td>
                <td className="num">{r.requests}</td>
                <td className="num">{fmtMicroCny(r.charged_micro_cny)}</td>
                <td>
                  {Object.entries(r.quantities)
                    .map(([dimension, n]) => `${DIMENSION_LABEL[dimension] ?? dimension} ${n}`)
                    .join("，")}
                </td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
    />
  );
}

// ── quotas, read-only ───────────────────────────────────────────────────────

const SCOPE_LABEL: Record<Budget["scope"], string> = { team: "组织", member: "成员", api_key: "密钥" };
const SET_BY_LABEL: Record<Budget["set_by"], string> = { operator: "平台", org_admin: "组织管理员", member: "成员" };
export const PERIOD_LABEL: Record<NonNullable<Budget["period"]>, string> = {
  day: "每天",
  week: "每周（周一起）",
  month: "每月",
};

export function budgetPeriod(b: Budget): string {
  if (b.kind === "cumulative") return b.counted_from ? `自 ${periodLabel(b.counted_from, "day")} 起` : "—";
  return b.period ? (PERIOD_LABEL[b.period] ?? b.period) : "—";
}

function Quotas({ account, quotaRev }: RegionProps) {
  const budgets = useLoad(() => billing.budgets(account.tenant_id), [account.tenant_id, account.balance_micro_cny, quotaRev]);
  return (
    <Region label="额度">
      <p className="hint">这个租户在每一级设置的额度。组织额度在下方的「组织额度」里设置；成员与密钥的额度由租户自己管理，这里只读。</p>
      {budgets.error ? (
        <Failed message={budgets.error} onRetry={() => void budgets.reload()} />
      ) : budgets.data === null ? (
        <Loading />
      ) : budgets.data.length === 0 ? (
        <EmptyState text="这个租户没有设置额度。" />
      ) : (
        <DataTable label="额度" compact>
          <thead>
            <tr>
              <th>层级</th>
              <th>对象</th>
              <th>设置方</th>
              <th>类型</th>
              <th>周期（UTC）</th>
              <th className="num">限额</th>
              <th className="num">已用</th>
              <th className="num">剩余</th>
              <th>状态</th>
            </tr>
          </thead>
          <tbody>
            {budgets.data.map((b) => (
              <tr key={b.id}>
                <td>{SCOPE_LABEL[b.scope] ?? b.scope}</td>
                <td>
                  <span className="trunc" title={b.target.label}>
                    {b.target.label}
                  </span>
                  {!b.target_live && "（已失效）"}
                </td>
                <td>{SET_BY_LABEL[b.set_by] ?? b.set_by}</td>
                <td>{b.kind === "cumulative" ? "一次性额度" : "按周期"}</td>
                <td>{budgetPeriod(b)}</td>
                <td className="num">{fmtMicroCny(b.limit_micro_cny)}</td>
                <td className="num">{fmtMicroCny(b.spent_micro_cny)}</td>
                <td className="num">{fmtMicroCny(b.remaining_micro_cny)}</td>
                <td>
                  <span data-state={b.exhausted ? "exhausted" : "ok"}>
                    <Lamp state={b.exhausted ? "rejected" : "applied"}>{b.exhausted ? "已用尽" : "正常"}</Lamp>
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
    </Region>
  );
}

/** The environment-wide budget fail mode, where an operator reading a tenant's budgets would wonder about it. */
function FailMode({ onGoto }: RegionProps) {
  return (
    <Region label="预算失败模式">
      <FailModeNote onEdit={onGoto && (() => onGoto("resources", "budget_policy"))} />
    </Region>
  );
}

// ── the panel ───────────────────────────────────────────────────────────────

/** The regions of the panel, top to bottom. Later tasks append theirs. */
export const PANEL_REGIONS: { id: string; Component: ComponentType<RegionProps> }[] = [
  { id: "balance", Component: BalanceCard },
  { id: "billing-mode", Component: BillingMode },
  { id: "credit-limit", Component: CreditLimit },
  { id: "ledger", Component: Ledger },
  { id: "period-summary", Component: PeriodSummary },
  { id: "usage-summary", Component: UsageSummary },
  { id: "quotas", Component: Quotas },
  { id: "org-quota", Component: OrgQuota },
  { id: "credit", Component: CreditForm },
  { id: "fail-mode", Component: FailMode },
];

/**
 * One tenant's billing, beside the account list. The account shown is the
 * list's, kept current by the actions that settle a new one; the panel
 * reads it fresh once when it opens, so a stale list never misleads.
 */
export function AccountPanel({
  account,
  canEdit,
  isAdmin,
  onChanged,
  onGoto,
  onClose,
}: RegionProps & {
  onClose: () => void;
}) {
  const [note, setNote] = useState<Note | null>(null);
  const [quotaRev, setQuotaRev] = useState(0);
  const tenantId = account.tenant_id;
  useEffect(() => {
    const controller = new AbortController();
    setNote(null);
    billing
      .account(tenantId, controller.signal)
      .then(onChanged)
      .catch((e: unknown) => {
        if (controller.signal.aborted) return;
        setNote({ text: e instanceof ApiError ? errorText(e) : String(e), kind: "crit" });
      });
    return () => controller.abort();
    // Once per tenant opened; `onChanged` is the list's own setter.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tenantId]);
  return (
    <SidePanel
      title={account.tenant_name}
      label={`租户计费：${account.tenant_name}`}
      extra={account.is_internal ? <span className="tag">平台内部</span> : undefined}
      onClose={onClose}
    >
      <Msg note={note} />
      {PANEL_REGIONS.map(({ id, Component }) => (
        <Component
          key={`${tenantId}/${id}`}
          account={account}
          canEdit={canEdit}
          isAdmin={isAdmin}
          onChanged={onChanged}
          onGoto={onGoto}
          quotaRev={quotaRev}
          onQuotasChanged={() => setQuotaRev((n) => n + 1)}
        />
      ))}
    </SidePanel>
  );
}
