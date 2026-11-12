import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  ApiError,
  billing,
  call,
  budgets as budgetApi,
  keys as keyApi,
  type Budget,
  type KeyQuota,
  type LedgerKind,
  type LedgerSummaryRow,
  type Page,
  type QuotaLevel,
  type SessionMember,
  type TenantKey,
  type Topup,
} from "./lib/api";
import { formulaBalance, formulaLedger, type FormulaBalance as Balance, type FormulaLedgerItem as LedgerItem } from "./lib/formulaApi";
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtMicroCny, isCnyAmount } from "./lib/money";
import { beijingRange, defaultCalendarRange, fmtBeijing, periodLabel } from "./lib/time";
import { useRecordTable, type RecordPageSize } from "./lib/recordTable";
import { RecordTablePager, RecordTableToolbar } from "./ui/RecordTable";
import { BalanceReadout, Money, type ReadoutStop } from "./ui/BalanceReadout";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field, useRefusalFocus } from "./ui/Field";
import { Lamp, type LampState } from "./ui/Lamp";
import { useToast } from "./ui/Toast";
import { BentoGrid, Card, CardHeader, Metric, PageHeader, span } from "./components/page";
import { TrendChart } from "./components/charts";
import { exactInteger, normalizeExactValues } from "./lib/chartFormat";
import { cn } from "./lib/cn";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** 图形和公式只显示可靠整数，不把旧接口舍入值显示为精确金额。 */
function moneyText(value: number | string | bigint): string {
  const integer = exactInteger(value);
  return integer === null ? "未知" : fmtMicroCny(integer);
}

/** Longest a top-up's note and reference may be: the control plane's own limits. */
const NOTE_MAX = 500;
const REFERENCE_MAX = 128;

/** One figure of the balance: its label over the amount, a tile of the card's grid. */
function Reading({ label, value, hero = false }: { label: string; value: ReactNode; hero?: boolean }) {
  return (
    <div className="entry min-w-0">
      <span className="text-meta font-mono text-muted">{label}</span>
      <div className="val"><Metric size={hero ? "hero" : "value"} className="mt-3 whitespace-normal break-all">{value}</Metric></div>
    </div>
  );
}

/** 保留业务区域名称，编号只用于视觉阅读顺序。 */
function Section({ title, number, actions, children, className = span.d12, tone }: { title: string; number: number; actions?: ReactNode; children: ReactNode; className?: string; tone?: "surface" | "inverse" }) {
  return (
    <Card role="region" aria-label={title} className={className} tone={tone}>
      <CardHeader number={number} title={title} actions={actions} />
      {children}
    </Card>
  );
}

/**
 * 「账户」: what the organisation can still spend, its top-up requests and
 * its ledger. Owners and admins read the readout for one of their keys;
 * a billing member, who holds none, reads the organisation's alone.
 */
export function Billing({ member }: { member: SessionMember }) {
  const [balance, setBalance] = useState<Balance | null>(null);
  const [balanceErr, setBalanceErr] = useState<string | null>(null);
  const [topups, setTopups] = useState<Topup[] | null>(null);
  const [topupsVersion, setTopupsVersion] = useState(0);
  const rememberTopups = useCallback((items: Topup[]) => {
    setTopups((current) => [...new Map([...(current ?? []), ...items].map((item) => [item.id, item])).values()]);
  }, []);

  const loadBalance = useCallback(async () => {
    try {
      setBalance(await formulaBalance());
      setBalanceErr(null);
    } catch (e) {
      setBalanceErr(failure(e));
    }
  }, []);

  // The organisation's own quotas, which owner, admin and billing may read. A
  // quota that is used up refuses calls while the balance is still positive,
  // so the lamp and the billing member's readout both need them. `null` until
  // they are read: a read that failed is not an organisation without quotas,
  // so nothing that depends on them is claimed until a read succeeds.
  const [team, setTeam] = useState<Budget[] | null>(null);
  const [teamErr, setTeamErr] = useState<string | null>(null);
  /** The newest read of the quotas asked for (a retry is another): an answer to an older one is dropped. */
  const teamTicket = useRef(0);
  const loadTeam = useCallback(async () => {
    const n = ++teamTicket.current;
    try {
      const list = await budgetApi.list();
      if (n !== teamTicket.current) return;
      setTeam(list.filter((b) => b.scope === "team"));
      setTeamErr(null);
    } catch (e) {
      if (n !== teamTicket.current) return;
      setTeam(null);
      setTeamErr(failure(e));
    }
  }, []);

  useEffect(() => {
    void loadBalance();
    void loadTeam();
  }, [loadBalance, loadTeam]);

  return (
    <>
      <PageHeader title="账户" eyebrow="BILLING · 组织账户" dataLead="余额来自全量已入账快照；可用额度同时受账户、组织、成员和密钥限制。待审批充值到账后才可使用。" />
      <BentoGrid className="billing">
        <BalancePanel balance={balance} team={team} teamErr={teamErr} onRetryTeam={() => void loadTeam()} err={balanceErr} />
        <Readout member={member} balance={balance} team={team} teamErr={teamErr} onRetryTeam={() => void loadTeam()} />
        <TopupList reloadKey={topupsVersion} onLoaded={rememberTopups} />
        <TopupForm onRequested={() => { setTopupsVersion((version) => version + 1); void loadBalance(); }} />
        <Ledger topups={topups} />
      </BentoGrid>
    </>
  );
}

