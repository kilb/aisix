import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  ApiError,
  billing,
  budgets as budgetApi,
  keys as keyApi,
  type Balance,
  type Budget,
  type KeyQuota,
  type LedgerItem,
  type LedgerKind,
  type LedgerSummaryRow,
  type QuotaLevel,
  type SessionMember,
  type TenantKey,
  type Topup,
} from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtUtc, isCnyAmount, periodLabel, todayUtc, utcRange } from "./lib/money";
import { BalanceReadout, Money, type ReadoutStop } from "./ui/BalanceReadout";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field, useRefusalFocus } from "./ui/Field";
import { Lamp, type LampState } from "./ui/Lamp";
import { useToast } from "./ui/Toast";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** Longest a top-up's note and reference may be: the control plane's own limits. */
const NOTE_MAX = 500;
const REFERENCE_MAX = 128;
/** Top-ups listed at most: the control plane's page maximum. */
const TOPUPS_SHOWN = 500;

/** A labelled reading in a panel, as the organisation page shows its own. */
function Reading({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="entry">
      <span className="lab">{label}</span>
      <span className="val">{value}</span>
    </div>
  );
}

/** A panel that is a region named by its heading. */
function Section({ title, children, list = false }: { title: string; children: ReactNode; list?: boolean }) {
  return (
    <div className={list ? "panel list" : "panel"} role="region" aria-label={title}>
      {list ? (
        <div className="panel-head">
          <h2>{title}</h2>
        </div>
      ) : (
        <h2>{title}</h2>
      )}
      {children}
    </div>
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
  const [topupsErr, setTopupsErr] = useState<string | null>(null);

  const loadBalance = useCallback(async () => {
    try {
      setBalance(await billing.balance());
      setBalanceErr(null);
    } catch (e) {
      setBalanceErr(failure(e));
    }
  }, []);
  const loadTopups = useCallback(async () => {
    try {
      setTopups((await billing.topups({ limit: TOPUPS_SHOWN })).items);
      setTopupsErr(null);
    } catch (e) {
      setTopupsErr(failure(e));
    }
  }, []);

  // The organisation's own quotas, which owner, admin and billing may read. A
  // quota that is used up refuses calls while the balance is still positive,
  // so the lamp and the billing member's readout both need them. `null` until
  // they are read: a read that failed is not an organisation without quotas,
  // so nothing that depends on them is claimed until a read succeeds.
  const [team, setTeam] = useState<Budget[] | null>(null);
  const [teamErr, setTeamErr] = useState<string | null>(null);
  const loadTeam = useCallback(async () => {
    try {
      setTeam((await budgetApi.list()).filter((b) => b.scope === "team"));
      setTeamErr(null);
    } catch (e) {
      setTeam(null);
      setTeamErr(failure(e));
    }
  }, []);

  useEffect(() => {
    void loadBalance();
    void loadTopups();
    void loadTeam();
  }, [loadBalance, loadTopups, loadTeam]);

  return (
    <div className="stack billing">
      <div className="deck billing-top">
        <BalancePanel balance={balance} team={team} teamErr={teamErr} onRetryTeam={() => void loadTeam()} err={balanceErr} />
        <Readout member={member} balance={balance} team={team} teamErr={teamErr} onRetryTeam={() => void loadTeam()} />
      </div>
      <TopupForm
        onRequested={() => {
          void loadTopups();
          void loadBalance();
        }}
      />
      <TopupList items={topups} err={topupsErr} />
      <Ledger topups={topups} />
    </div>
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
    <Section title="余额">
      {err && (
        <div className="note crit" role="alert">
          {err}
        </div>
      )}
      {balance && (
        <>
          <p className="balance-state" data-state={!known ? "unknown" : exhausted ? "exhausted" : "ok"}>
            <Lamp state={!known ? "pending" : exhausted ? "rejected" : "applied"}>{stateText}</Lamp>
          </p>
          {!known && teamErr && <TeamFailed err={teamErr} onRetry={onRetryTeam} />}
          <div className="entries">
            <Reading label="余额" value={<Money micro={balance.balance_micro_cny} />} />
            <Reading label="信用额度" value={<Money micro={balance.credit_limit_micro_cny} />} />
            <Reading label="可用额度" value={<Money micro={balance.available_micro_cny} />} />
            <Reading label="待审批的充值" value={`${balance.pending_topups} 笔`} />
          </div>
        </>
      )}
      {!balance && !err && <Loading />}
    </Section>
  );
}

// ── what can still be spent (可用额度) ────────────────────────────────────────────────────────────────

const LEVEL_LABEL: Record<QuotaLevel, string> = { organization: "组织", member: "成员", key: "密钥" };
const SCOPE_LEVEL = { team: "organization", member: "member", api_key: "key" } as const;

/** What one key's quota answer shows as stops: the levels that limit it, each at its smallest remaining amount. */
function stopsOf(q: KeyQuota): ReadoutStop[] {
  const least: Partial<Record<QuotaLevel, number>> = {};
  const lower = (level: QuotaLevel, micro: number) => {
    const amount = Math.max(0, micro);
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
  const available = balance?.available_micro_cny;
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
      <Section title="可用额度">
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        {!err && <Loading />}
      </Section>
    );
  }

  let view: { effective: number | null; stops: ReadoutStop[]; binding: QuotaLevel | null } | null = null;
  if (keyId !== null && quota && quota.key_id === keyId) {
    view = { effective: quota.effective_remaining_micro_cny, stops: stopsOf(quota), binding: quota.binding_level };
  } else if (keyId === null && balance && team !== null) {
    // No key to read (a billing member's case): the smaller of the organisation's
    // balance and its smallest remaining quota, once the quotas are read.
    const left = Math.min(Math.max(0, balance.available_micro_cny), ...team.map((b) => Math.max(0, b.remaining_micro_cny)));
    view = { effective: left, stops: [{ level: "organization", label: "组织", remainingMicroCny: left }], binding: "organization" };
  }

  const picker =
    manager && items.length > 0 ? (
      <div className="readout-pick">
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
      <Section title="可用额度">
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
    <Section title="申请充值">
      <form className="form cols" ref={form} onSubmit={submit} noValidate>
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
          <div className="note crit" role="alert">
            {general}
          </div>
        )}
        <div className="form-actions">
          <Button kind="primary" type="submit" busy={busy} busyLabel="提交中…">
            提交申请
          </Button>
        </div>
      </form>
    </Section>
  );
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

