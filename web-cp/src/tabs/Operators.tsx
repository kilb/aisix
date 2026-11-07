import { useCallback, useEffect, useId, useState } from "react";
import type { TabProps } from "../App";
import { ApiError, operators, type Detail, type OperatorAccount, type OperatorRole } from "../lib/api";
import { messageFor } from "../lib/errors";
import { errorText, fmtTime } from "../lib/fmt";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { Lamp } from "../ui/Lamp";

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
    }
  }

  function created(email: string, role: OperatorRole) {
    setCreating(false);
    setOutcome({ ok: true, text: `已创建 ${email}（${roleLabel(role)}）` });
    void reload();
  }

  return (
    <>
      <div className="panel">
        <h2>运维账号</h2>
        <p className="hint">
          管理员可以新建账号、调整角色、停用或启用。停用会立即结束该账号的全部会话；账号不会被删除，停用即收回访问。至少要保留一个可用的管理员。
        </p>
        <div className="r">
          {!creating && (
            <Button
              kind="primary"
              type="button"
              onClick={() => {
                setOutcome(null);
                setCreating(true);
              }}
            >
              新建运维账号
            </Button>
          )}
          <Button type="button" icon="refresh" onClick={() => void reload()}>
            刷新
          </Button>
        </div>
        {outcome && (
          <div className={outcome.ok ? "note" : "note crit"} role={outcome.ok ? "status" : "alert"}>
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
          <DataTable>
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
                      {a.email}
                      {a.id === operator.id && <span className="hint">（当前账号）</span>}
                    </td>
                    <td>{a.display_name ?? "—"}</td>
                    <td>
                      <select
                        aria-label="角色"
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
                    <td>{fmtTime(a.created_at)}</td>
                    <td className="right">
                      {mine?.kind === "role" && (
                        <>
                          <Button
                            kind="primary"
                            size="sm"
                            type="button"
                            disabled={busy}
                            onClick={() =>
                              void change(a, { role: mine.role }, `${a.email} 已改为${roleLabel(mine.role)}`)
                            }
                          >
                            确认改为{roleLabel(mine.role)}
                          </Button>{" "}
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
                            onClick={() => void change(a, { disabled: true }, `${a.email} 已停用`)}
                          >
                            确认停用
                          </Button>{" "}
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
                          onClick={() => void change(a, { disabled: false }, `${a.email} 已启用`)}
                        >
                          启用
                        </Button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </DataTable>
        )}
      </div>

      {creating && <CreateForm onCreated={created} onCancel={() => setCreating(false)} />}
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
    <form className="panel" aria-label="新建运维账号" onSubmit={(e) => void submit(e)}>
      <h2>新建运维账号</h2>
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
      <p className="hint">只读：只能查看；运维：可修改网关资源、租户与密钥；管理员：另可管理运维账号、网关凭据与下发。</p>
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
      {failure && (
        <div className="note crit" role="alert">
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
      <div className="r">
        <Button kind="primary" type="submit" disabled={busy}>
          创建
        </Button>
        <Button type="button" onClick={onCancel}>
          取消
        </Button>
      </div>
    </form>
  );
}