// ── balance (余额) ────────────────────────────────────────────────────────────────────

/** A failed read of the organisation's quotas, with a retry: what depends on them is not known until it succeeds. */
function TeamFailed({ err, onRetry }: { err: string; onRetry: () => void }) {
  return (
    <div className="note crit" role="alert">
      组织额度读取失败：{err}{" "}
      <Button size="sm" onClick={onRetry}>
        重试
      </Button>
    </div>
  );
}

function BalancePanel({
  balance,
  team,
  teamErr,
  onRetryTeam,
  err,
}: {
  balance: Balance | null;
  team: Budget[] | null;
  teamErr: string | null;
  onRetryTeam: () => void;
  err: string | null;
}) {
  const postpaid = balance?.billing_mode === "postpaid";
  const balanceOut = balance?.exhausted === true;
  // The lamp means "calls are accepted now": a used-up organisation quota refuses them too,
  // so until the quotas are read only a spent balance decides.
  const orgOut = team?.some((b) => b.exhausted) ?? false;
  const known = balanceOut || team !== null;
  const exhausted = balanceOut || orgOut;
  const stateText = balanceOut
    ? postpaid
      ? "已达信用额度，调用已停止"
      : "余额不足，调用已停止"
    : !known
      ? teamErr
        ? "组织额度未能读取，暂不能确认能否调用"
        : "正在读取组织额度…"
      : orgOut
        ? "组织额度已用尽，调用已停止"
        : "可正常调用";
  return (
    <Card role="region" aria-label="余额" className={span.d8}>
      <CardHeader number={1} title="余额" actions={balance && (
        <p className="balance-state text-caption" data-state={!known ? "unknown" : exhausted ? "exhausted" : "ok"}>
          <Lamp state={!known ? "pending" : exhausted ? "rejected" : "applied"}>{stateText}</Lamp>
        </p>
      )} />
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
      {balance && (
        <>
          {!known && teamErr && <TeamFailed err={teamErr} onRetry={onRetryTeam} />}
          <div className="entries grid gap-6">
            <Reading hero label="余额" value={<Money micro={balance.calculation.balance_micro_cny} />} />
            <div className="grid gap-5 tablet:grid-cols-3">
            <Reading label="信用额度" value={<Money micro={balance.calculation.credit_limit_micro_cny} />} />
            <Reading label="可用额度" value={<Money micro={balance.calculation.available_micro_cny} />} />
            <Reading label="待审批的充值" value={`${balance.pending_topups.toLocaleString("en-US")} 笔`} />
            </div>
          </div>
          <BalanceCalculation balance={balance} />
        </>
      )}
      {!balance && !err && <Loading />}
    </Card>
  );
}

/** 同一只读入账快照中的全量流水，不受下面流水分页或筛选影响。 */
function BalanceCalculation({ balance }: { balance: Balance }) {
  const f = balance.calculation;
  const values = [f.topups_micro_cny, f.usage_micro_cny, f.adjustments_micro_cny, f.balance_micro_cny].map(exactInteger);
  const [topups, usage, adjustments, total] = values;
  const matches = topups == null || usage == null || adjustments == null || total == null ? null : topups - usage + adjustments === total;
  return (
    <section aria-label="余额计算" className="grid gap-3 border-t border-ink/7 pt-4 text-body">
      <h3 className="text-stat font-medium">余额计算</h3>
      <p className="break-words tabular">累计到账 {moneyText(f.topups_micro_cny)} − 原始扣费 {moneyText(f.usage_micro_cny)} + 余额调整（{moneyText(f.adjustments_micro_cny)}）{matches ? " = " : "；账户余额 "}{moneyText(f.balance_micro_cny)}</p>
      {matches === false && <p className="note crit" role="alert">流水合计与账户余额不一致，请联系管理员核对。</p>}
      {matches === null && <p className="note crit" role="alert">余额计算金额未知，暂不能核对等式。</p>}
      {f.available_overflow ? <p className="note crit" role="alert">余额加信用额度超出支持范围，可用额度显示上限值，调用已按现有规则停止。</p> : (
        <p className="break-words tabular">余额 {moneyText(f.balance_micro_cny)} + 信用额度 {moneyText(f.credit_limit_micro_cny)} = 可用额度 {moneyText(f.available_micro_cny)}</p>
      )}
      <p className="hint">截至 {fmtBeijing(f.as_of)} 的已入账快照。待审批充值与尚未入账用量不计入；信用额度是透支上限，预算限制另行生效。</p>
    </section>
  );
}