function TopupList({ items, err }: { items: Topup[] | null; err: string | null }) {
  return (
    <Section title="充值记录" list>
      {err && (
        <div className="panel-notes">
          <div className="note crit" role="alert">
            {err}
          </div>
        </div>
      )}
      {items === null && !err && <Loading />}
      {items !== null && items.length === 0 && <EmptyState text="还没有充值记录，在上方填写金额即可提交申请" mark="route" />}
      {items !== null && items.length > 0 && (
        <DataTable label="充值记录">
          <thead>
            <tr>
              <th className="num">金额</th>
              <th>状态</th>
              <th>来源</th>
              <th>提交人</th>
              <th>提交时间（UTC）</th>
              <th>处理时间（UTC）</th>
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
                  <td className="tabular">{fmtUtc(t.requested_at)}</td>
                  <td className="tabular">{t.decided_at ? fmtUtc(t.decided_at) : "—"}</td>
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
    </Section>
  );
}

// ── ledger (余额流水) and period summary (按期汇总) ────────────────────────────────────────────────────

const KIND_LABEL: Record<LedgerKind, string> = { usage: "扣费", topup: "充值", adjustment: "调整" };

/** The summary's range when no date is chosen: the last 30 days by day, the last 12 months by month. */
function defaultRange(group: "day" | "month"): { from: string; to: string } {
  const now = new Date();
  const today = todayUtc(now);
  const start =
    group === "day"
      ? new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() - 29))
      : new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() - 11, 1));
  return utcRange(start.toISOString().slice(0, 10), today);
}

/** The instants a date filter names (either may be left empty), or null when the end is before the start. */
function rangeOf(fromDate: string, toDate: string): { from?: string; to?: string } | null {
  if (fromDate && toDate && toDate < fromDate) return null;
  return {
    ...(fromDate ? { from: utcRange(fromDate, fromDate).from } : {}),
    ...(toDate ? { to: utcRange(toDate, toDate).to } : {}),
  };
}

