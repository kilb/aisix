import { useCallback, useEffect, useId, useState } from "react";
import type { TabProps } from "../App";
import { ApiError, operators, type Detail, type OperatorAccount, type OperatorRole } from "../lib/api";
import { messageFor } from "../lib/errors";
import { errorText, fmtInt, fmtTime } from "../lib/fmt";
import { cn } from "../lib/cn";
import { Button } from "../ui/Button";
import { useRefusalFocus } from "../ui/Field";
import { DataTable, Loading } from "../ui/DataTable";
import { Lamp } from "../ui/Lamp";
import {
  BentoGrid,
  Card,
  CardFooter,
  CardHeader,
  CardMessage,
  ChartSkeleton,
  Metric,
  MetricSkeleton,
  PageHeader,
  span,
} from "../components/page";
import { RankBars } from "../components/charts";
import { Plus, RefreshCw } from "lucide-react";

/** The control plane's operator roles, highest first, by the names this page shows. */
const ROLES: { role: OperatorRole; label: string }[] = [
  { role: "admin", label: "管理员" },
  { role: "operator", label: "运维" },
  { role: "viewer", label: "只读" },
];

/** What a new account starts at: the least it can be given. */
const NEW_ROLE: OperatorRole = "viewer";

const roleLabel = (role: OperatorRole) => ROLES.find((r) => r.role === role)?.label ?? role;

/** A row action waiting for its confirmation (ruling P1b-R6: inline, never a dialog). */
type Pending = { id: string; kind: "disable" } | { id: string; kind: "role"; role: OperatorRole };

/**
 * "运维账号" (admins only): every operator with role and status; create one,
 * change a role, disable and enable. The control plane refuses to leave no
 * enabled admin (`409 last_admin`) and is the authority on every write, so
 * each refusal is shown as it comes back.
 */