// ── what can still be spent (可用额度) ────────────────────────────────────────────────────────────────

const LEVEL_LABEL: Record<QuotaLevel, string> = { organization: "组织", member: "成员", key: "密钥" };
const SCOPE_LEVEL = { team: "organization", member: "member", api_key: "key" } as const;

/** What one key's quota answer shows as stops: the levels that limit it, each at its smallest remaining amount. */
function stopsOf(q: KeyQuota): ReadoutStop[] {
  const least: Partial<Record<QuotaLevel, number>> = {};
  const lower = (level: QuotaLevel, micro: number) => {
    const amount = Number.isSafeInteger(micro) ? Math.max(0, micro) : Number.NaN;
    least[level] = Math.min(least[level] ?? amount, amount);
  };
  if (q.org) lower("organization", q.org.available_micro_cny);
  for (const f of q.quotas) lower(SCOPE_LEVEL[f.scope], f.remaining_micro_cny);
  return (["organization", "member", "key"] as const).flatMap((level) => {
    const remaining = least[level];
    return remaining === undefined ? [] : [{ level, label: LEVEL_LABEL[level], remainingMicroCny: remaining }];
  });
}

/** The key to read first: the viewer's own newest, else the organisation's newest. */
function defaultKey(items: TenantKey[], member: SessionMember): string | null {
  const newest = (list: TenantKey[]) => [...list].sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at))[0];
  const own = newest(items.filter((k) => k.owner?.id === member.id));
  return (own ?? newest(items))?.id ?? null;
}

function Readout({
  member,
  balance,
  team,
  teamErr,
  onRetryTeam,
}: {
  member: SessionMember;
  balance: Balance | null;
  team: Budget[] | null;
  teamErr: string | null;
  onRetryTeam: () => void;
}) {
  const manager = member.role === "owner" || member.role === "admin";
  const [items, setItems] = useState<TenantKey[] | null>(manager ? null : []);
  const [keyId, setKeyId] = useState<string | null>(null);
  const [quota, setQuota] = useState<KeyQuota | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!manager) return;
    let live = true;
    keyApi.list().then(
      (list) => {
        if (!live) return;
        setItems(list);
        setKeyId(defaultKey(list, member));
      },
      (e: unknown) => {
        if (!live) return;
        setItems([]);
        setErr(failure(e));
      },
    );
    return () => {
      live = false;
    };
  }, [manager, member]);

  // The figures move as calls are made, so they are read again whenever the
  // key changes or the balance does.
  const available = balance?.calculation.available_micro_cny;
  useEffect(() => {
    if (keyId === null) {
      setQuota(null);
      return;
    }
    let live = true;
    billing.keyQuota(keyId).then(
      (q) => {
        if (!live) return;
        setQuota(q);
        setErr(null);
      },
      (e: unknown) => live && setErr(failure(e)),
    );
    return () => {
      live = false;
    };
  }, [keyId, available]);

  if (items === null || (balance === null && keyId === null)) {
    return (
      <Section number={2} title="可用额度" tone="inverse" className={cn(span.d4, "tablet:col-span-12")}>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        {!err && <Loading />}
      </Section>
    );
  }

  let view: { effective: number | string | bigint | null; stops: ReadoutStop[]; binding: QuotaLevel | null } | null = null;
  if (keyId !== null && quota && quota.key_id === keyId) {
    view = { effective: quota.effective_remaining_micro_cny, stops: stopsOf(quota), binding: quota.binding_level };
  } else if (keyId === null && balance && team !== null) {
    // No key to read (a billing member's case): the smaller of the organisation's
    // balance and its smallest remaining quota, once the quotas are read.
    const amounts = [balance.calculation.available_micro_cny, ...team.map((b) => b.remaining_micro_cny)].map(exactInteger);
    const knownAmounts = amounts.filter((value): value is bigint => value !== null);
    const left = knownAmounts.length !== amounts.length ? Number.NaN : knownAmounts.reduce((least, value) => {
      const nonnegative = value < 0n ? 0n : value;
      return nonnegative < least ? nonnegative : least;
    }, knownAmounts[0]! < 0n ? 0n : knownAmounts[0]!);
    view = { effective: left, stops: [{ level: "organization", label: "组织", remainingMicroCny: left }], binding: "organization" };
  }

  const picker =
    manager && items.length > 0 ? (
      <div className="w-full">
        <Field label="密钥">
          <select value={keyId ?? ""} onChange={(e) => setKeyId(e.target.value)}>
            {items.map((k) => (
              <option key={k.id} value={k.id}>
                {k.label}
              </option>
            ))}
          </select>
        </Field>
      </div>
    ) : null;

  if (!view) {
    // A billing member's figure waits on the organisation's quotas.
    const quotaErr = keyId === null && team === null ? teamErr : null;
    return (
      <Section number={2} title="可用额度" tone="inverse" className={cn(span.d4, "tablet:col-span-12")}>
        {picker}
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        {quotaErr && <TeamFailed err={quotaErr} onRetry={onRetryTeam} />}
        {!err && !quotaErr && <Loading />}
      </Section>
    );
  }
  return (
    <BalanceReadout effectiveMicroCny={view.effective} stops={view.stops} binding={view.binding} picker={picker}>
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
    </BalanceReadout>
  );
}

