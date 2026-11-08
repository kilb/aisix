import { useCallback, useEffect, useRef, useState } from "react";
import type { TabProps } from "../../App";
import { ApiError, repricing, resources, tenants, type RepricingRun, type Tenant } from "../../lib/api";
import { messageFor, problemAt } from "../../lib/errors";
import { errorText } from "../../lib/fmt";
import { fmtMicroUsd, fmtUtc } from "../../lib/money";
import { deltaWord, outcomeUnknown, utcInputToIso, utcInputValue } from "../../lib/repricing";
import { Button } from "../../ui/Button";
import { DataTable, Loading } from "../../ui/DataTable";
import { Field, useRefusalFocus } from "../../ui/Field";
import { Lamp } from "../../ui/Lamp";
import { TwoStep } from "../../ui/TwoStep";

const STATUS_OPTIONS: [string, string][] = [
  ["unpriced", "待计价"],
  ["charged", "已计费"],
  ["exempt", "免计费"],
  ["nothing_to_bill", "无需计费"],
  ["", "全部"],
];
const statusLabel = (s: string) => STATUS_OPTIONS.find(([v]) => v === s)?.[1] ?? s;

const APPLY_WARNING = "将按现行价格改写所选时间段内的费用，并为每个租户记一笔调整，无法撤销";

/** What the page says about the run on screen after an action. */
type Notice =
  | { runId: string; kind: "done" | "info" | "refused" | "state"; text: string }
  // The apply request got no answer that proves what happened: the run may or may not be applied.
  | { runId: string; kind: "unknown"; text: string };

interface Form {
  from: string;
  to: string;
  model_id: string;
  tenant_id: string;
  status: string;
  reason: string;
}

function startOfTodayUtc(): string {
  return `${utcInputValue(new Date()).slice(0, 10)}T00:00:00`;
}

/**
 * "重新计价": correct past charges at today's prices. An operator previews a
 * run over a window of calls and reads what it would change, in total and
 * per tenant; only an admin applies it. Applying moves money, so the page
 * sends it once (one request at a time, a two-step confirm that stays busy
 * until the answer), shows `409 stale_preview` with a way to preview again,
 * and, when the answer never arrives, does not offer the apply again until
 * the run's own status has been read.
 */