export function Operators({ operator }: TabProps) {
  const [items, setItems] = useState<OperatorAccount[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [pending, setPending] = useState<Pending | null>(null);
  const [busy, setBusy] = useState(false);
  // The account whose change is in flight.
  const [changing, setChanging] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<{ ok: boolean; text: string } | null>(null);

  const reload = useCallback(async () => {
    try {
      setItems(await operators.list());
      setLoadError(null);
    } catch (e) {
      setLoadError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  async function change(account: OperatorAccount, changes: { role?: OperatorRole; disabled?: boolean }, done: string) {
    setBusy(true);
    setChanging(account.id);
    setOutcome(null);
    try {
      const view = await operators.update(account.id, changes);
      setItems((prev) =>
        prev?.map((i) => (i.id === view.id ? { ...i, ...view, disabled: changes.disabled ?? i.disabled } : i)) ?? null,
      );
      setOutcome({ ok: true, text: done });
      void reload();
    } catch (e) {
      // The row keeps what is stored: a refused change was not made.
      setOutcome({ ok: false, text: `${account.email}：${errorText(e)}` });
    } finally {
      setPending(null);
      setBusy(false);
      setChanging(null);
    }
  }

  function created(email: string, role: OperatorRole) {
    setCreating(false);
    setOutcome({ ok: true, text: `已创建 ${email}（${roleLabel(role)}）` });
    void reload();
  }

  const enabled = items?.filter((account) => !account.disabled) ?? null;
  const roles = ROLES.map(({ role, label }) => {
    const value = items?.filter((account) => account.role === role).length ?? 0;
    return { label, value, displayValue: fmtInt(value) };
  }).sort((a, b) => b.value - a.value || a.label.localeCompare(b.label));

  // 桌面和平板：概览 6 + 6；列表 12，创建时列表 7 + 表单 5（平板各占 12）。
  const list = (
    <Card className={creating ? span.d7 : span.d12}>
      <CardHeader
        number={3}
        as="h2"
        description="管理员可以新建账号、调整角色、停用或启用。停用会立即结束该账号的全部会话；账号不会被删除，停用即收回访问。至少要保留一个可用的管理员。"
        title={
          <span className="inline-flex items-center gap-2">
            全部账号
            {items && items.length > 0 && <span className="tag">{fmtInt(items.length)}</span>}
          </span>
        }
      />
      {outcome && (
        <div className={outcome.ok ? "note ok mt-0" : "note crit mt-0"} role={outcome.ok ? "status" : "alert"}>
          {outcome.text}
        </div>
      )}
      {loadError && (
        <div className="note crit" role="alert">
          {loadError}
        </div>
      )}
      {items === null && !loadError && <Loading />}
      {items !== null && (
        <DataTable label="全部运维账号">
          <thead>
            <tr>
              <th>邮箱</th>
              <th>显示名</th>
              <th>角色</th>
              <th>状态</th>
              <th>创建时间</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {items.map((a) => {
              const mine = pending?.id === a.id ? pending : null;
              const shownRole = mine?.kind === "role" ? mine.role : a.role;
              return (
                <tr key={a.id}>
                  <td>
                    <span className="inline-flex items-center gap-2.5 align-middle">
                      <span
                        aria-hidden="true"
                        className="grid size-11 shrink-0 place-items-center rounded-full bg-well text-meta font-medium text-ink-soft uppercase"
                      >
                        {(a.display_name || a.email).slice(0, 1)}
                      </span>
                      <span className="flex flex-col leading-tight">
                        <span className="font-medium">{a.email}</span>
                        {a.id === operator.id && <span className="hint mt-0.5 text-meta">（当前账号）</span>}
                      </span>
                    </span>
                  </td>
                  <td>{a.display_name ?? <span className="text-muted-foreground">—</span>}</td>
                  <td>
                    <select
                      aria-label="角色"
                      className="select-sm"
                      value={shownRole}
                      disabled={busy || mine?.kind === "disable"}
                      onChange={(e) => {
                        const role = e.target.value as OperatorRole;
                        setOutcome(null);
                        setPending(role === a.role ? null : { id: a.id, kind: "role", role });
                      }}
                    >
                      {ROLES.map((r) => (
                        <option key={r.role} value={r.role}>
                          {r.label}
                        </option>
                      ))}
                    </select>
                  </td>
                  <td>
                    <Lamp state={a.disabled ? "disabled" : "applied"}>{a.disabled ? "已停用" : "正常"}</Lamp>
                  </td>
                  <td className="text-muted-foreground">{fmtTime(a.created_at)}</td>
                  <td className="right">
                    <span className="inline-flex items-center justify-end gap-1.5">
                      {mine?.kind === "role" && (
                        <>
                          <Button
                            kind="primary"
                            size="sm"
                            type="button"
                            disabled={busy}
                            busy={changing === a.id}
                            busyLabel={`确认改为${roleLabel(mine.role)}`}
                            onClick={() =>
                              void change(a, { role: mine.role }, `${a.email} 已改为${roleLabel(mine.role)}`)
                            }
                          >
                            确认改为{roleLabel(mine.role)}
                          </Button>
                          <Button size="sm" type="button" disabled={busy} onClick={() => setPending(null)}>
                            取消
                          </Button>
                        </>
                      )}
                      {mine?.kind === "disable" && (
                        <>
                          <Button
                            kind="danger"
                            size="sm"
                            type="button"
                            disabled={busy}
                            busy={changing === a.id}
                            busyLabel="确认停用"
                            onClick={() => void change(a, { disabled: true }, `${a.email} 已停用`)}
                          >
                            确认停用
                          </Button>
                          <Button size="sm" type="button" disabled={busy} onClick={() => setPending(null)}>
                            取消
                          </Button>
                        </>
                      )}
                      {!mine && !a.disabled && (
                        <Button
                          size="sm"
                          type="button"
                          disabled={busy}
                          onClick={() => {
                            setOutcome(null);
                            setPending({ id: a.id, kind: "disable" });
                          }}
                        >
                          停用
                        </Button>
                      )}
                      {!mine && a.disabled && (
                        <Button
                          size="sm"
                          type="button"
                          disabled={busy}
                          busy={changing === a.id}
                          busyLabel="启用"
                          onClick={() => void change(a, { disabled: false }, `${a.email} 已启用`)}
                        >
                          启用
                        </Button>
                      )}
                    </span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </DataTable>
      )}
    </Card>
  );

  return (
    <>
      <PageHeader
        title="运维账号"
        eyebrow="平台访问 · 当前账号快照"
        dataLead={items === null
          ? "读取账号状态后显示当前访问概况。"
          : <>平台共有 {fmtInt(items.length)} 个运维账号，{fmtInt(enabled?.length ?? 0)} 个可用，{fmtInt(items.length - (enabled?.length ?? 0))} 个已停用。</>}
        description="管理员可以新建账号、调整角色、停用或启用。停用会立即结束该账号的全部会话；账号不会被删除，停用即收回访问。至少要保留一个可用的管理员。"
        actions={
          <>
            <Button type="button" onClick={() => void reload()}>
              <RefreshCw aria-hidden />
              刷新
            </Button>
            {!creating && (
              <Button
                kind="primary"
                type="button"
                onClick={() => {
                  setOutcome(null);
                  setCreating(true);
                }}
              >
                <Plus aria-hidden />
                新建运维账号
              </Button>
            )}
          </>
        }
      />
      <BentoGrid>
        <Card tone="inverse" className={span.d6}>
          <CardHeader number={1} title="可用账号 · 全部运维账号" />
          {enabled === null
            ? (loadError ? <CardMessage onRetry={() => void reload()}>账号状态读取失败。</CardMessage> : <MetricSkeleton />)
            : <Metric size="hero">{fmtInt(enabled.length)}</Metric>}
          <CardFooter>
            <dl className="grid grid-cols-2 gap-4 border-t border-surface/12 pt-4">
              <div>
                <dt className="font-mono text-micro text-on-inverse-muted">可用管理员</dt>
                <dd className="mt-2"><Metric size="value">{enabled === null ? "—" : fmtInt(enabled.filter((account) => account.role === "admin").length)}</Metric></dd>
              </div>
              <div>
                <dt className="font-mono text-micro text-on-inverse-muted">已停用账号</dt>
                <dd className="mt-2"><Metric size="value">{items === null || enabled === null ? "—" : fmtInt(items.length - enabled.length)}</Metric></dd>
              </div>
            </dl>
            <p className="mt-4 text-caption text-on-inverse-muted">停用会立即结束全部会话。至少保留一位可用管理员。</p>
            {loadError && items !== null && <p className="mt-3 text-caption text-on-inverse-muted">刷新失败，显示最近成功读取的状态。</p>}
          </CardFooter>
        </Card>
        <Card className={span.d6}>
          <CardHeader number={2} title="角色分布 · 全部账号" description="包含可用与已停用账号；角色决定平台操作范围。" />
          <CardFooter>
            {items === null ? (loadError
              ? <CardMessage onRetry={() => void reload()}>账号读取失败，重试后查看角色分布。</CardMessage>
              : <ChartSkeleton />)
              : <RankBars items={roles} accessibleLabel={`全部 ${fmtInt(items.length)} 个运维账号的角色分布：${roles.map((role) => `${role.label} ${role.displayValue} 个`).join("，")}`} emptyLabel="暂无运维账号" />}
          </CardFooter>
        </Card>
        {list}
        {creating && <CreateForm onCreated={created} onCancel={() => setCreating(false)} />}
      </BentoGrid>
    </>
  );
}

/** `新建运维账号`: the email, a display name, the role and an initial password. */
function CreateForm({
  onCreated,
  onCancel,
}: {
  onCreated: (email: string, role: OperatorRole) => void;
  onCancel: () => void;
}) {
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [role, setRole] = useState<OperatorRole>(NEW_ROLE);
  const [password, setPassword] = useState("");
  const [failure, setFailure] = useState<{ text: string; details: Detail[] } | null>(null);
  const [busy, setBusy] = useState(false);
  const ids = { email: useId(), name: useId(), password: useId() };
  const [form, focusRefusal] = useRefusalFocus<HTMLFormElement>();

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setFailure(null);
    setBusy(true);
    try {
      const op = await operators.create({
        email: email.trim(),
        display_name: name.trim() || undefined,
        role,
        password,
      });
      // The admin typed it; it is never shown back.
      setPassword("");
      onCreated(op.email, op.role);
    } catch (err) {
      setFailure(
        err instanceof ApiError
          ? { text: messageFor(err), details: err.details }
          : { text: err instanceof Error ? err.message : String(err), details: [] },
      );
      focusRefusal();
    } finally {
      setBusy(false);
    }
  }

  const problemAt = (path: string) =>
    failure?.details
      .filter((d) => d.path === path)
      .map((d) => d.message)
      .join("；") || null;
  const fields = { email: problemAt("/email"), name: problemAt("/display_name"), password: problemAt("/password") };
  const listed = failure?.details.filter((d) => !["/email", "/display_name", "/password"].includes(d.path)) ?? [];

  return (
    <form ref={form} className={cn("panel bento-card", span.d5Wide)} aria-label="新建运维账号" onSubmit={(e) => void submit(e)}>
      <CardHeader number={4} as="h2" title="新建运维账号" className="max-w-none" />
      <div className="grid max-w-none gap-x-4 tablet:grid-cols-2 desktop:grid-cols-1">
        <div className="min-w-0">
          <label className="f">
            <span>邮箱</span>
            <input
              type="email"
              required
              autoComplete="off"
              value={email}
              aria-invalid={fields.email ? true : undefined}
              aria-describedby={fields.email ? ids.email : undefined}
              onChange={(e) => setEmail(e.target.value)}
            />
          </label>
          {fields.email && (
            <p id={ids.email} className="field-problem">
              {fields.email}
            </p>
          )}
        </div>
        <div className="min-w-0">
          <label className="f">
            <span>显示名</span>
            <input
              value={name}
              aria-invalid={fields.name ? true : undefined}
              aria-describedby={fields.name ? ids.name : undefined}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
          {fields.name && (
            <p id={ids.name} className="field-problem">
              {fields.name}
            </p>
          )}
        </div>
        <div className="min-w-0">
          <label className="f">
            <span>角色</span>
            <select value={role} onChange={(e) => setRole(e.target.value as OperatorRole)}>
              {ROLES.map((r) => (
                <option key={r.role} value={r.role}>
                  {r.label}
                </option>
              ))}
            </select>
          </label>
          <p className="hint -mt-2 mb-4 text-caption">
            只读：只能查看；运维：可修改网关资源、租户与密钥；管理员：另可管理运维账号、网关凭据与下发。
          </p>
        </div>
        <div className="min-w-0">
          <label className="f">
            <span>初始口令</span>
            <input
              type="password"
              required
              autoComplete="new-password"
              value={password}
              aria-invalid={fields.password ? true : undefined}
              aria-describedby={fields.password ? ids.password : undefined}
              onChange={(e) => setPassword(e.target.value)}
            />
          </label>
          {fields.password && (
            <p id={ids.password} className="field-problem">
              {fields.password}
            </p>
          )}
        </div>
      </div>
      {failure && (
        <div className="note crit mt-0" role="alert">
          {failure.text}
          {listed.length > 0 && (
            <ul className="json-problems">
              {listed.map((d, i) => (
                <li key={i} data-path={d.path}>
                  <mark className="json-path">{d.path || "（请求根）"}</mark> {d.message}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      <CardFooter className="mt-4 flex flex-wrap justify-end gap-2 border-t border-ink/7 pt-4">
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
        <Button kind="primary" type="submit" busy={busy} busyLabel="创建">
          创建
        </Button>
      </CardFooter>
    </form>
  );
}
