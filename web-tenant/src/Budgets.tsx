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
import { formMessageFor, messageFor } from "./lib/errors";
import { fmtUtc, isUsdAmount } from "./lib/money";
import { Money } from "./ui/BalanceReadout";
import { Button } from "./ui/Button";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field, useRefusalFocus } from "./ui/Field";
import { Lamp } from "./ui/Lamp";
import { useToast } from "./ui/Toast";
import { TwoStep } from "./ui/TwoStep";

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

const OVER_PARENT = "超过上级额度时以先用尽者为准";

const SET_BY: Record<Budget["set_by"], string> = { operator: "平台", org_admin: "组织管理员", member: "成员" };
const KIND: Record<Budget["kind"], string> = { periodic: "按周期", cumulative: "一次性额度" };
const PERIOD: Record<NonNullable<Budget["period"]>, string> = { day: "每天", week: "每周（周一起）", month: "每月" };
const LEVEL: Record<QuotaLevel, string> = { organization: "组织", member: "成员", key: "密钥" };

// ── amounts ─────────────────────────────────────────────────────────────────

/** Micro-dollars of a typed amount that `isUsdAmount` accepted: integer arithmetic, no float. */
function usdToMicro(s: string): number {
  const [whole = "0", cents = ""] = s.split(".");
  return Number(whole) * 1_000_000 + Number(cents.padEnd(2, "0")) * 10_000;
}

/** A limit as the dollars it was typed in (`1.50`), for an edit field. */
function microToUsd(micro: number): string {
  const cents = String(Math.floor((micro % 1_000_000) / 10_000)).padStart(2, "0");
  return `${Math.floor(micro / 1_000_000)}.${cents}`;
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
    .map((f) => ({ kind: f.kind, period: f.period, limit: f.limit_micro_usd }));
}

const termsOf = (b: Budget): Terms => ({ kind: b.kind, period: b.period, limit: b.limit_micro_usd });

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

  const [rows, setRows] = useState<Budget[] | null>(null);
  const [rowsErr, setRowsErr] = useState<string | null>(null);
  const [keys, setKeys] = useState<TenantKey[] | null>(role === "billing" ? [] : null);
  const [people, setPeople] = useState<Member[]>([]);
  const [quotas, setQuotas] = useState<Record<string, KeyQuota>>({});
  const [quotaErr, setQuotaErr] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const [reloading, setReloading] = useState(true);
  const reload = useCallback(() => setTick((n) => n + 1), []);

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
        setRows((r) => r ?? []);
        setRowsErr(failure(list.reason));
      }
      setKeys(keyList.status === "fulfilled" ? keyList.value : []);
      if (peopleList.status === "fulfilled") setPeople(peopleList.value);

      // Each key's own answer: what is left on it and which level binds.
      const ids = new Set<string>();
      if (keyList.status === "fulfilled") for (const k of keyList.value) ids.add(k.id);
      if (list.status === "fulfilled") {
        for (const b of list.value) if (b.scope === "api_key" && b.target_live) ids.add(b.target.id);
      }
      const answers = await Promise.allSettled([...ids].map((id) => billing.keyQuota(id)));
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
  }, [role, manager, tick]);

  const team = (rows ?? []).filter((b) => b.scope === "team");
  const memberRows = (rows ?? []).filter((b) => b.scope === "member");
  const keyRows = (rows ?? []).filter((b) => b.scope === "api_key");

  return (
    <div className="stack">
      {rowsErr && (
        <div className="note crit" role="alert">
          {rowsErr}
        </div>
      )}
      {creator && (
        <CreateForm role={role} keys={keys} people={people} team={team} quotas={quotas} onCreated={reload} />
      )}
      {role !== "member" && (
        <Section title="组织额度（平台分配）" list>
          <QuotaTable
            label="组织额度"
            rows={rows === null ? null : team}
            empty="平台还没有为组织设置额度"
            role={role}
            parentsOf={() => []}
            onChanged={reload}
          />
        </Section>
      )}
      <Section title="成员额度" list>
        <QuotaTable
          label="成员额度"
          rows={rows === null ? null : memberRows}
          empty={manager ? "还没有成员额度，在上方新建" : "还没有给你设置成员额度"}
          role={role}
          parentsOf={() => team.map(termsOf)}
          onChanged={reload}
        />
      </Section>
      <Section title="密钥额度" list>
        <div className="panel-notes">
          {quotaErr && (
            <div className="note crit" role="alert">
              {quotaErr}
            </div>
          )}
          <div>
            <Button size="sm" icon="refresh" busy={reloading} busyLabel="刷新中…" onClick={reload}>
              刷新
            </Button>
          </div>
        </div>
        <KeyFigures keys={keys} rows={keyRows} quotas={quotas} />
        <QuotaTable
          label="密钥额度"
          rows={rows === null ? null : keyRows}
          empty="还没有密钥额度，在上方新建"
          role={role}
          parentsOf={(b) => keyParents(quotas[b.target.id])}
          onChanged={reload}
        />
      </Section>
    </div>
  );
}

