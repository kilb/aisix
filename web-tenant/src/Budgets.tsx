import { useCallback, useEffect, useState, type ReactNode } from "react";
import {
  ApiError,
  billing,
  budgets as budgetApi,
  keys as keyApi,
  members as memberApi,
  type Budget,
  type KeyQuota,
  type Member,
  type NewBudget,
  type QuotaLevel,
  type SessionMember,
  type TenantKey,
} from "./lib/api";
import { BudgetAccountAmounts, type BudgetAccountBalance } from "./BudgetAccountAmounts";
import { formulaBalance } from "./lib/formulaApi";
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtUtc, isCnyAmount, microCnyToInput } from "./lib/money";
import { Money } from "./ui/BalanceReadout";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field, useRefusalFocus } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { useToast } from "./ui/Toast";
import { TwoStep } from "./ui/TwoStep";
import { BentoGrid, Card, CardFooter, CardHeader, CardMessage, Metric, PageHeader, span } from "./components/page";
import { BudgetProgress } from "./components/charts";
import { exactInteger, exactRatio } from "./lib/chartFormat";
import { cn } from "./lib/cn";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

const OVER_PARENT = "超过上级额度时以先用尽者为准";

const SET_BY: Record<Budget["set_by"], string> = { operator: "平台", org_admin: "组织管理员", member: "成员" };
const KIND: Record<Budget["kind"], string> = { periodic: "按周期", cumulative: "一次性额度" };
const PERIOD: Record<NonNullable<Budget["period"]>, string> = { day: "每天", week: "每周（周一起）", month: "每月" };
const LEVEL: Record<QuotaLevel, string> = { organization: "组织", member: "成员", key: "密钥" };

// ── amounts ─────────────────────────────────────────────────────────────────

/** Micro-cny of a typed amount that `isCnyAmount` accepted: integer arithmetic, no float. */
function cnyToMicro(s: string): number {
  const [whole = "0", cents = ""] = s.split(".");
  return Number(whole) * 1_000_000 + Number(cents.padEnd(2, "0")) * 10_000;
}

/** Quota reads in flight at once: the page's share of the control plane's small connection pool. */
export const QUOTA_READS_IN_FLIGHT = 4;
/** Keys the 实际可用 table lists before 显示更多; only the keys listed are read. */
const KEYS_PER_VIEW = 25;