// ── top-up request (申请充值) ────────────────────────────────────────────────────────────────

type TopupField = "amount" | "note" | "reference";

const TOPUP_PATH: Record<string, TopupField> = { "/amount_cny": "amount", "/note": "note", "/reference": "reference" };

function TopupForm({ onRequested }: { onRequested: () => void }) {
  const [amount, setAmount] = useState("");
  const [note, setNote] = useState("");
  const [reference, setReference] = useState("");
  const [errors, setErrors] = useState<Partial<Record<TopupField, string>>>({});
  const [general, setGeneral] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const toast = useToast();

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setGeneral(null);
    const amountText = amount.trim();
    if (!isCnyAmount(amountText, { allowZero: false })) {
      setErrors({ amount: "请输入金额，最多两位小数" });
      focusRefusal();
      return;
    }
    setErrors({});
    setBusy(true);
    try {
      await billing.requestTopup({
        amount_cny: amountText,
        ...(note.trim() ? { note: note.trim() } : {}),
        ...(reference.trim() ? { reference: reference.trim() } : {}),
      });
      setAmount("");
      setNote("");
      setReference("");
      toast("已提交申请");
      onRequested();
    } catch (err) {
      if (err instanceof ApiError && err.code === "validation_failed") {
        const next: Partial<Record<TopupField, string>> = {};
        const rest: string[] = [];
        for (const d of err.details) {
          const field = TOPUP_PATH[d.path];
          if (field) next[field] = d.message;
          else rest.push(d.message);
        }
        setErrors(next);
        if (Object.keys(next).length === 0 || rest.length > 0) setGeneral(rest.length > 0 ? rest.join("；") : formMessageFor(err));
        if (Object.keys(next).length > 0) focusRefusal();
      } else {
        setGeneral(failure(err));
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <Section number={4} title="申请充值" className={span.d5Wide}>
      <p className="text-caption text-muted">提交申请后由平台审批，审批通过才计入账户余额。</p>
      <form className="grid gap-4" ref={form} onSubmit={submit} noValidate>
        <Field label="充值金额（元）" error={errors.amount} hint="最多两位小数">
          {(aria) => (
            <input inputMode="decimal" autoComplete="off" value={amount} onChange={(e) => setAmount(e.target.value)} {...aria} />
          )}
        </Field>
        <Field label="付款凭证号" error={errors.reference}>
          {(aria) => (
            <input autoComplete="off" maxLength={REFERENCE_MAX} value={reference} onChange={(e) => setReference(e.target.value)} {...aria} />
          )}
        </Field>
        <Field label="备注" wide error={errors.note}>
          {(aria) => <input autoComplete="off" maxLength={NOTE_MAX} value={note} onChange={(e) => setNote(e.target.value)} {...aria} />}
        </Field>
        {general && (
          <div className="note crit col-span-full" role="alert">
            {general}
          </div>
        )}
        <div className="col-span-full mt-2 flex justify-end gap-2 border-t border-ink/7 pt-4">
          <Button kind="primary" type="submit" busy={busy} busyLabel="提交中…">
            提交申请
          </Button>
        </div>
      </form>
    </Section>
  );
}

/** 游标只从成功响应延续；本地搜索保留已加载页，下一页仍可继续读取更早记录。 */
function useBillingRecords<T>({
  fetchPage,
  keyOf,
  searchText,
  resetKey,
  enabled = true,
  onLoaded,
}: {
  fetchPage: (before: number | undefined, limit: RecordPageSize) => Promise<Page<T>>;
  keyOf: (item: T) => string | number;
  searchText: (item: T) => string | readonly string[];
  resetKey: string;
  enabled?: boolean;
  onLoaded?: (items: T[]) => void;
}) {
  const [pages, setPages] = useState<Page<T>[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const generation = useRef(0);
  const loading = useRef(false);
  const fetchRef = useRef(fetchPage);
  const loadedRef = useRef(onLoaded);
  fetchRef.current = fetchPage;
  loadedRef.current = onLoaded;
  const items = pages?.flatMap((page) => page.items) ?? [];
  const table = useRecordTable({ items, searchText, resetKey, initialPageSize: 25 });
  const next = pages?.at(-1)?.next_before ?? null;
  const queryRef = useRef(table.query);
  queryRef.current = table.query;

  useEffect(() => {
    const mine = ++generation.current;
    setPages(enabled ? null : []);
    setErr(null);
    table.setPage(1);
    loading.current = enabled;
    setBusy(enabled);
    if (enabled) {
      fetchRef.current(undefined, table.pageSize).then(
        (page) => {
          if (mine !== generation.current) return;
          setPages([page]);
          loadedRef.current?.(page.items);
        },
        (error: unknown) => {
          if (mine !== generation.current) return;
          setPages([]);
          setErr(failure(error));
        },
      ).finally(() => {
        if (mine !== generation.current) return;
        loading.current = false;
        setBusy(false);
      });
    }
    return () => { generation.current += 1; };
    // 业务筛选和页大小才更换游标链；回调及本地搜索不改变服务端查询范围。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [resetKey, enabled, table.pageSize]);

  async function changePage(page: number) {
    if (loading.current) return;
    if (page <= table.pageCount) {
      table.setPage(page);
      return;
    }
    if (page !== table.page + 1 || next === null) return;
    const mine = generation.current;
    const queryAtLoad = table.query;
    loading.current = true;
    setBusy(true);
    setErr(null);
    try {
      const answer = await fetchRef.current(next, table.pageSize);
      if (mine !== generation.current) return;
      setPages((current) => {
        const seen = new Set((current ?? []).flatMap((entry) => entry.items.map(keyOf)));
        return [...(current ?? []), { ...answer, items: answer.items.filter((item) => !seen.has(keyOf(item))) }];
      });
      loadedRef.current?.(answer.items);
      if (queryRef.current === queryAtLoad) table.setPage(table.filteredCount >= table.page * table.pageSize ? page : table.page);
    } catch (error) {
      if (mine === generation.current) setErr(failure(error));
    } finally {
      if (mine === generation.current) {
        loading.current = false;
        setBusy(false);
      }
    }
  }

  return { ...table, loaded: pages !== null, loadedCount: items.length, err, busy, hasNext: table.page < table.pageCount || next !== null, changePage };
}

// ── top-up history (充值记录) ────────────────────────────────────────────────────────────────

const TOPUP_STATE: Record<Topup["status"], { text: string; lamp: LampState }> = {
  pending: { text: "待审批", lamp: "pending" },
  approved: { text: "已到账", lamp: "applied" },
  rejected: { text: "已驳回", lamp: "rejected" },
};

/** A top-up's explanation: why it was refused, why the platform credited it, or the requester's own note. */
function explanation(t: Topup): string | null {
  if (t.status === "rejected") return t.reject_reason;
  if (t.origin === "operator") return t.credit_reason;
  return t.note;
}

function TopupList({ reloadKey, onLoaded }: { reloadKey: number; onLoaded: (items: Topup[]) => void }) {
  const [status, setStatus] = useState<"" | Topup["status"]>("");
  const records = useBillingRecords({
    fetchPage: (before, limit) => billing.topups({ ...(status ? { status } : {}), before, limit }),
    keyOf: (item) => item.id,
    searchText: (item) => [item.origin === "tenant" ? item.reference ?? "" : "", item.requested_by?.display_name ?? ""],
    resetKey: `${reloadKey}/${status}`,
    onLoaded,
  });
  const { rows: items, err } = records;
  return (
    <Section number={3} title="充值记录" className={span.d7}>
      <p className="text-caption text-muted">按时间读取申请与平台入账记录，搜索仅作用于已加载记录；待审批总数见账户余额。</p>
      <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索已加载充值记录", placeholder: "付款凭证号、提交人" }}>
        <Field label="充值状态">
          <select value={status} onChange={(event) => setStatus(event.target.value as "" | Topup["status"])}>
            <option value="">全部</option>
            <option value="pending">待审批</option>
            <option value="approved">已到账</option>
            <option value="rejected">已驳回</option>
          </select>
        </Field>
      </RecordTableToolbar>
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
      {!records.loaded && !err && <Loading />}
      {records.loaded && items.length === 0 && !err && <EmptyState text={records.query ? "已加载记录中没有匹配的充值记录" : "所选范围内没有充值记录"} mark="route" />}
      {items.length > 0 && (
        <DataTable label="充值记录">
          <thead>
            <tr>
              <th className="num">金额</th>
              <th>状态</th>
              <th>来源</th>
              <th>提交人</th>
              <th>提交时间</th>
              <th>处理时间</th>
              <th>说明</th>
            </tr>
          </thead>
          <tbody>
            {items.map((t) => {
              const state = TOPUP_STATE[t.status] ?? { text: t.status, lamp: "pending" as LampState };
              const why = explanation(t);
              return (
                <tr key={t.id}>
                  <td className="num">
                    <Money micro={t.amount_micro_cny} />
                  </td>
                  <td>
                    <Lamp state={state.lamp}>{state.text}</Lamp>
                  </td>
                  <td>{t.origin === "operator" ? "平台入账" : "本组织申请"}</td>
                  <td>{t.requested_by ? (t.requested_by.display_name ?? "—") : "平台"}</td>
                  {/* Below 1800px a time takes two lines, its date over its clock, so the list fits beside the form. */}
                  <td className="tabular max-[1799px]:whitespace-normal">
                    <span className="block max-[1799px]:max-w-[6.5rem]">{fmtBeijing(t.requested_at)}</span>
                  </td>
                  <td className="tabular max-[1799px]:whitespace-normal">
                    <span className="block max-[1799px]:max-w-[6.5rem]">{t.decided_at ? fmtBeijing(t.decided_at) : "—"}</span>
                  </td>
                  <td>
                    {why ? (
                      <span className="trunc" title={why}>
                        {why}
                      </span>
                    ) : (
                      "—"
                    )}
                    {t.reference && t.origin === "tenant" && <span className="hint cell-note">付款凭证号 {t.reference}</span>}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </DataTable>
      )}
      <RecordTablePager label="充值记录分页" page={records.page} pageSize={records.pageSize} onPageChange={(page) => void records.changePage(page)} onPageSizeChange={records.setPageSize} loadedCount={records.loadedCount} hasNext={records.hasNext} busy={records.busy} />
    </Section>
  );
}

// ── ledger (余额流水) and period summary (按期汇总) ────────────────────────────────────────────────────

const KIND_LABEL: Record<LedgerKind, string> = { usage: "扣费", topup: "充值", adjustment: "调整" };

/** The instants a date filter names (either may be left empty), or null when the end is before the start. */
function rangeOf(fromDate: string, toDate: string): { from?: string; to?: string } | null {
  if (fromDate && toDate && toDate < fromDate) return null;
  return {
    ...(fromDate ? { from: beijingRange(fromDate, fromDate).from } : {}),
    ...(toDate ? { to: beijingRange(toDate, toDate).to } : {}),
  };
}

function Ledger({ topups }: { topups: Topup[] | null }) {
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [kind, setKind] = useState<"" | LedgerKind>("");
  const range = rangeOf(fromDate, toDate);
  const records = useBillingRecords({
    fetchPage: (before, limit) => formulaLedger({ ...range, ...(kind ? { kind } : {}), before, limit }),
    keyOf: (item) => item.seq,
    searchText: (item) => [item.event_id ?? "", item.model?.name ?? "", KIND_LABEL[item.kind]],
    resetKey: `${fromDate}/${toDate}/${kind}`,
    enabled: range !== null,
  });
  const { rows, err } = records;
  const [extraTopups, setExtraTopups] = useState<Topup[]>([]);
  const originScope = `${fromDate}/${toDate}/${kind}/${records.pageSize}/${records.page}/${records.query}`;
  const [originFailures, setOriginFailures] = useState<{ scope: string; items: Record<string, string> }>({ scope: "", items: {} });
  const failedOrigins = originFailures.scope === originScope ? originFailures.items : {};
  const originOf = new Map([...(topups ?? []), ...extraTopups].map((item) => [item.id, item.origin]));
  const missingTopups = [...new Set(rows.flatMap((row) => row.topup_id && !originOf.has(row.topup_id) && !failedOrigins[row.topup_id] ? [row.topup_id] : []))];
  const missingKey = missingTopups.join(",");
  const originErr = rows.flatMap((row) => row.topup_id && failedOrigins[row.topup_id] ? [failedOrigins[row.topup_id]] : [])[0] ?? null;
  useEffect(() => {
    if (!missingKey) return;
    let live = true;
    void Promise.allSettled(missingKey.split(",").map(async (id) => (await call<Topup>("GET", `/billing/topups/${encodeURIComponent(id)}`)).data)).then((answers) => {
      if (!live) return;
      const loaded = answers.flatMap((answer) => answer.status === "fulfilled" ? [answer.value] : []);
      setExtraTopups((current) => [...new Map([...current, ...loaded].map((item) => [item.id, item])).values()]);
      const ids = missingKey.split(",");
      const failures: Record<string, string> = {};
      answers.forEach((answer, index) => {
        const id = ids[index];
        if (id && answer.status === "rejected") failures[id] = failure(answer.reason);
      });
      if (Object.keys(failures).length > 0) {
        setOriginFailures((current) => ({ scope: originScope, items: { ...(current.scope === originScope ? current.items : {}), ...failures } }));
      }
    });
    return () => { live = false; };
  }, [missingKey, originScope]);
  const explain = (r: LedgerItem): string => {
    if (r.kind === "usage") return r.model?.name ?? "—";
    if (r.kind === "adjustment") return "重新计价调整";
    if (!r.topup_id || !originOf.has(r.topup_id)) return "充值（来源未读取）";
    return originOf.get(r.topup_id) === "operator" ? "平台入账" : "充值";
  };

  return (
    <>
      <Section number={5} title="余额流水">
        <RecordTableToolbar search={{ value: records.query, onChange: records.setQuery, label: "搜索已加载余额流水", placeholder: "模型、事件 ID、类型" }}>
          <Field label="开始日期" error={range ? null : "结束日期不能早于开始日期"}>
            <input type="date" value={fromDate} max={toDate || undefined} onChange={(e) => setFromDate(e.target.value)} />
          </Field>
          <Field label="结束日期">
            <input type="date" value={toDate} min={fromDate || undefined} onChange={(e) => setToDate(e.target.value)} />
          </Field>
          <Field label="类型">
            <select value={kind} onChange={(e) => setKind(e.target.value as "" | LedgerKind)}>
              <option value="">全部</option>
              <option value="usage">扣费</option>
              <option value="topup">充值</option>
              <option value="adjustment">调整</option>
            </select>
          </Field>
        </RecordTableToolbar>
        <p className="hint">搜索仅作用于已加载记录；日期和类型筛选由服务端执行，金额与余额快照不随分页重算。</p>
        {originErr && <p className="note crit" role="alert">入账来源读取失败：{originErr}</p>}
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        {!records.loaded && range && <Loading />}
        {records.loaded && rows.length === 0 && !err && <EmptyState text={records.query ? "已加载记录中没有匹配的流水" : "所选范围内没有流水"} mark="route" />}
        {rows.length > 0 && (
          <DataTable label="余额流水">
            <thead>
              <tr>
                <th>时间</th>
                <th>类型</th>
                <th className="num">金额</th>
                <th className="num">余额</th>
                <th>本笔余额计算</th>
                <th>说明</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.seq}>
                  <td className="tabular">{fmtBeijing(r.posted_at)}</td>
                  <td>{KIND_LABEL[r.kind] ?? r.kind}</td>
                  <td className="num">
                    <Money micro={r.calculation.amount_micro_cny} />
                  </td>
                  <td className="num">
                    <Money micro={r.calculation.after_micro_cny} />
                  </td>
                  <td className="tabular">
                    {fmtMicroCny(r.calculation.before_micro_cny)} +（{fmtMicroCny(r.calculation.amount_micro_cny)}）= <Money micro={r.calculation.after_micro_cny} />
                  </td>
                  <td>
                    <span className="trunc" title={explain(r)}>
                      {explain(r)}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </DataTable>
        )}
        <RecordTablePager label="余额流水分页" page={records.page} pageSize={records.pageSize} onPageChange={(page) => void records.changePage(page)} onPageSizeChange={records.setPageSize} loadedCount={records.loadedCount} hasNext={records.hasNext} busy={records.busy} />
      </Section>
      <Summary range={range} fromDate={fromDate} toDate={toDate} />
    </>
  );
}

/** 图表数据与表格行分开投影，显示金额保留原始精度。 */
export function ledgerSummaryChartPoints(rows: readonly LedgerSummaryRow[], group: "day" | "month") {
  const chartRows = [...rows].sort((a, b) => Date.parse(a.period_start) - Date.parse(b.period_start));
  const geometry = normalizeExactValues(chartRows.map((row) => row.usage_micro_cny));
  return chartRows.map((row, index) => ({ label: periodLabel(row.period_start, group), value: geometry[index] ?? null, displayValue: moneyText(row.usage_micro_cny) }));
}

function Summary({ range, fromDate, toDate }: { range: { from?: string; to?: string } | null; fromDate: string; toDate: string }) {
  const [group, setGroup] = useState<"day" | "month">("day");
  const [rows, setRows] = useState<LedgerSummaryRow[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const ticket = useRef(0);
  const chosen = fromDate !== "" && toDate !== "";

  useEffect(() => {
    const mine = ++ticket.current;
    if (!range) {
      setRows([]);
      setErr(null);
      return;
    }
    // A summary needs both ends: the dates left empty read as the default span.
    const fallback = defaultCalendarRange(group);
    setRows(null);
    billing
      .ledgerSummary({ group, from: range.from ?? fallback.from, to: range.to ?? fallback.to })
      .then(
        (items) => {
          if (mine !== ticket.current) return;
          setRows([...items].reverse());
          setErr(null);
        },
        (e: unknown) => {
          if (mine !== ticket.current) return;
          setRows([]);
          setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
        },
      );
    // `range` is derived from the two dates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [group, fromDate, toDate]);

  const table = useRecordTable({ items: rows ?? [], searchText: (row) => periodLabel(row.period_start, group), resetKey: `${group}/${fromDate}/${toDate}` });
  const points = ledgerSummaryChartPoints(rows ?? [], group);

  return (
    <Section
      number={6}
      title="按期汇总"
      actions={
        <label className="flex items-center gap-2">
          <span className="f-label">汇总方式</span>
          <select className="min-h-11 w-auto pr-8 text-body" value={group} onChange={(e) => setGroup(e.target.value as "day" | "month")}>
            <option value="day">按天</option>
            <option value="month">按月</option>
          </select>
        </label>
      }
    >
      <p className="hint">
        {chosen ? "使用余额流水的日期范围" : group === "day" ? "未选日期时汇总最近 30 天" : "未选日期时汇总最近 12 个月"}；时间采用 UTC+8。
      </p>
      <RecordTableToolbar search={{ value: table.query, onChange: table.setQuery, label: "搜索汇总期间", placeholder: "期间日期" }} />
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
      {rows === null && range && <Loading />}
      {rows !== null && rows.length === 0 && !err && <EmptyState text="所选范围内没有汇总" mark="route" />}
      {rows !== null && rows.length > 0 && <>
        <div className="grid gap-3 border-b border-ink/7 pb-5">
          <h3 className="text-stat font-medium">期间扣费 · 入账口径</h3>
          <p className="text-caption text-muted">与下表使用同一汇总范围，仅绘制已返回期间；未入账调用不计入，缺失期间不补零。</p>
          <TrendChart points={points} accessibleLabel="所选期间的已入账扣费，各期间精确金额可从下方按期汇总表查看" />
        </div>
        <DataTable label="按期汇总">
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
            {table.rows.map((r) => (
              <tr key={r.period_start}>
                <td className="tabular">{periodLabel(r.period_start, group)}</td>
                <td className="num">
                  <Money micro={r.usage_micro_cny} />
                </td>
                <td className="num">
                  <Money micro={r.topups_micro_cny} />
                </td>
                <td className="num">
                  <Money micro={r.adjustments_micro_cny} />
                </td>
                <td className="num">
                  <Money micro={r.closing_balance_micro_cny} />
                </td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      </>}
      {rows !== null && rows.length > 0 && table.filteredCount === 0 && <EmptyState text="没有匹配的汇总期间" mark="route" />}
      <RecordTablePager label="按期汇总分页" page={table.page} pageSize={table.pageSize} onPageChange={table.setPage} onPageSizeChange={table.setPageSize} total={table.filteredCount} busy={rows === null} />
    </Section>
  );
}