// ── what each key can still spend ───────────────────────────────────────────

/** Per key, what it can still spend and the level that stops it first. */
function KeyFigures({ keys, rows, quotas }: { keys: TenantKey[] | null; rows: Budget[]; quotas: Record<string, KeyQuota> }) {
  if (keys === null) return <Loading />;
  // A viewer who cannot list keys (billing) still reads the keys that carry a quota.
  const seen = new Map<string, string>(keys.map((k) => [k.id, k.label]));
  for (const b of rows) if (b.target_live && !seen.has(b.target.id)) seen.set(b.target.id, b.target.label);
  if (seen.size === 0) return null;
  return (
    <DataTable label="密钥实际可用">
      <thead>
        <tr>
          <th>密钥</th>
          <th className="num">实际可用</th>
          <th>受限于</th>
        </tr>
      </thead>
      <tbody>
        {[...seen].map(([id, label]) => {
          const q = quotas[id];
          return (
            <tr key={id}>
              <td>
                <span className="trunc" title={label}>
                  {label}
                </span>
              </td>
              <td className="num">
                {!q ? "—" : q.effective_remaining_micro_usd === null ? "不限" : <Money micro={q.effective_remaining_micro_usd} />}
              </td>
              <td>{q?.binding_level ? LEVEL[q.binding_level] : "—"}</td>
            </tr>
          );
        })}
      </tbody>
    </DataTable>
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
  return b.period ? PERIOD[b.period] : "—";
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

  const draftOk = isUsdAmount(draft.trim(), { allowZero: false });
  const limit = editing && draftOk ? usdToMicro(draft.trim()) : b.limit_micro_usd;
  const over = exceeds({ ...termsOf(b), limit }, parents);

  async function save() {
    const text = draft.trim();
    if (!isUsdAmount(text, { allowZero: false })) {
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
      <td>{SET_BY[b.set_by]}</td>
      <td>{KIND[b.kind]}</td>
      <td className="tabular">{periodText(b)}</td>
      <td className="num">
        {editing ? (
          <Field label="新的限额（美元）" error={problem}>
            {(aria) => (
              <input inputMode="decimal" autoComplete="off" value={draft} onChange={(e) => setDraft(e.target.value)} {...aria} />
            )}
          </Field>
        ) : (
          <Money micro={b.limit_micro_usd} />
        )}
        {over && <span className="hint cell-note">{OVER_PARENT}</span>}
        {!editing && problem && <span className="field-problem">{problem}</span>}
      </td>
      <td className="num">
        <Money micro={b.spent_micro_usd} />
      </td>
      <td className="num">
        <Money micro={b.remaining_micro_usd} />
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
                      setDraft(microToUsd(b.limit_micro_usd));
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
  "/limit_usd": "limit",
};

function CreateForm({
  role,
  keys,
  people,
  team,
  quotas,
  onCreated,
}: {
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
    isUsdAmount(text, { allowZero: false }) &&
    exceeds({ kind, period: kind === "periodic" ? period : null, limit: usdToMicro(text) }, parents);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setGeneral(null);
    const next: Partial<Record<FormField, string>> = {};
    if (!chosen) next.target = "请选择对象";
    if (!isUsdAmount(text, { allowZero: false })) next.limit = "请输入金额，最多两位小数";
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
        limit_usd: text,
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
    <Section title="新建额度">
      <form className="form cols" ref={form} onSubmit={submit} noValidate>
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
        <Field label="限额（美元）" error={errors.limit} hint={over ? OVER_PARENT : "最多两位小数"}>
          {(aria) => <input inputMode="decimal" autoComplete="off" value={limit} onChange={(e) => setLimit(e.target.value)} {...aria} />}
        </Field>
        {general && (
          <div className="note crit" role="alert">
            {general}
          </div>
        )}
        <div className="form-actions">
          <Button kind="primary" type="submit" busy={busy} busyLabel="创建中…">
            创建
          </Button>
        </div>
      </form>
    </Section>
  );
}
