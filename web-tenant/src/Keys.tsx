import { useCallback, useEffect, useId, useState } from "react";
import {
  ApiError,
  keys as keyApi,
  members as memberApi,
  models as modelApi,
  org as readOrg,
  type Member,
  type NewKey,
  type Org,
  type PortalModel,
  type SessionMember,
  type TenantKey,
  type TenantStatus,
} from "./lib/api";
import { formMessageFor, messageFor } from "./lib/errors";
import { ConfirmButton, ShownOnce } from "./controls";

type Caps = Org["limits"];

function failure(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** A time from the API, in the viewer's own zone. */
export function when(at: string): string {
  return new Date(at).toLocaleString("zh-CN", { hour12: false });
}

/**
 * The organisation's API keys. An owner or an admin sees every key of the
 * organisation and a member their own: the control plane decides which. A
 * billing member never reaches this page and holds no key.
 *
 * While the organisation is suspended its keys do not work, and the page
 * offers revocation only: nothing that makes a key or switches one on.
 */
export function Keys({ member, status }: { member: SessionMember; status: TenantStatus }) {
  const active = status === "active";
  const manager = member.role === "owner" || member.role === "admin";
  const [items, setItems] = useState<TenantKey[] | null>(null);
  const [listErr, setListErr] = useState<string | null>(null);
  const [catalog, setCatalog] = useState<PortalModel[] | null>(null);
  const [caps, setCaps] = useState<Caps | null>(null);
  const [people, setPeople] = useState<Member[] | null>(null);
  const [readErr, setReadErr] = useState<string | null>(null);
  // The plaintext, in this page's state alone: it goes with a reload or a
  // change of page, as the notice says.
  const [minted, setMinted] = useState<string | null>(null);
  const [actionErr, setActionErr] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setItems(await keyApi.list());
      setListErr(null);
    } catch (e) {
      setListErr(failure(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The caps hold in any state; the models and the members feed the form,
  // which only an active organisation has.
  useEffect(() => {
    const failed = (e: unknown) => setReadErr(failure(e));
    readOrg().then((o) => setCaps(o.limits), failed);
    if (!active) return;
    modelApi.list().then(setCatalog, failed);
    if (manager) memberApi.list().then(setPeople, failed);
  }, [active, manager]);

  /** One change of key `k`; the list is read again whatever the answer. */
  async function act(k: TenantKey, change: () => Promise<unknown>) {
    setBusy(k.id);
    setActionErr(null);
    try {
      await change();
    } catch (e) {
      setActionErr(
        e instanceof ApiError && e.code === "version_mismatch"
          ? "该密钥已被他人修改，列表已重新载入，请再操作一次"
          : failure(e),
      );
    } finally {
      setBusy(null);
    }
    await load();
  }

  function ownerOf(k: TenantKey): string {
    if (!k.owner) return "组织";
    if (k.owner.id === member.id) return "我";
    const id = k.owner.id;
    return k.owner.display_name ?? people?.find((p) => p.id === id)?.email ?? "成员";
  }

  function modelsOf(k: TenantKey): string {
    if (k.model_ids === null) return "全部授权模型";
    if (k.model_ids.length === 0) return "无";
    if (!catalog) return `${k.model_ids.length} 个模型`;
    const names = k.model_ids.flatMap((id) => catalog.find((m) => m.id === id)?.name ?? []);
    const gone = k.model_ids.length - names.length;
    return [...names, ...(gone > 0 ? [`${gone} 个已不在授权内`] : [])].join("、");
  }

  function limitsOf(k: TenantKey): string {
    // A limit the key leaves unset follows the organisation's cap.
    const one = (label: string, own?: number, cap?: number) =>
      own !== undefined ? `${label} ${own}` : cap !== undefined ? `${label} ≤${cap}` : null;
    const parts = [
      one("并发", k.rate_limit.concurrency, caps?.key_concurrency_max),
      one("每分钟", k.rate_limit.rpm, caps?.key_rpm_max),
    ].filter((p) => p !== null);
    return parts.length > 0 ? parts.join(" · ") : "不限";
  }

  return (
    <div className="stack">
      {active && (
        <NewKeyForm
          me={member}
          manager={manager}
          catalog={catalog}
          caps={caps}
          people={people}
          readErr={readErr}
          onCreated={(plaintext) => {
            setMinted(plaintext);
            void load();
          }}
        />
      )}
      <div className="panel">
        <h2>密钥</h2>
        {!active && <div className="note crit">组织已被停用，所有密钥暂时无法调用；这里只能吊销密钥。</div>}
        {minted && (
          <ShownOnce title="明文只显示这一次" value={minted} onDone={() => setMinted(null)}>
            <span className="hint">请立即复制并妥善保存；关闭、刷新或离开此页后无法再次查看。</span>
          </ShownOnce>
        )}
        {listErr && (
          <div className="note crit" role="alert">
            {listErr}
          </div>
        )}
        {actionErr && (
          <div className="note crit" role="alert">
            {actionErr}
          </div>
        )}
        {items === null ? (
          !listErr && <p className="hint">读取中…</p>
        ) : items.length === 0 ? (
          <p className="hint">还没有密钥。</p>
        ) : (
          <div className="scroll">
            <table>
              <thead>
                <tr>
                  <th>名称</th>
                  <th>前缀</th>
                  <th>归属</th>
                  <th>模型</th>
                  <th>限制</th>
                  <th>状态</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {items.map((k) => {
                  const state = stateOf(k);
                  return (
                    <tr key={k.id}>
                      <td>{k.label}</td>
                      <td className="num">{k.prefix}…</td>
                      <td>{ownerOf(k)}</td>
                      <td>{modelsOf(k)}</td>
                      <td>{limitsOf(k)}</td>
                      <td>
                        {state.text}
                        {state.note && <span className="hint cell-note">{state.note}</span>}
                      </td>
                      <td className="right">
                        <span className="acts">
                          {/* The organisation's own switch, which the platform's overrides. */}
                          {active && !k.disabled_by_platform && (
                            <button
                              className="ghost"
                              disabled={busy === k.id}
                              onClick={() => void act(k, () => keyApi.setDisabled(k.id, !k.disabled, k.version))}
                            >
                              {k.disabled ? "启用" : "停用"}
                            </button>
                          )}
                          <ConfirmButton
                            label="吊销"
                            warning="吊销后立即失效，且无法恢复"
                            disabled={busy === k.id}
                            onConfirm={() => void act(k, () => keyApi.revoke(k.id, k.version))}
                          />
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

function stateOf(k: TenantKey): { text: string; note?: string } {
  if (k.disabled_by_platform) return { text: "已被平台停用", note: "只有平台可以重新启用" };
  if (k.disabled) return { text: "已停用" };
  if (k.expires_at && Date.parse(k.expires_at) <= Date.now()) return { text: "已过期" };
  return { text: "可用", note: k.expires_at ? `${when(k.expires_at)} 到期` : undefined };
}

type Field = "label" | "models" | "concurrency" | "rpm" | "expires" | "owner";
type FieldErrors = Partial<Record<Field, string>>;

/** The form field a refusal's JSON path names, or null for one the form has no field for. */
function fieldOf(path: string): Field | null {
  if (path === "/label") return "label";
  if (path === "/model_ids" || path.startsWith("/model_ids/")) return "models";
  if (path === "/rate_limit/concurrency") return "concurrency";
  if (path === "/rate_limit/rpm") return "rpm";
  if (path === "/expires_at") return "expires";
  if (path === "/member_id") return "owner";
  return null;
}

/** A limit as typed: none when empty, a whole number, or why it is neither. */
function limit(raw: string): { value?: number; problem?: string } {
  const text = raw.trim();
  if (text === "") return {};
  if (!/^\d+$/.test(text)) return { problem: "请填整数；留空表示不单独限制" };
  return { value: Number(text) };
}

function capHint(cap: number | undefined, caps: Caps | null): string {
  if (caps === null) return "组织上限读取中…";
  return cap === undefined ? "组织未设上限；留空表示不限" : `组织上限 ${cap}；留空则按此上限`;
}

function NewKeyForm({
  me,
  manager,
  catalog,
  caps,
  people,
  readErr,
  onCreated,
}: {
  me: SessionMember;
  manager: boolean;
  catalog: PortalModel[] | null;
  caps: Caps | null;
  people: Member[] | null;
  readErr: string | null;
  onCreated: (plaintext: string) => void;
}) {
  const id = useId();
  const [label, setLabel] = useState("");
  const [picked, setPicked] = useState<string[]>([]);
  const [concurrency, setConcurrency] = useState("");
  const [rpm, setRpm] = useState("");
  const [expires, setExpires] = useState("");
  // Empty: the caller.
  const [owner, setOwner] = useState("");
  const [errs, setErrs] = useState<FieldErrors>({});
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setErr(null);
    const found: FieldErrors = {};
    const c = limit(concurrency);
    const r = limit(rpm);
    if (c.problem) found.concurrency = c.problem;
    if (r.problem) found.rpm = r.problem;
    let expiresAt: string | undefined;
    if (expires) {
      const at = new Date(expires);
      if (Number.isNaN(at.getTime())) found.expires = "时间格式不正确";
      else expiresAt = at.toISOString();
    }
    setErrs(found);
    if (Object.keys(found).length > 0) return;

    // Only what was set is sent: no model picked follows every grant, and a
    // limit left empty follows the organisation's cap.
    const body: NewKey = { label };
    if (picked.length > 0) body.model_ids = picked;
    if (c.value !== undefined || r.value !== undefined) body.rate_limit = { concurrency: c.value, rpm: r.value };
    if (expiresAt) body.expires_at = expiresAt;
    if (manager && owner) body.member_id = owner;
    setBusy(true);
    try {
      const { plaintext } = await keyApi.create(body);
      setLabel("");
      setPicked([]);
      setConcurrency("");
      setRpm("");
      setExpires("");
      setOwner("");
      onCreated(plaintext);
    } catch (e) {
      if (e instanceof ApiError && e.code === "validation_failed" && e.details.length > 0) {
        // Each refusal beside its field; one the form has no field for above the button.
        const byField: FieldErrors = {};
        const rest: string[] = [];
        for (const d of e.details) {
          const f = fieldOf(d.path);
          if (f) byField[f] = byField[f] ? `${byField[f]}；${d.message}` : d.message;
          else rest.push(d.message);
        }
        setErrs(byField);
        if (rest.length > 0) setErr(rest.join("；"));
      } else if (e instanceof ApiError && e.code === "unavailable") {
        setErr("平台暂时无法创建密钥，请稍后再试");
      } else {
        setErr(e instanceof ApiError ? formMessageFor(e) : String(e));
      }
    } finally {
      setBusy(false);
    }
  }

  const hintId = (f: Field) => `${id}-${f}-hint`;
  const errId = (f: Field) => `${id}-${f}-err`;
  /** The ids describing field `f`: its hint when it has one, then its refusal. */
  const describedBy = (f: Field, hinted: boolean) =>
    [hinted ? hintId(f) : null, errs[f] ? errId(f) : null].filter((x) => x !== null).join(" ") || undefined;
  const invalid = (f: Field) => (errs[f] ? true : undefined);
  const refusal = (f: Field) =>
    errs[f] && (
      <p className="field-err" id={errId(f)} role="alert">
        {errs[f]}
      </p>
    );
  const choices = (people ?? []).filter((p) => p.id !== me.id && p.role !== "billing");

  return (
    <div className="panel">
      <h2>新建密钥</h2>
      {readErr && (
        <div className="note crit" role="alert">
          {readErr}
        </div>
      )}
      <form onSubmit={submit} noValidate>
        <label className="f">
          <span>名称</span>
          <input
            value={label}
            onChange={(e) => setLabel(e.target.value)}
            aria-invalid={invalid("label")}
            aria-describedby={describedBy("label", false)}
          />
        </label>
        {refusal("label")}

        <fieldset className="f" aria-describedby={describedBy("models", true)} aria-invalid={invalid("models")}>
          <legend>可用模型</legend>
          {catalog === null ? (
            <span className="hint">读取中…</span>
          ) : catalog.length === 0 ? (
            <span className="hint">组织还没有获授权的模型</span>
          ) : (
            catalog.map((m) => (
              <label key={m.id} className="pick">
                <input
                  type="checkbox"
                  checked={picked.includes(m.id)}
                  onChange={(e) =>
                    setPicked(e.target.checked ? [...picked, m.id] : picked.filter((p) => p !== m.id))
                  }
                />
                {m.name}
              </label>
            ))
          )}
        </fieldset>
        <p className="hint" id={hintId("models")}>
          不勾选即可使用组织获授权的全部模型，包括以后新授权的
        </p>
        {refusal("models")}

        <label className="f">
          <span>并发</span>
          <input
            inputMode="numeric"
            value={concurrency}
            onChange={(e) => setConcurrency(e.target.value)}
            aria-invalid={invalid("concurrency")}
            aria-describedby={describedBy("concurrency", true)}
          />
        </label>
        <p className="hint" id={hintId("concurrency")}>
          {capHint(caps?.key_concurrency_max, caps)}
        </p>
        {refusal("concurrency")}

        <label className="f">
          <span>每分钟请求</span>
          <input
            inputMode="numeric"
            value={rpm}
            onChange={(e) => setRpm(e.target.value)}
            aria-invalid={invalid("rpm")}
            aria-describedby={describedBy("rpm", true)}
          />
        </label>
        <p className="hint" id={hintId("rpm")}>
          {capHint(caps?.key_rpm_max, caps)}
        </p>
        {refusal("rpm")}

        <label className="f">
          <span>过期时间</span>
          <input
            type="datetime-local"
            value={expires}
            onChange={(e) => setExpires(e.target.value)}
            aria-invalid={invalid("expires")}
            aria-describedby={describedBy("expires", true)}
          />
        </label>
        <p className="hint" id={hintId("expires")}>
          留空表示永不过期；按本机时区
        </p>
        {refusal("expires")}

        {manager && (
          <>
            <label className="f">
              <span>归属成员</span>
              <select
                value={owner}
                onChange={(e) => setOwner(e.target.value)}
                aria-invalid={invalid("owner")}
                aria-describedby={describedBy("owner", false)}
              >
                <option value="">我</option>
                {choices.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.display_name ?? p.email}
                    {p.disabled ? "（已停用）" : ""}
                  </option>
                ))}
              </select>
            </label>
            {refusal("owner")}
          </>
        )}

        {err && (
          <div className="note crit" role="alert">
            {err}
          </div>
        )}
        <button className="act narrow" type="submit" disabled={busy}>
          {busy ? "处理中…" : "新建密钥"}
        </button>
      </form>
    </div>
  );
}