export function Repricing({ operator, canEdit, refreshShell }: TabProps) {
  const isAdmin = operator.role === "admin";
  const [form, setForm] = useState<Form>(() => ({
    from: startOfTodayUtc(),
    to: utcInputValue(new Date()),
    model_id: "",
    tenant_id: "",
    status: "unpriced",
    reason: "",
  }));
  const [problems, setProblems] = useState<Partial<Record<keyof Form, string>>>({});
  const [formError, setFormError] = useState<string | null>(null);
  const [formRef, focusRefusal] = useRefusalFocus<HTMLFormElement>();
  const [previewing, setPreviewing] = useState(false);

  const [tenantList, setTenantList] = useState<Tenant[]>([]);
  const [models, setModels] = useState<{ id: string; name: string }[]>([]);

  const [runs, setRuns] = useState<RepricingRun[] | null>(null);
  const [next, setNext] = useState<number | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  const [shown, setShown] = useState<RepricingRun | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  // Runs the control plane said are stale: they are never applied, only previewed again.
  const [stale, setStale] = useState<ReadonlySet<string>>(new Set());
  const [applying, setApplying] = useState(false);
  const [reading, setReading] = useState(false);
  // Held from before the request is sent until its answer: a second apply can never start.
  const sending = useRef(false);
  const latest = useRef(0);

  useEffect(() => {
    tenants.list().then(setTenantList, () => undefined);
    resources.list<{ display_name?: string }>("models").then(
      (items) => setModels(items.map((m) => ({ id: m.id, name: m.doc.display_name ?? m.id }))),
      () => undefined,
    );
  }, []);

  const load = useCallback(async (before?: number) => {
    const mine = ++latest.current;
    try {
      const page = await repricing.list(before);
      if (mine !== latest.current) return;
      setRuns((prev) => (before === undefined ? page.items : [...(prev ?? []), ...page.items]));
      setNext(page.next_before);
      setLoadError(null);
    } catch (e) {
      if (mine === latest.current) setLoadError(errorText(e));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const modelName = (id: string | null) => (id ? (models.find((m) => m.id === id)?.name ?? id) : null);
  const tenantName = (id: string | null) => (id ? (tenantList.find((t) => t.id === id)?.name ?? id) : null);

  function set<K extends keyof Form>(key: K, value: Form[K]) {
    setForm((f) => ({ ...f, [key]: value }));
    setProblems((p) => ({ ...p, [key]: undefined }));
  }

  /** Previews `body` and shows the stored run; false when it was refused. */
  async function runPreview(body: Parameters<typeof repricing.preview>[0]): Promise<boolean> {
    setPreviewing(true);
    setNotice(null);
    setFormError(null);
    try {
      const run = await repricing.preview(body);
      setShown(run);
      await load();
      return true;
    } catch (e) {
      if (e instanceof ApiError && e.status === 422) {
        const next = {
          from: problemAt(e.details, (p) => p === "/from"),
          to: problemAt(e.details, (p) => p === "/to"),
          status: problemAt(e.details, (p) => p === "/status"),
          reason: problemAt(e.details, (p) => p === "/reason"),
        };
        const placed = Object.values(next).some((v) => v !== null);
        setProblems(Object.fromEntries(Object.entries(next).filter(([, v]) => v !== null)));
        if (!placed) setFormError(errorText(e));
        else focusRefusal();
      } else {
        setFormError(errorText(e));
      }
      return false;
    } finally {
      setPreviewing(false);
    }
  }

  async function submitPreview() {
    if (previewing || sending.current) return;
    const from = utcInputToIso(form.from);
    const to = utcInputToIso(form.to);
    const found: Partial<Record<keyof Form, string>> = {};
    if (!from) found.from = "请填写开始时间（UTC）";
    if (!to) found.to = "请填写结束时间（UTC）";
    if (from && to && Date.parse(from) >= Date.parse(to)) found.to = "结束时间必须晚于开始时间";
    if (form.reason.trim() === "") found.reason = "请填写重新计价原因（1–500 个字符）";
    setProblems(found);
    if (!from || !to || Object.keys(found).length > 0) {
      focusRefusal();
      return;
    }
    await runPreview({
      from,
      to,
      model_id: form.model_id || undefined,
      tenant_id: form.tenant_id || undefined,
      status: form.status || undefined,
      reason: form.reason.trim(),
    });
  }

  /** Previews the same window and reason again, at today's prices. */
  async function previewAgain(run: RepricingRun) {
    if (previewing || sending.current) return;
    const f = run.filter;
    await runPreview({
      from: f.from,
      to: f.to,
      model_id: f.model_id ?? undefined,
      tenant_id: f.tenant_id ?? undefined,
      status: f.status ?? undefined,
      reason: run.reason,
    });
  }

  async function view(id: string) {
    setReading(true);
    setNotice(null);
    try {
      setShown(await repricing.get(id));
    } catch (e) {
      setNotice({ runId: id, kind: "refused", text: errorText(e) });
    } finally {
      setReading(false);
    }
  }

  /** Reads the run's own status after an apply whose outcome is unknown. */
  async function recheck(run: RepricingRun) {
    setReading(true);
    try {
      const now = await repricing.get(run.id);
      setShown(now);
      setNotice({
        runId: run.id,
        kind: "info",
        text:
          now.status === "applied"
            ? "已确认：这次重新计价已经执行。"
            : "已确认：这次重新计价尚未执行。如需执行，可以再次执行；若上一次请求仍在处理，系统只会执行一次。",
      });
      refreshShell();
      await load();
    } catch (e) {
      setNotice({
        runId: run.id,
        kind: "unknown",
        text: `执行结果仍未确认（${errorText(e)}）。请稍后再查看，确认之前不要再次执行。`,
      });
    } finally {
      setReading(false);
    }
  }

  async function apply(run: RepricingRun) {
    // One apply at a time, whatever the buttons say.
    if (sending.current) return;
    sending.current = true;
    setApplying(true);
    setNotice(null);
    try {
      const done = await repricing.apply(run.id);
      setShown(done);
      setNotice({
        runId: run.id,
        kind: "done",
        text: `已执行：${done.changed} 笔调用已改价，合计差额 ${fmtMicroUsd(done.delta_micro_usd)}（${deltaWord(done.delta_micro_usd)}）。`,
      });
      refreshShell();
      await load();
    } catch (e) {
      if (e instanceof ApiError && e.code === "stale_preview") {
        setStale((s) => new Set(s).add(run.id));
        setNotice({ runId: run.id, kind: "refused", text: messageFor(e) });
      } else if (e instanceof ApiError && e.code === "run_state") {
        setNotice({ runId: run.id, kind: "state", text: messageFor(e) });
        // Show what is stored now.
        try {
          setShown(await repricing.get(run.id));
        } catch {
          // The notice stands; the list below is read again.
        }
        await load();
      } else if (outcomeUnknown(e)) {
        setNotice({
          runId: run.id,
          kind: "unknown",
          text: `执行结果未知：没有收到控制面的确认（${errorText(e)}），这次重新计价可能已经执行。请先点「查看最新状态」读取这次运行的状态，确认之前不要再次执行。`,
        });
        await load();
      } else {
        // The control plane answered that it did nothing: the apply may be tried again.
        setNotice({ runId: run.id, kind: "refused", text: errorText(e) });
      }
    } finally {
      sending.current = false;
      setApplying(false);
    }
  }

  const busy = previewing || applying || reading;
  const mine = shown && notice && notice.runId === shown.id ? notice : null;
  const isStale = shown ? stale.has(shown.id) : false;
  const unknown = mine?.kind === "unknown";

  return (
    <div className="panel">
      <h2>重新计价</h2>
      <p className="hint">
        价格补录或改正之后，按现行价格重新计算过去的调用。先预览：看影响多少调用、各租户相差多少；确认无误后由管理员执行。预览不会改动任何费用。
      </p>

      {canEdit && (
        <form
          ref={formRef}
          aria-label="新建重新计价"
          onSubmit={(e) => {
            e.preventDefault();
            void submitPreview();
          }}
        >
          <h3>新建重新计价</h3>
          <div className="r">
            <Field label="开始（UTC）" error={problems.from}>
              {(aria) => (
                <input
                  {...aria}
                  type="datetime-local"
                  step={1}
                  value={form.from}
                  disabled={busy}
                  onChange={(e) => set("from", e.target.value)}
                />
              )}
            </Field>
            <Field label="结束（UTC）" hint="不得晚于当前时间。" error={problems.to}>
              {(aria) => (
                <input
                  {...aria}
                  type="datetime-local"
                  step={1}
                  value={form.to}
                  disabled={busy}
                  onChange={(e) => set("to", e.target.value)}
                />
              )}
            </Field>
          </div>
          <div className="r">
            <Field label="模型">
              <select value={form.model_id} disabled={busy} onChange={(e) => set("model_id", e.target.value)}>
                <option value="">全部</option>
                {models.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.name}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="租户">
              <select value={form.tenant_id} disabled={busy} onChange={(e) => set("tenant_id", e.target.value)}>
                <option value="">全部</option>
                {tenantList.map((t) => (
                  <option key={t.id} value={t.id}>
                    {t.name}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="状态" error={problems.status}>
              {(aria) => (
                <select {...aria} value={form.status} disabled={busy} onChange={(e) => set("status", e.target.value)}>
                  {STATUS_OPTIONS.map(([v, label]) => (
                    <option key={v} value={v}>
                      {label}
                    </option>
                  ))}
                </select>
              )}
            </Field>
          </div>
          <Field label="原因" hint="记入这次运行，1–500 个字符。" error={problems.reason}>
            {(aria) => (
              <input
                {...aria}
                maxLength={500}
                value={form.reason}
                disabled={busy}
                onChange={(e) => set("reason", e.target.value)}
              />
            )}
          </Field>
          {formError && (
            <div className="note crit" role="alert">
              {formError}
            </div>
          )}
          <div className="r">
            <Button type="submit" kind="primary" busy={previewing} busyLabel="预览" disabled={busy}>
              预览
            </Button>
          </div>
        </form>
      )}

      {shown && (
        <section aria-label="预览结果">
          <h3>{shown.status === "applied" ? "已执行的运行" : "预览结果"}</h3>
          <p className="hint">
            {fmtUtc(shown.filter.from)} 至 {fmtUtc(shown.filter.to)}
            {shown.filter.model_id && `，模型 ${modelName(shown.filter.model_id)}`}
            {shown.filter.tenant_id && `，租户 ${tenantName(shown.filter.tenant_id)}`}
            {`，状态 ${shown.filter.status ? statusLabel(shown.filter.status) : "全部"}`}。原因：{shown.reason}
          </p>
          <DataTable label="合计">
            <tbody>
              <tr>
                <th scope="row">选中的调用</th>
                <td className="num">{shown.events}</td>
              </tr>
              <tr>
                <th scope="row">有变化</th>
                <td className="num">{shown.changed}</td>
              </tr>
              <tr>
                <th scope="row">仍无法计价</th>
                <td className="num">{shown.still_unpriced}</td>
              </tr>
              <tr>
                <th scope="row">无归属</th>
                <td className="num">{shown.skipped_unattributed}</td>
              </tr>
              <tr>
                <th scope="row">合计差额</th>
                <td className="num money">
                  {fmtMicroUsd(shown.delta_micro_usd)}
                  <span className="cell-note">{deltaWord(shown.delta_micro_usd)}</span>
                </td>
              </tr>
            </tbody>
          </DataTable>
          <p className="hint">差额为正是多收，为负是退还。「无归属」是没有归属租户的调用，只统计，不会计费。</p>

          {shown.by_tenant.length > 0 && (
            <DataTable label="各租户差额">
              <thead>
                <tr>
                  <th>租户</th>
                  <th className="num">选中的调用</th>
                  <th className="num">有变化</th>
                  <th className="num">差额</th>
                </tr>
              </thead>
              <tbody>
                {shown.by_tenant.map((t) => (
                  <tr key={t.tenant_id}>
                    <td>{t.tenant_name || t.tenant_id}</td>
                    <td className="num">{t.events}</td>
                    <td className="num">{t.changed}</td>
                    <td className="num money">
                      {fmtMicroUsd(t.delta_micro_usd)}
                      <span className="cell-note">{deltaWord(t.delta_micro_usd)}</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </DataTable>
          )}

          {mine && (
            <div
              className={mine.kind === "done" || mine.kind === "info" ? "note ok" : "note crit"}
              role={mine.kind === "done" || mine.kind === "info" ? "status" : "alert"}
            >
              {mine.text}
            </div>
          )}

          {shown.status === "previewed" && (
            <div className="r">
              {unknown ? (
                <Button type="button" kind="primary" busy={reading} busyLabel="查看最新状态" disabled={busy} onClick={() => void recheck(shown)}>
                  查看最新状态
                </Button>
              ) : isStale ? (
                <Button type="button" kind="primary" busy={previewing} busyLabel="重新预览" disabled={busy} onClick={() => void previewAgain(shown)}>
                  重新预览
                </Button>
              ) : isAdmin ? (
                <TwoStep
                  label="执行"
                  kind="primary"
                  warning={APPLY_WARNING}
                  disabled={busy}
                  busy={applying}
                  onConfirm={() => void apply(shown)}
                />
              ) : (
                <span className="hint">执行需要管理员权限，运维账号可以预览。</span>
              )}
            </div>
          )}
          {shown.status === "previewed" && isAdmin && !unknown && !isStale && (
            <p className="hint">
              执行可能需要几秒钟。调整记入执行当月的租户流水；在用量统计里，则计入这些调用发生的月份。
            </p>
          )}
          {shown.status === "applied" && (
            <p className="hint">
              已由 {shown.applied_by?.email ?? "—"} 于 {shown.applied_at ? fmtUtc(shown.applied_at) : "—"} 执行。调整记入执行当月的租户流水；在用量统计里，则计入这些调用发生的月份。
            </p>
          )}
        </section>
      )}

      <h3>运行记录</h3>
      <div className="r">
        <Button type="button" size="sm" icon="refresh" disabled={busy} onClick={() => void load()}>
          刷新
        </Button>
      </div>
      {loadError && (
        <div className="note crit" role="alert">
          {loadError}
        </div>
      )}
      {runs === null && !loadError && <Loading />}
      {runs !== null && (
        <DataTable label="重新计价运行">
          <thead>
            <tr>
              <th>时间（UTC）</th>
              <th>范围</th>
              <th>原因</th>
              <th>状态</th>
              <th className="num">差额</th>
              <th className="right">操作</th>
            </tr>
          </thead>
          <tbody>
            {runs.length === 0 && (
              <tr>
                <td colSpan={6} className="hint">
                  还没有重新计价的记录
                </td>
              </tr>
            )}
            {runs.map((r) => (
              <tr key={r.id}>
                <td className="tabular">{fmtUtc(r.created_at)}</td>
                <td>
                  {fmtUtc(r.filter.from)} 至 {fmtUtc(r.filter.to)}
                  <span className="cell-note">
                    模型：{modelName(r.filter.model_id) ?? "全部"}；租户：{tenantName(r.filter.tenant_id) ?? "全部"}；状态：
                    {r.filter.status ? statusLabel(r.filter.status) : "全部"}
                  </span>
                </td>
                <td>{r.reason}</td>
                <td>
                  <Lamp state={r.status === "applied" ? "applied" : "pending"}>{r.status === "applied" ? "已执行" : "已预览"}</Lamp>
                </td>
                <td className="num money">{fmtMicroUsd(r.delta_micro_usd)}</td>
                <td className="right">
                  <Button size="sm" type="button" disabled={busy} onClick={() => void view(r.id)}>
                    查看
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </DataTable>
      )}
      {runs !== null && next !== null && (
        <div className="r">
          <Button type="button" disabled={busy} onClick={() => void load(next)}>
            加载更多
          </Button>
        </div>
      )}
    </div>
  );
}