/** `f` over `items`, at most `limit` at a time, settled like `Promise.allSettled`; stops starting reads once `live()` is false. */
async function settledLimited<T, R>(
  items: T[],
  limit: number,
  live: () => boolean,
  f: (t: T) => Promise<R>,
): Promise<PromiseSettledResult<R>[]> {
  const out: PromiseSettledResult<R>[] = [];
  let next = 0;
  const worker = async () => {
    while (next < items.length && live()) {
      const i = next++;
      try {
        out[i] = { status: "fulfilled", value: await f(items[i]!) };
      } catch (reason) {
        out[i] = { status: "rejected", reason };
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out.filter(Boolean);
}

/** The keys the 实际可用 table lists: the viewer's keys, then keys that only carry a quota. */
function keyEntries(keys: TenantKey[] | null, rows: Budget[]): [string, string][] {
  const seen = new Map<string, string>((keys ?? []).map((k) => [k.id, k.label]));
  for (const b of rows) if (b.target_live && !seen.has(b.target.id)) seen.set(b.target.id, b.target.label);
  return [...seen];
}

// ── comparing with the level above ──────────────────────────────────────────

/** What a quota is, for comparing it with another. */
interface Terms {
  kind: Budget["kind"];
  period: Budget["period"];
  limit: number;
}

/**
 * Whether `limit` is above a parent's, compared only like for like: two
 * periodic quotas of the same period, or two cumulative ones. A periodic
 * one against a cumulative one measures different things, so it never warns.
 */
function exceeds(own: Terms, parents: Terms[]): boolean {
  return parents.some((p) => p.kind === own.kind && p.period === own.period && own.limit > p.limit);
}

/** The levels above a key, from its own quota answer: the organisation's and its member's. */
function keyParents(q: KeyQuota | undefined): Terms[] {
  return (q?.quotas ?? [])
    .filter((f) => f.scope !== "api_key")
    .map((f) => ({ kind: f.kind, period: f.period, limit: f.limit_micro_cny }));
}

const termsOf = (b: Budget): Terms => ({ kind: b.kind, period: b.period, limit: b.limit_micro_cny });

// ── who may do what ─────────────────────────────────────────────────────────

/**
 * What the viewer may do to a row. An owner or admin edits and deletes the
 * quotas of `org_admin` and may delete a member's own cap on a key; a member
 * edits and deletes its own caps; nobody touches the platform's.
 */
function powers(b: Budget, role: SessionMember["role"]): { edit: boolean; del: boolean } {
  if (b.set_by === "operator") return { edit: false, del: false };
  if (role === "owner" || role === "admin") {
    return b.set_by === "org_admin" ? { edit: true, del: true } : { edit: false, del: true };
  }
  if (role === "member" && b.set_by === "member") return { edit: true, del: true };
  return { edit: false, del: false };
}

// ── the page ────────────────────────────────────────────────────────────────

/** A card that is a region named by its heading, its actions beside the heading. */
function Section({ title, number, actions, children, className, tone }: { title: string; number: number; actions?: ReactNode; children: ReactNode; className?: string; tone?: "surface" | "inverse" }) {
  return (
    <Card role="region" aria-label={title} className={className} tone={tone}>
      <CardHeader number={number} title={title} actions={actions} />
      {children}
    </Card>
  );
}

/**
 * 「预算」: the three levels of quota that stand between a key and the
 * organisation's money. Operators set the organisation's; owners and admins
 * set members' and any key's; a member caps its own keys. A child level may
 * exceed its parent: the smallest remaining amount binds, and the page says
 * so where a limit passes the one above it.
 */
export function Budgets({ member }: { member: SessionMember }) {
  const role = member.role;
  const manager = role === "owner" || role === "admin";
  const creator = manager || role === "member";
  const financial = manager || role === "billing";

  const [rows, setRows] = useState<Budget[] | null>(null);
  const [rowsErr, setRowsErr] = useState<string | null>(null);
  const [balance, setBalance] = useState<BudgetAccountBalance | null>(null);
  const [balanceErr, setBalanceErr] = useState<string | null>(null);
  const [keys, setKeys] = useState<TenantKey[] | null>(role === "billing" ? [] : null);
  const [people, setPeople] = useState<Member[]>([]);
  const [quotas, setQuotas] = useState<Record<string, KeyQuota>>({});
  const [quotaErr, setQuotaErr] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const [listed, setListed] = useState(0);
  const [shown, setShown] = useState(KEYS_PER_VIEW);
  const [reloading, setReloading] = useState(true);
  const reload = useCallback(() => setTick((n) => n + 1), []);

  // 账户与额外预算独立读取；普通成员不发起无权访问的财务请求。
  useEffect(() => {
    if (!financial) return;
    let live = true;
    formulaBalance().then(
      (value) => {
        if (!live) return;
        setBalance(value);
        setBalanceErr(null);
      },
      (e: unknown) => {
        if (!live) return;
        setBalance(null);
        setBalanceErr(failure(e));
      },
    );
    return () => {
      live = false;
    };
  }, [financial, tick]);

  useEffect(() => {
    let live = true;
    setReloading(true);
    void (async () => {
      const [list, keyList, peopleList] = await Promise.allSettled([
        budgetApi.list(),
        role === "billing" ? Promise.resolve<TenantKey[]>([]) : keyApi.list(),
        manager ? memberApi.list() : Promise.resolve<Member[]>([]),
      ]);
      if (!live) return;
      if (list.status === "fulfilled") {
        setRows(list.value);
        setRowsErr(null);
      } else {
        setRowsErr(failure(list.reason));
      }
      setKeys(keyList.status === "fulfilled" ? keyList.value : []);
      if (peopleList.status === "fulfilled") setPeople(peopleList.value);
      setListed((n) => n + 1);
    })();
    return () => {
      live = false;
    };
  }, [role, manager, tick]);

  // Each key's own answer: what is left on it and which level binds. Only the
  // keys in view are read, and at most QUOTA_READS_IN_FLIGHT at a time.
  useEffect(() => {
    if (listed === 0 || keys === null) return;
    let live = true;
    void (async () => {
      const ids = new Set(keyEntries(keys, (rows ?? []).filter((b) => b.scope === "api_key")).slice(0, shown).map(([id]) => id));
      for (const b of rows ?? []) if (b.scope === "api_key" && b.target_live) ids.add(b.target.id);
      const answers = await settledLimited([...ids], QUOTA_READS_IN_FLIGHT, () => live, (id) => billing.keyQuota(id));
      if (!live) return;
      const next: Record<string, KeyQuota> = {};
      let failed: unknown = null;
      answers.forEach((a) => {
        if (a.status === "fulfilled") next[a.value.key_id] = a.value;
        else failed = a.reason;
      });
      setQuotas(next);
      setQuotaErr(failed === null ? null : failure(failed));
      setReloading(false);
    })();
    return () => {
      live = false;
    };
    // `listed` moves once per list read; `rows` and `keys` are the ones it set.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [listed, shown]);

  const team = (rows ?? []).filter((b) => b.scope === "team");
  const memberRows = (rows ?? []).filter((b) => b.scope === "member");
  const keyRows = (rows ?? []).filter((b) => b.scope === "api_key");

  return (
    <>
      <PageHeader title="预算" eyebrow="BUDGETS · 分级额度" dataLead="账户额度与组织、成员、密钥预算独立生效。多条限制同时存在时，以先用尽者为准。" />
      <BentoGrid>
        {rowsErr && <div className={cn(span.d12, "note crit")} role="alert">预算读取失败：{rowsErr}</div>}
        {financial && <>
          <Section number={1} title="账户额度" className={span.d8} actions={<a className="inline-flex min-h-11 items-center text-body" href="#billing">查看账户 ↗</a>}>
            {balanceErr ? <div role="alert"><CardMessage>账户额度读取失败：{balanceErr}</CardMessage></div> : balance === null ? <Loading /> : <>
              <BudgetAccountAmounts balance={balance} />
              {balance.exhausted && <div className="note warn">{balance.billing_mode === "postpaid" ? "账户已达信用额度" : "账户余额不足"}</div>}
            </>}
            <CardFooter className="border-t border-ink/7 pt-4 text-caption text-muted">账户额度与额外预算上限分别生效；调用仍受组织、成员、密钥预算及其他限制约束。</CardFooter>
          </Section>
          <Section number={2} title="组织预算（平台设置）" tone="inverse" className={cn(span.d4, "tablet:col-span-12")}>
            {rows !== null && !rowsErr && <><Metric size="display">{team.length.toLocaleString("en-US")}</Metric><p className="text-caption text-on-inverse-muted">条额外组织预算</p></>}
            <p className="text-caption text-on-inverse-muted">平台可额外设置按周期或一次性的组织预算上限，与账户余额和信用额度不同。</p>
            {!rowsErr && <QuotaTable label="组织额度" rows={rows === null ? null : team} empty="未设置额外组织预算上限" role={role} parentsOf={() => []} onChanged={reload} />}
          </Section>
        </>}
        <Section number={financial ? 3 : 1} title="成员额度" className={creator && !rowsErr ? span.d7 : span.d12}>
          <p className="text-caption text-muted">{manager || role === "billing" ? "组织中每个成员的独立额度" : "分配给我的成员额度"}；进度仅比较每条预算自身的已用与限额，不合并不同周期。</p>
          {!rowsErr && <QuotaTable label="成员额度" rows={rows === null ? null : memberRows}
            empty={manager ? "还没有成员额度，可在「新建额度」中新建" : role === "billing" ? "还没有成员额度" : "还没有给你设置成员额度"}
            role={role} parentsOf={() => team.map(termsOf)} onChanged={reload} />}
        </Section>
        {creator && !rowsErr && <CreateForm number={financial ? 4 : 2} role={role} keys={keys} people={people} team={team} quotas={quotas} onCreated={reload} />}
        <Section number={financial ? creator && !rowsErr ? 5 : 4 : creator && !rowsErr ? 3 : 2} title="密钥额度" className={span.d12}
          actions={<Button size="sm" icon="refresh" busy={reloading} busyLabel="刷新中…" onClick={reload}>刷新</Button>}>
          <p className="text-caption text-muted">实际可用额度取账户与各级预算的最小剩余值；未设置限额时显示“不限”。每次展开最多增加 25 个密钥。</p>
          {quotaErr && <div className="note crit" role="alert">{quotaErr}</div>}
          <KeyFigures keys={keys} rows={keyRows} quotas={quotas} shown={shown} onMore={() => setShown((n) => n + KEYS_PER_VIEW)} />
          {!rowsErr && <QuotaTable label="密钥额度" rows={rows === null ? null : keyRows}
            empty={creator ? "还没有密钥额度，可在「新建额度」中新建" : "还没有密钥额度"}
            role={role} parentsOf={(b) => keyParents(quotas[b.target.id])} onChanged={reload} />}
        </Section>
      </BentoGrid>
    </>
  );
}

// ── what each key can still spend ───────────────────────────────────────────

/** Per key, what it can still spend and the level that stops it first. */
function KeyFigures({
  keys,
  rows,
  quotas,
  shown,
  onMore,
}: {
  keys: TenantKey[] | null;
  rows: Budget[];
  quotas: Record<string, KeyQuota>;
  shown: number;
  onMore: () => void;
}) {
  if (keys === null) return <Loading />;
  // A viewer who cannot list keys (billing) still reads the keys that carry a quota.
  const seen = keyEntries(keys, rows);
  if (seen.length === 0) return null;
  return (
    <>
    <DataTable label="密钥实际可用">
      <thead>
        <tr>
          <th>密钥</th>
          <th className="num">实际可用</th>
          <th>受限于</th>
        </tr>
      </thead>
      <tbody>
        {seen.slice(0, shown).map(([id, label]) => {
          const q = quotas[id];
          return (
            <tr key={id}>
              <td>
                <span className="trunc" title={label}>
                  {label}
                </span>
              </td>
              <td className="num">
                {!q ? "—" : q.effective_remaining_micro_cny === null ? "不限" : <Money micro={q.effective_remaining_micro_cny} />}
              </td>
              <td>{q?.binding_level ? (LEVEL[q.binding_level] ?? q.binding_level) : "—"}</td>
            </tr>
          );
        })}
      </tbody>
    </DataTable>
    {seen.length > shown && (
      <div className="!mt-0 flex justify-center border-t border-border pt-3">
        <Button size="sm" onClick={onMore}>
          显示更多
        </Button>
      </div>
    )}
    </>
  );
}

// ── the rows of one level ───────────────────────────────────────────────────

function QuotaTable({
  label,
  rows,
  empty,
  role,
  parentsOf,
  onChanged,
}: {
  label: string;
  rows: Budget[] | null;
  empty: string;
  role: SessionMember["role"];
  parentsOf: (b: Budget) => Terms[];
  onChanged: () => void;
}) {
  if (rows === null) return <Loading />;
  if (rows.length === 0) return <EmptyState text={empty} mark="route" />;
  return (
    <DataTable label={label}>
      <thead>
        <tr>
          <th>对象</th>
          <th>设置方</th>
          <th>类型</th>
          <th>周期（UTC）</th>
          <th className="num">限额</th>
          <th className="num">已用</th>
          <th className="num">剩余</th>
          <th>状态</th>
          <th>
            <span className="vh">操作</span>
          </th>
        </tr>
      </thead>
      <tbody>
        {rows.map((b) => (
          <QuotaRow key={b.id} b={b} can={powers(b, role)} parents={parentsOf(b)} onChanged={onChanged} />
        ))}
      </tbody>
    </DataTable>
  );
}

/** A cumulative quota counts from the day it was set; a periodic one is its period. */
function periodText(b: Budget): string {
  if (b.kind === "cumulative") return b.counted_from ? `自 ${fmtUtc(b.counted_from).slice(0, 10)} 起` : "自设置起";
  return b.period ? (PERIOD[b.period] ?? b.period) : "—";
}

function QuotaRow({
  b,
  can,
  parents,
  onChanged,
}: {
  b: Budget;
  can: { edit: boolean; del: boolean };
  parents: Terms[];
  onChanged: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [acting, setActing] = useState<"save" | "delete" | null>(null);
  const [form, focusRefusal] = useRefusalFocus<HTMLTableRowElement>();
  const toast = useToast();

  const draftOk = isCnyAmount(draft.trim(), { allowZero: false });
  const limit = editing && draftOk ? cnyToMicro(draft.trim()) : b.limit_micro_cny;
  const over = exceeds({ ...termsOf(b), limit }, parents);
  const progressNote = exactInteger(b.limit_micro_cny) === 0n ? "限额为零，不计算比例" : undefined;

  async function save() {
    const text = draft.trim();
    if (!isCnyAmount(text, { allowZero: false })) {
      setProblem("请输入金额，最多两位小数");
      focusRefusal();
      return;
    }
    setProblem(null);
    setActing("save");
    try {
      await budgetApi.setLimit(b.id, text);
      setEditing(false);
      toast("已保存");
      onChanged();
    } catch (e) {
      setProblem(e instanceof ApiError ? formMessageFor(e) : String(e));
      focusRefusal();
    } finally {
      setActing(null);
    }
  }

  async function remove() {
    setActing("delete");
    try {
      await budgetApi.remove(b.id);
      toast("已删除");
      onChanged();
    } catch (e) {
      setProblem(failure(e));
    } finally {
      setActing(null);
    }
  }

  return (
    <tr ref={form}>
      <td>
        <span className="trunc" title={b.target.label}>
          {b.target.label}
          {!b.target_live && "（已失效）"}
        </span>
      </td>
      <td>{SET_BY[b.set_by] ?? b.set_by}</td>
      <td>{KIND[b.kind] ?? b.kind}</td>
      <td className="tabular">{periodText(b)}</td>
      <td className="num">
        {editing ? (
          <Field label="新的限额（元）" error={problem}>
            {(aria) => (
              <input inputMode="decimal" autoComplete="off" value={draft} onChange={(e) => setDraft(e.target.value)} {...aria} />
            )}
          </Field>
        ) : (
          <Money micro={b.limit_micro_cny} />
        )}
        {over && <span className="hint cell-note">{OVER_PARENT}</span>}
        {!editing && problem && <span className="field-problem">{problem}</span>}
      </td>
      <td className="num">
        <Money micro={b.spent_micro_cny} />
        <BudgetProgress value={exactRatio(b.spent_micro_cny, b.limit_micro_cny)} accessibleLabel={`${b.target.label}，${periodText(b)}预算已用进度`} valueLabel={progressNote} className="mt-2 min-w-24" />
      </td>
      <td className="num">
        <Money micro={b.remaining_micro_cny} />
      </td>
      <td>
        <span data-state={b.exhausted ? "exhausted" : "ok"}>
          <Lamp state={b.exhausted ? "rejected" : "applied"}>{b.exhausted ? "已用尽" : "正常"}</Lamp>
        </span>
      </td>
      <td className="right">
        {(can.edit || can.del) && (
          <span className="acts">
            {editing ? (
              <>
                <Button size="sm" kind="primary" busy={acting === "save"} busyLabel="保存中…" onClick={() => void save()}>
                  保存
                </Button>
                <Button
                  size="sm"
                  disabled={acting !== null}
                  onClick={() => {
                    setEditing(false);
                    setProblem(null);
                  }}
                >
                  取消
                </Button>
              </>
            ) : (
              <>
                {can.edit && (
                  <Button
                    size="sm"
                    disabled={acting !== null}
                    onClick={() => {
                      setDraft(microCnyToInput(b.limit_micro_cny));
                      setProblem(null);
                      setEditing(true);
                    }}
                  >
                    修改限额
                  </Button>
                )}
                {can.del && (
                  <TwoStep
                    label="删除"
                    size="sm"
                    warning="删除后不再按这个额度拦截"
                    disabled={acting !== null}
                    busy={acting === "delete"}
                    onConfirm={() => void remove()}
                  />
                )}
              </>
            )}
          </span>
        )}
      </td>
    </tr>
  );
}

// ── the new-quota form ──────────────────────────────────────────────────────

type FormField = "scope" | "target" | "kind" | "period" | "limit";

const FIELD_OF: Record<string, FormField> = {
  "/scope": "scope",
  "/scope_ref": "target",
  "/kind": "kind",
  "/period": "period",
  "/limit_cny": "limit",
};

function CreateForm({
  number,
  role,
  keys,
  people,
  team,
  quotas,
  onCreated,
}: {
  number: number;
  role: SessionMember["role"];
  keys: TenantKey[] | null;
  people: Member[];
  team: Budget[];
  quotas: Record<string, KeyQuota>;
  onCreated: () => void;
}) {
  const manager = role === "owner" || role === "admin";
  const [scope, setScope] = useState<NewBudget["scope"]>(manager ? "member" : "api_key");
  const [target, setTarget] = useState("");
  const [kind, setKind] = useState<Budget["kind"]>("periodic");
  const [period, setPeriod] = useState<NonNullable<Budget["period"]>>("month");
  const [limit, setLimit] = useState("");
  const [errors, setErrors] = useState<Partial<Record<FormField, string>>>({});
  const [general, setGeneral] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const toast = useToast();

  const options: { id: string; label: string }[] =
    scope === "member"
      ? people.map((p) => ({ id: p.id, label: `${p.display_name ?? p.email}${p.disabled ? "（已停用）" : ""}` }))
      : (keys ?? []).map((k) => ({
          id: k.id,
          label: manager ? `${k.label}（${k.owner ? (k.owner.display_name ?? "成员") : "组织"}）` : k.label,
        }));
  const chosen = options.some((o) => o.id === target) ? target : (options[0]?.id ?? "");

  // Warn as the amount is typed, against the levels above the chosen object.
  const text = limit.trim();
  const parents = scope === "member" ? team.map(termsOf) : keyParents(quotas[chosen]);
  const over =
    isCnyAmount(text, { allowZero: false }) &&
    exceeds({ kind, period: kind === "periodic" ? period : null, limit: cnyToMicro(text) }, parents);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setGeneral(null);
    const next: Partial<Record<FormField, string>> = {};
    if (!chosen) next.target = "请选择对象";
    if (!isCnyAmount(text, { allowZero: false })) next.limit = "请输入金额，最多两位小数";
    setErrors(next);
    if (Object.keys(next).length > 0) {
      focusRefusal();
      return;
    }
    setBusy(true);
    try {
      await budgetApi.create({
        scope,
        scope_ref: chosen,
        kind,
        ...(kind === "periodic" ? { period } : {}),
        limit_cny: text,
      });
      setLimit("");
      toast("已创建");
      onCreated();
    } catch (err) {
      if (err instanceof ApiError && err.code === "validation_failed") {
        const refused: Partial<Record<FormField, string>> = {};
        const rest: string[] = [];
        for (const d of err.details) {
          const f = FIELD_OF[d.path];
          if (f) refused[f] = d.message;
          else rest.push(d.message);
        }
        setErrors(refused);
        if (Object.keys(refused).length === 0 || rest.length > 0) setGeneral(rest.length > 0 ? rest.join("；") : formMessageFor(err));
        if (Object.keys(refused).length > 0) focusRefusal();
      } else {
        setGeneral(failure(err));
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <Section number={number} title="新建额度" className={span.d5Wide}>
      <form className="grid gap-4" ref={form} onSubmit={submit} noValidate>
        <Field label="对象类型" error={errors.scope}>
          {(aria) => (
            <select
              value={scope}
              onChange={(e) => {
                setScope(e.target.value as NewBudget["scope"]);
                setTarget("");
              }}
              {...aria}
            >
              {manager && <option value="member">成员</option>}
              <option value="api_key">密钥</option>
            </select>
          )}
        </Field>
        <Field label="对象" error={errors.target}>
          {(aria) => (
            <select value={chosen} onChange={(e) => setTarget(e.target.value)} {...aria}>
              {options.length === 0 && <option value="">（没有可选的对象）</option>}
              {options.map((o) => (
                <option key={o.id} value={o.id}>
                  {o.label}
                </option>
              ))}
            </select>
          )}
        </Field>
        <Field label="类型" error={errors.kind}>
          {(aria) => (
            <select value={kind} onChange={(e) => setKind(e.target.value as Budget["kind"])} {...aria}>
              <option value="periodic">{KIND.periodic}</option>
              <option value="cumulative">{KIND.cumulative}</option>
            </select>
          )}
        </Field>
        {kind === "periodic" && (
          <Field label="周期" error={errors.period}>
            {(aria) => (
              <select value={period} onChange={(e) => setPeriod(e.target.value as NonNullable<Budget["period"]>)} {...aria}>
                <option value="day">{PERIOD.day}</option>
                <option value="week">{PERIOD.week}</option>
                <option value="month">{PERIOD.month}</option>
              </select>
            )}
          </Field>
        )}
        <Field label="限额（元）" error={errors.limit} hint={over ? OVER_PARENT : "最多两位小数"}>
          {(aria) => <input inputMode="decimal" autoComplete="off" value={limit} onChange={(e) => setLimit(e.target.value)} {...aria} />}
        </Field>
        {general && (
          <div className="note crit col-span-full" role="alert">
            {general}
          </div>
        )}
        <div className="col-span-full mt-2 flex justify-end gap-2 border-t border-ink/7 pt-4">
          <Button kind="primary" type="submit" busy={busy} busyLabel="创建中…">
            创建
          </Button>
        </div>
      </form>
    </Section>
  );
}