function Ledger({ topups }: { topups: Topup[] | null }) {
  const [fromDate, setFromDate] = useState("");
  const [toDate, setToDate] = useState("");
  const [kind, setKind] = useState<"" | LedgerKind>("");
  const [rows, setRows] = useState<LedgerItem[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [more, setMore] = useState(false);
  const range = rangeOf(fromDate, toDate);
  // The latest first-page request: an answer to an older filter is dropped,
  // and so is a later page of one.
  const ticket = useRef(0);

  useEffect(() => {
    const mine = ++ticket.current;
    // A continuation of the old filter is not this one's: its busy state and
    // its cursor go with it, so no next page is asked for until this
    // filter's first page names one.
    setMore(false);
    setNext(null);
    // An end before the start names nothing: the field says so, and no row shows.
    if (!range) {
      setRows([]);
      setErr(null);
      return;
    }
    setRows(null);
    billing.ledger({ ...range, ...(kind ? { kind } : {}) }).then(
      (p) => {
        if (mine !== ticket.current) return;
        setRows(p.items);
        setNext(p.next_before);
        setErr(null);
      },
      (e: unknown) => {
        if (mine !== ticket.current) return;
        setRows([]);
        setNext(null);
        setErr(failure(e));
      },
    );
    // `range` is derived from the two dates.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fromDate, toDate, kind]);

  async function loadMore() {
    if (!range || next === null) return;
    const mine = ticket.current;
    setMore(true);
    try {
      const p = await billing.ledger({ ...range, ...(kind ? { kind } : {}), before: next });
      if (mine !== ticket.current) return;
      // A page never repeats a seq, but a double click must not either.
      setRows((cur) => {
        const seen = new Set((cur ?? []).map((r) => r.seq));
        return [...(cur ?? []), ...p.items.filter((r) => !seen.has(r.seq))];
      });
      setNext(p.next_before);
    } catch (e) {
      if (mine === ticket.current) setErr(failure(e));
    } finally {
      if (mine === ticket.current) setMore(false);
    }
  }

  const originOf = new Map((topups ?? []).map((t) => [t.id, t.origin]));
  const explain = (r: LedgerItem): string => {
    if (r.kind === "usage") return r.model?.name ?? "—";
    if (r.kind === "adjustment") return "重新计价调整";
    return r.topup_id && originOf.get(r.topup_id) === "operator" ? "平台入账" : "充值";
  };

  return (
    <>
      <Section title="余额流水" list>
        <div className="panel-notes">
          <div className="filters">
            <Field label="开始日期（UTC）" error={range ? null : "结束日期不能早于开始日期"}>
              <input type="date" value={fromDate} max={toDate || undefined} onChange={(e) => setFromDate(e.target.value)} />
            </Field>
            <Field label="结束日期（UTC）">
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
          </div>
          {err && (
            <div className="note crit" role="alert">
              {err}
            </div>
          )}
        </div>
        {rows === null && range && <Loading />}
        {rows !== null && rows.length === 0 && !err && <EmptyState text="所选范围内没有流水" mark="route" />}
        {rows !== null && rows.length > 0 && (
          <DataTable label="余额流水">
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
              {rows.map((r) => (
                <tr key={r.seq}>
                  <td className="tabular">{fmtUtc(r.posted_at)}</td>
                  <td>{KIND_LABEL[r.kind] ?? r.kind}</td>
                  <td className="num">
                    <Money micro={r.amount_micro_cny} />
                  </td>
                  <td className="num">
                    <Money micro={r.balance_after_micro_cny} />
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
        {next !== null && (
          <div className="panel-more">
            <Button busy={more} busyLabel="加载中…" onClick={() => void loadMore()}>
              加载更多
            </Button>
          </div>
        )}
      </Section>
      <Summary range={range} fromDate={fromDate} toDate={toDate} />
    </>
  );
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
    const fallback = defaultRange(group);
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

  return (
    <Section title="按期汇总" list>
      <div className="panel-notes">
        <div className="filters">
          <Field label="汇总方式">
            <select value={group} onChange={(e) => setGroup(e.target.value as "day" | "month")}>
              <option value="day">按天</option>
              <option value="month">按月</option>
            </select>
          </Field>
        </div>
        <p className="hint">
          {chosen ? "使用余额流水的日期范围（UTC）" : group === "day" ? "未选日期时汇总最近 30 天（UTC）" : "未选日期时汇总最近 12 个月（UTC）"}
        </p>
        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
      </div>
      {rows === null && range && <Loading />}
      {rows !== null && rows.length === 0 && !err && <EmptyState text="所选范围内没有汇总" mark="route" />}
      {rows !== null && rows.length > 0 && (
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
            {rows.map((r) => (
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
      )}
    </Section>
  );
}
