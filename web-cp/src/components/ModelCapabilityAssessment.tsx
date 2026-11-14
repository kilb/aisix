import { Download, FileSearch, Play, RefreshCw, Square } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { ApiError, type ResourceView } from "../lib/api";
import {
  ASSESSMENT_DIMENSIONS, ASSESSMENT_STATES, assessmentCost, assessmentDelta, assessmentPercent,
  assessmentSelectionError, assessmentTargets, assessmentVerdict, assessmentRequestId, canCancelAssessment, isAssessmentActive,
  type AssessmentMode, type AssessmentRun, type AssessmentRunSummary, type AssessmentSample,
  type AssessmentSource, type AssessmentTargetInput,
} from "../lib/modelAssessment";
import { cancelAssessment, exportAssessment, getAssessment, listAssessments, listAssessmentSamples, planAssessment, startAssessment, type AssessmentPage } from "../lib/modelAssessmentApi";
import { beijingInputValue, fmtBeijing, parseBeijingInput } from "../lib/time";
import { useHashQuery } from "../lib/useHashQuery";
import type { AccountDoc, ModelDoc } from "../tabs/models/catalog";
import { FormDialog } from "../ui/FormDialog";
import { Button } from "../ui/Button";
import { Field } from "../ui/Field";
import { RecordTablePager, type RecordPageSize } from "../ui/RecordTable";
import { OperationalTable, Loading } from "./OperationalTable";
import { Card, CardHeader } from "./page";
import "./ModelCapabilityAssessment.css";

const completionLabels: Record<NonNullable<AssessmentRun["completion_reason"]>, string> = {
  all_scheduled: "固定计划全部结束", cancelled: "操作员取消", deadline: "达到总时限", budget: "达到请求预算", configuration_changed: "资源配置发生变化", owner_lost: "执行器失联",
};
const resultLabels: Record<AssessmentSample["result"], string> = { pass: "答对", fail: "答错", error: "调用失败", unsupported: "不支持", not_run: "未执行" };
const dispatchLabels: Record<AssessmentSample["dispatch_state"], string> = { not_sent: "未发送", reserved: "已预留，发送待确认", sent: "已知发送", finished: "已结束", cancelled_before_send: "发送前取消", aborted_after_send: "发送后中断", interrupted_unknown: "中断，发送未知" };
function errorMessage(error: unknown): string {
  if (error instanceof ApiError) {
    const prefix = error.status === 403 ? "当前会话没有能力对照权限。" : error.status === 409 ? "计划已过期、配置已改变，或运行状态冲突；请刷新后重新生成计划。" : "";
    return `${prefix}${error.message}${error.details.length ? `；${error.details.map((detail) => `${detail.path}：${detail.message}`).join("；")}` : ""}`;
  }
  return error instanceof Error ? error.message : "请求失败，请刷新查看持久状态。";
}

/** 游标来自服务端，只能前往已取得的下一页，不虚构总量。 */
function useAssessmentPage<T>(load: (limit: number, cursor: string | undefined, signal: AbortSignal) => Promise<AssessmentPage<T>>, revision: number | string) {
  const [pageSize, setPageSize] = useState<RecordPageSize>(25);
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const [page, setPage] = useState(1);
  const [data, setData] = useState<AssessmentPage<T> | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cursor = cursors[page - 1];
  useEffect(() => {
    const controller = new AbortController();
    setBusy(true);
    setError(null);
    setData(null);
    void load(pageSize, cursor, controller.signal).then((next) => {
      if (!controller.signal.aborted) setData(next);
    }).catch((failure: unknown) => {
      if (!controller.signal.aborted) setError(errorMessage(failure));
    }).finally(() => { if (!controller.signal.aborted) setBusy(false); });
    return () => controller.abort();
  }, [load, cursor, pageSize, revision]);
  return {
    data, busy, error,
    pager: {
      page, pageSize, busy, loadedCount: data?.items.length ?? 0, hasNext: data?.next_cursor != null,
      onPageChange(next: number) {
        if (next > page && data?.next_cursor) setCursors((previous) => [...previous.slice(0, page), data.next_cursor!]);
        setPage(next);
      },
      onPageSizeChange(size: RecordPageSize) { setPageSize(size); setPage(1); setCursors([undefined]); },
    },
  };
}

/** 只在 Admin 入口挂载；浏览、计划、历史、详情及导出都不调用模型。 */
export function ModelCapabilityAssessment({ models, accounts, loadError }: {
  models: readonly ResourceView<ModelDoc>[] | null;
  accounts: readonly ResourceView<AccountDoc>[] | null;
  loadError: boolean;
}) {
  const targets = useMemo(() => assessmentTargets(models ?? [], accounts ?? []), [models, accounts]);
  const [query, updateQuery] = useHashQuery();
  const selectedId = query.get("assessment_run") ?? "";
  const [planOpen, setPlanOpen] = useState(false);
  useEffect(() => { if (selectedId) setPlanOpen(false); }, [selectedId]);
  const [referenceId, setReferenceId] = useState("");
  const [candidateId, setCandidateId] = useState("");
  const [kind, setKind] = useState<AssessmentSource["kind"]>("direct_provider");
  const [note, setNote] = useState("");
  const [verifiedAt, setVerifiedAt] = useState("");
  const [independent, setIndependent] = useState(false);
  const [mode, setMode] = useState<AssessmentMode>("quick");
  const [run, setRun] = useState<AssessmentRun | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [readError, setReadError] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const [acceptUnknown, setAcceptUnknown] = useState(false);
  const startAttempt = useRef<{ runId: string; requestId: string; accept: boolean } | null>(null);
  const writing = useRef(false);
  const readVersion = useRef(0);
  const lifetime = useRef<AbortController | null>(null);
  const reference = targets.find((target) => target.model_id === referenceId);
  const candidate = targets.find((target) => target.model_id === candidateId);
  const selectionError = assessmentSelectionError(reference, candidate, independent);
  const currentRun = run?.id === selectedId ? run : null;

  useEffect(() => {
    const controller = new AbortController();
    lifetime.current = controller;
    return () => controller.abort();
  }, []);

  // 刷新和可分享链接恢复持久运行；本地离页只停止读取，不取消服务端任务。
  useEffect(() => {
    if (!selectedId) return;
    const controller = new AbortController();
    const version = ++readVersion.current;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setReadError(null);
    setAcceptUnknown(startAttempt.current?.runId === selectedId ? startAttempt.current.accept : false);
    async function read() {
      try {
        const latest = await getAssessment(selectedId, controller.signal);
        if (controller.signal.aborted || version !== readVersion.current) return;
        setRun(latest);
        setReadError(null);
        if (isAssessmentActive(latest.state)) timer = setTimeout(() => void read(), 1000);
      } catch (failure) {
        if (!controller.signal.aborted && version === readVersion.current) {
          setReadError(`无法读取最新进度，请勿据此认定任务已停止。${errorMessage(failure)}`);
          if (!(failure instanceof ApiError && [401, 403, 404].includes(failure.status))) timer = setTimeout(() => void read(), 1000);
        }
      }
    }
    void read();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [selectedId, revision]);

  async function write(action: (signal: AbortSignal) => Promise<AssessmentRun>) {
    if (writing.current || !lifetime.current) return;
    writing.current = true;
    readVersion.current += 1;
    setBusy(true);
    setError(null);
    try {
      const next = await action(lifetime.current.signal);
      if (lifetime.current.signal.aborted) return;
      setRun(next);
      updateQuery({ assessment_run: next.id });
    } catch (failure) {
      if (!lifetime.current.signal.aborted) setError(errorMessage(failure));
    } finally {
      writing.current = false;
      if (!lifetime.current.signal.aborted) {
        setBusy(false);
        setRevision((value) => value + 1);
      }
    }
  }

  function plan(event: FormEvent) {
    event.preventDefault();
    const verified = parseBeijingInput(verifiedAt);
    if (selectionError || !reference || !candidate || !note.trim() || !verified || Date.parse(verified) > Date.now()) {
      setError(selectionError ?? "请填写参考来源核实说明和有效的核实时间（北京时间，不能晚于当前时间）。");
      return;
    }
    const input = (target: AssessmentTargetInput): AssessmentTargetInput => ({ model_id: target.model_id, model_version: target.model_version, provider_key_version: target.provider_key_version });
    startAttempt.current = null;
    setAcceptUnknown(false);
    void write((signal) => planAssessment({ reference: input(reference), candidate: input(candidate), reference_source: { kind, note: note.trim(), verified_at: verified, independence_confirmed: true }, mode, suite_id: "capability-v1", suite_version: "1" }, signal));
  }

  function start() {
    if (!currentRun || currentRun.state !== "planned") return;
    if (startAttempt.current?.runId !== currentRun.id) startAttempt.current = { runId: currentRun.id, requestId: assessmentRequestId(), accept: acceptUnknown };
    const attempt = startAttempt.current;
    void write((signal) => startAssessment(currentRun.id, attempt.requestId, attempt.accept, signal));
  }

  async function download() {
    if (!currentRun || writing.current || !lifetime.current) return;
    writing.current = true;
    setBusy(true);
    setError(null);
    try {
      const evidence = await exportAssessment(currentRun.id, lifetime.current.signal);
      if (lifetime.current.signal.aborted) return;
      const url = URL.createObjectURL(new Blob([JSON.stringify(evidence, null, 2)], { type: "application/json" }));
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = `model-assessment-${currentRun.id}${evidence.partial ? "-partial" : ""}.json`;
      anchor.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (failure) {
      if (!lifetime.current.signal.aborted) setError(`导出失败。${errorMessage(failure)}`);
    } finally {
      writing.current = false;
      if (!lifetime.current.signal.aborted) setBusy(false);
    }
  }

  return <section className="model-assessment" id="model-tests-assessment" role="tabpanel" aria-labelledby="model-tests-assessment-tab">
    <div className="assessment-create-toolbar"><p className="assessment-help">Quick：20 次生成调用；Full：160 次生成调用。生成计划（不调用模型）后，审核预算再显式开始。</p><Button kind="primary" icon="plus" onClick={() => setPlanOpen(true)}>新建能力对照</Button></div>
    <FormDialog open={planOpen} onOpenChange={setPlanOpen} title="新建能力对照" description="在固定合成题集内比较两个渠道的能力。结果不认证模型身份。" busy={busy}>
    <Card>
      <CardHeader title="新建能力对照" description="在固定合成题集内对照两个渠道的能力。来源独立性由操作员审定，结果不认证模型身份。" />
      <form onSubmit={plan}>
        <fieldset disabled={busy || loadError || models === null || accounts === null}>
          <legend className="vh">参考与被测</legend>
          <div className="assessment-form-grid">
            <Field label="可信参考模型" hint="只可选择已保存的具体模型及其账号。">{(aria) => <select {...aria} required value={referenceId} onChange={(event) => setReferenceId(event.target.value)}><option value="">选择参考模型</option>{targets.map((target) => <option key={target.model_id} value={target.model_id}>{target.model_name} · {target.account_name}</option>)}</select>}</Field>
            <Field label="被测模型" hint="须与参考使用不同模型、不同账号；账号不同不证明渠道独立。">{(aria) => <select {...aria} required value={candidateId} onChange={(event) => setCandidateId(event.target.value)}><option value="">选择被测模型</option>{targets.map((target) => <option key={target.model_id} value={target.model_id}>{target.model_name} · {target.account_name}</option>)}</select>}</Field>
            <Field label="参考来源"><select value={kind} onChange={(event) => setKind(event.target.value as AssessmentSource["kind"])}><option value="direct_provider">官方直连</option><option value="verified_channel">已核实的独立渠道</option></select></Field>
            <Field label="来源核实时间（北京时间）"><input required type="datetime-local" value={verifiedAt} max={beijingInputValue(new Date()).slice(0, 16)} onChange={(event) => setVerifiedAt(event.target.value)} /></Field>
            <Field label="来源核实说明" hint="说明参考渠道和核实依据，最多256字；请勿填入密钥或含凭据的地址。">{(aria) => <textarea {...aria} required maxLength={256} rows={2} value={note} onChange={(event) => setNote(event.target.value)} />}</Field>
            <Field label="固定题集模式"><select value={mode} onChange={(event) => setMode(event.target.value as AssessmentMode)}><option value="quick">Quick · 10 对 / 20 次生成调用 · 仅描述</option><option value="full">Full · 80 对 / 160 次生成调用</option></select></Field>
          </div>
          <label className="assessment-checkbox"><input type="checkbox" checked={independent} onChange={(event) => setIndependent(event.target.checked)} /><span>我已核实参考来源，并审定参考与被测的供应渠道独立性。</span></label>
        </fieldset>
        {loadError ? <p className="note crit" role="alert">模型或账号资源加载失败，请刷新资源后再生成计划。</p> : models === null || accounts === null ? <p role="status">正在加载可选模型与账号…</p> : targets.length < 2 ? <p className="note warn">需要至少两个已保存的具体模型与不同账号。</p> : null}
        {selectionError && reference && candidate ? <p className="assessment-help">{selectionError}</p> : null}
        <p className="assessment-help">Quick：20 次生成调用；Full：160 次生成调用。生成计划不调用模型；审核冻结预算与费用后，还需显式开始。工具题只评第一轮选择与参数，不运行工具；代码题不执行返回代码。</p>
        <Button type="submit" kind="primary" busy={busy} busyLabel="请求处理中…" disabled={Boolean(selectionError) || !note.trim() || !verifiedAt || loadError || models === null || accounts === null}><FileSearch aria-hidden="true" />生成计划（不调用模型）</Button>
      </form>
    </Card>
      {error && <p className="note crit" role="alert">{error}</p>}
    </FormDialog>
    {!planOpen && error && <p className="note crit" role="alert">{error}{startAttempt.current && <> 开始响应未确认时，请刷新进度或重试相同开始请求；不要据此认定没有产生费用。</>}</p>}
    {readError && <p className="note crit" role="alert">{readError}<Button kind="quiet" onClick={() => setRevision((value) => value + 1)}><RefreshCw aria-hidden="true" />重新读取</Button></p>}
    {selectedId && !currentRun && !readError && <Loading />}
    {currentRun && <>
      <AssessmentRunPanel run={currentRun} busy={busy} acceptUnknown={acceptUnknown} acceptLocked={startAttempt.current?.runId === currentRun.id} onAcceptUnknown={setAcceptUnknown} onStart={start} onCancel={() => void write((signal) => cancelAssessment(currentRun.id, signal))} onRefresh={() => setRevision((value) => value + 1)} onExport={() => void download()} />
      <AssessmentSamples key={currentRun.id} run={currentRun} />
    </>}
    <AssessmentHistory revision={`${revision}:${currentRun?.state ?? ""}`} busy={busy} selectedId={selectedId} onSelect={(id) => { setError(null); updateQuery({ assessment_run: id }); }} />
  </section>;
}

export function AssessmentRunPanel({ run, busy, acceptUnknown, acceptLocked, onAcceptUnknown, onStart, onCancel, onRefresh, onExport }: {
  run: AssessmentRun;
  busy: boolean;
  acceptUnknown: boolean;
  acceptLocked: boolean;
  onAcceptUnknown: (accept: boolean) => void;
  onStart: () => void;
  onCancel: () => void;
  onRefresh: () => void;
  onExport: () => void;
}) {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (run.state !== "planned") return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [run.state]);
  const expired = Date.parse(run.expires_at) <= now;
  const unknown = run.cost_status === "unknown" || run.estimated_cost === null;
  return <Card className="assessment-run" aria-label="能力对照运行详情">
    <CardHeader title="计划与运行" description={<><span>{ASSESSMENT_STATES[run.state]} · {run.mode === "quick" ? "Quick（仅描述）" : "Full"}</span><code className="assessment-run-id">{run.id}</code></>} actions={<><Button kind="quiet" iconOnly aria-label="刷新对照进度" title="刷新对照进度" disabled={busy} onClick={onRefresh}><RefreshCw aria-hidden="true" /></Button><Button disabled={busy} onClick={onExport}><Download aria-hidden="true" />下载可重评 JSON</Button></>} />
    <div className="assessment-targets">
      {(["reference", "candidate"] as const).map((role) => <div key={role}><h3>{role === "reference" ? "可信参考" : "被测"}</h3><strong>{run[role].model_name}</strong><span>{run[role].account_name} · {run[role].provider} · {run[role].adapter}</span><span>实际模型：{run[role].configured_model}</span><code>{run[role].endpoint_display}</code><span>模型 revision {run[role].model_version} · 账号 revision {run[role].provider_key_version}</span></div>)}
    </div>
    <dl className="assessment-budget">
      <div><dt>生成调用硬上限</dt><dd>{run.limits.max_requests} 次</dd></div>
      <div><dt>每次输出参数上限</dt><dd>{run.limits.max_output_tokens_per_request} tokens</dd></div>
      <div><dt>每次输入 / 响应上限</dt><dd>{run.limits.max_input_bytes_per_request} / {run.limits.max_response_bytes_per_request} 字节</dd></div>
      <div><dt>单次 / 总时限</dt><dd>{run.limits.request_timeout_seconds} / {run.limits.run_timeout_seconds} 秒</dd></div>
      <div><dt>运行 / 每账号在途上限</dt><dd>{run.limits.concurrency} / {run.limits.per_account_concurrency}</dd></div>
      <div><dt>费用</dt><dd>{assessmentCost(run)}</dd></div>
    </dl>
    {run.estimated_cost && <p className="assessment-help">成本价格 revision：{run.estimated_cost.price_revision}。估算金额不是远端收费硬上限。</p>}
    <p className="assessment-help">硬预算约束本地派发与等待，不能保证上游停止生成或计费。取消可能仍产生费用，不自动重试、不切换渠道、不追加样本。</p>
    <details className="assessment-metadata"><summary>冻结题集与参考来源</summary><dl><div><dt>题集</dt><dd>{run.suite.id} / {run.suite.version}</dd></div><div><dt>Manifest hash</dt><dd><code>{run.suite.manifest_hash}</code></dd></div><div><dt>生成器 / 评分器</dt><dd>{run.suite.generator_version} / {run.suite.grader_version}</dd></div><div><dt>调度 seed</dt><dd><code>{run.suite.seed}</code></dd></div><div><dt>参考来源</dt><dd>{run.reference_source.kind === "direct_provider" ? "官方直连" : "已核实的独立渠道"} · {fmtBeijing(run.reference_source.verified_at)}（北京时间）</dd></div><div><dt>来源说明</dt><dd>{run.reference_source.note}</dd></div><div><dt>独立性</dt><dd>{run.reference_source.independence_confirmed ? "操作员已审定；不证明供应链独立" : "未审定"}</dd></div></dl></details>
    {run.state === "planned" && <div className="assessment-start">
      <p>计划有效至 {fmtBeijing(run.expires_at)}（北京时间）。{expired ? "已过期，请另建计划。" : "当前未开始；仅点击下方开始按钮才授权模型调用。"}</p>
      {unknown && <label className="assessment-checkbox"><input type="checkbox" checked={acceptUnknown} disabled={busy || acceptLocked || expired} onChange={(event) => onAcceptUnknown(event.target.checked)} /><span>我接受费用未知，授权此计划最多 {run.limits.max_requests} 次生成调用。</span></label>}
      <Button kind="primary" disabled={busy || expired || (unknown && !acceptUnknown)} onClick={onStart}><Play aria-hidden="true" />{unknown ? "开始对照（费用未知）" : "开始对照（按估算费用）"}</Button>
    </div>}
    {canCancelAssessment(run.state) && <div className="assessment-actions"><Button disabled={busy || run.cancel_requested_at !== null} onClick={onCancel}><Square aria-hidden="true" />取消对照</Button>{run.cancel_requested_at && <span role="status">取消已请求；等待持久状态更新，远端可能继续计费。</span>}</div>}
    <div className="assessment-progress" role="status" aria-live="polite" aria-atomic="true">{ASSESSMENT_STATES[run.state]} · 已结束 {run.finished_requests} / {run.planned_requests} 次{run.completion_reason && <> · {completionLabels[run.completion_reason]}</>}</div>
    <progress className="assessment-progress-bar" aria-label="已结束的固定计划调用" max={Math.max(1, run.planned_requests)} value={run.finished_requests} />
    <dl className="assessment-budget assessment-counts"><div><dt>已预留（不可退回、潜在收费）</dt><dd>{run.reserved_requests}</dd></div><div><dt>已知发送</dt><dd>{run.sent_requests}</dd></div><div><dt>未知发送</dt><dd>{run.unknown_requests}</dd></div></dl>
    <p className="assessment-help">未知发送可能已经计费；已知发送为 0 不表示没有费用。运行中、取消、中断和提前结束都只展示描述，不能推断差异。</p>
    <h3 className="assessment-result-title">题集内结果：{assessmentVerdict(run.verdict)}</h3>
    <p className="assessment-help">结果不代表模型身份真伪；无方向性证据不代表等价，不形成认证总分。Quick 始终仅描述、证据不足。题库和阈值尚未完成真实校准，可能漏检温和差异或混用。</p>
    {run.reason_codes.length > 0 && <p className="assessment-help">服务端原因：{run.reason_codes.join("、")}</p>}
    {run.warnings.length > 0 && <ul className="assessment-warnings">{run.warnings.map((warning, index) => <li key={index}>{warning}</li>)}</ul>}
    {run.dimensions.length > 0 && <OperationalTable label="能力对照各维报告"><thead><tr><th>维度 / 有效覆盖</th><th>双方正确率</th><th>成对差异</th><th>服务端判定</th></tr></thead><tbody>{run.dimensions.map((dimension) => <tr key={dimension.dimension}>
      <th scope="row">{ASSESSMENT_DIMENSIONS[dimension.dimension]}<span className="assessment-secondary">有效独立簇 {dimension.valid_clusters} / {dimension.planned_clusters}</span><span className="assessment-secondary">有效配对 {dimension.valid_pairs} · 覆盖 {assessmentPercent(dimension.paired_coverage)}</span></th>
      <td>参考 {assessmentPercent(dimension.reference_score)}<br />被测 {assessmentPercent(dimension.candidate_score)}<span className="assessment-secondary">只描述有效簇；覆盖不足保留在左列。</span></td>
      <td>{assessmentDelta(dimension.delta)}<span className="assessment-secondary">被测 − 参考</span><span className="assessment-secondary">参考胜 {dimension.reference_wins} / 被测胜 {dimension.candidate_wins} / 平 {dimension.ties}</span></td>
      <td><strong>{assessmentVerdict(dimension.verdict)}</strong><span className="assessment-secondary">p：{dimension.p_one_sided === null ? "—" : String(dimension.p_one_sided)} · 阈值 {dimension.p_threshold}</span>{dimension.reason_codes.length > 0 && <span className="assessment-secondary">{dimension.reason_codes.join("、")}</span>}</td>
    </tr>)}</tbody></OperationalTable>}
    <p className="assessment-help">JSON 含冻结合成输入、答案约束、评分器与脱敏后的完整规范化回答，可独立复核。未结束时下载的是 partial 快照；答案脱敏标记为 true 的样本不能逐字重评。</p>
  </Card>;
}

function AssessmentHistory({ revision, busy, selectedId, onSelect }: { revision: number | string; busy: boolean; selectedId: string; onSelect: (id: string) => void }) {
  const [refresh, setRefresh] = useState(0);
  const load = useCallback((limit: number, before: string | undefined, signal: AbortSignal) => listAssessments(limit, before, undefined, signal), []);
  const history = useAssessmentPage<AssessmentRunSummary>(load, `${revision}:${refresh}`);
  return <Card>
    <CardHeader title="能力对照历史" description="读取持久计划与结果，不产生模型调用。默认保留30天，未开始计划保留24小时。" actions={<Button kind="quiet" iconOnly aria-label="刷新能力对照历史" title="刷新能力对照历史" disabled={history.busy} onClick={() => setRefresh((value) => value + 1)}><RefreshCw aria-hidden="true" /></Button>} />
    {history.error && <p className="note crit" role="alert">历史读取失败：{history.error}</p>}
    {history.busy && <Loading />}
    {history.data && <><OperationalTable label="能力对照历史"><thead><tr><th>参考 → 被测</th><th>模式 / 时间</th><th>状态 / 请求计数</th><th>题集判定</th><th className="right">操作</th></tr></thead><tbody>{history.data.items.map((item) => <tr key={item.id} aria-current={item.id === selectedId ? "true" : undefined}><td><strong>{item.reference.model_name} → {item.candidate.model_name}</strong><span className="assessment-secondary">{item.reference.account_name} → {item.candidate.account_name}</span></td><td>{item.mode === "quick" ? "Quick · 仅描述" : "Full"}<time className="assessment-secondary" dateTime={item.created_at}>{fmtBeijing(item.created_at)}（北京时间）</time></td><td>{ASSESSMENT_STATES[item.state]}<span className="assessment-secondary">已结束 {item.finished_requests} / {item.planned_requests}</span><span className="assessment-secondary">预留 {item.reserved_requests} · 已知发送 {item.sent_requests} · 未知 {item.unknown_requests}</span></td><td>{assessmentVerdict(item.verdict)}</td><td className="right"><Button kind="quiet" iconOnly aria-label={`查看能力对照 ${item.id}`} title="查看能力对照" disabled={busy} onClick={() => onSelect(item.id)}><FileSearch aria-hidden="true" /></Button></td></tr>)}</tbody></OperationalTable>{history.data.items.length === 0 && <p className="assessment-help">暂无能力对照记录。</p>}</>}
    <RecordTablePager {...history.pager} label="能力对照历史分页" />
  </Card>;
}

function AssessmentSamples({ run }: { run: AssessmentRun }) {
  const [refresh, setRefresh] = useState(0);
  const load = useCallback((limit: number, after: string | undefined, signal: AbortSignal) => listAssessmentSamples(run.id, limit, after, signal), [run.id]);
  const samples = useAssessmentPage<AssessmentSample>(load, `${run.state}:${refresh}`);
  return <Card>
    <CardHeader title="样本与证据" description="全部固定计划行均保留；调用失败、不支持和未执行不当作答错，也不从覆盖分母中删除。" actions={<Button kind="quiet" iconOnly aria-label="刷新对照样本" title="刷新对照样本" disabled={samples.busy} onClick={() => setRefresh((value) => value + 1)}><RefreshCw aria-hidden="true" /></Button>} />
    {samples.error && <p className="note crit" role="alert">样本读取失败：{samples.error}</p>}
    {samples.busy && <Loading />}
    {samples.data && <OperationalTable label="能力对照样本"><thead><tr><th>题组 / 角色</th><th>执行 / 答题</th><th>计量 / 耗时</th><th>回答预览与检查</th></tr></thead><tbody>{samples.data.items.map((sample) => <tr key={sample.id}>
      <th scope="row">#{sample.ordinal} · {ASSESSMENT_DIMENSIONS[sample.dimension]}<span className="assessment-secondary">{sample.role === "reference" ? "参考" : "被测"}</span><code className="assessment-secondary">{sample.cluster_id}</code></th>
      <td>{resultLabels[sample.result]}<span className="assessment-secondary">{dispatchLabels[sample.dispatch_state]}</span>{sample.http_status !== null && <span className="assessment-secondary">HTTP {sample.http_status}</span>}{sample.error && <span className="assessment-secondary">{sample.error}</span>}{sample.error_kind && <code className="assessment-secondary">{sample.error_kind}</code>}</td>
      <td>输入 {sample.usage.input_tokens ?? "—"} / 输出 {sample.usage.output_tokens ?? "—"}<span className="assessment-secondary">上游总量 {sample.usage.upstream_total_tokens ?? "—"}</span><span className="assessment-secondary">{sample.duration_ms === null ? "耗时未知" : `${sample.duration_ms} ms`}</span></td>
      <td>{sample.evidence.answer_scrubbed && <strong>答案已脱敏，不能逐字重评</strong>}<details><summary>查看预览与检查</summary><pre className="assessment-answer">{sample.evidence.answer_preview ?? "无回答预览"}</pre>{sample.evidence.checks.map((check, index) => <p key={index}>{check.name}：{check.passed ? "通过" : "未通过"}</p>)}<code className="assessment-secondary">task hash: {sample.task_hash}</code><code className="assessment-secondary">request hash: {sample.request_semantic_hash}</code><span className="assessment-secondary">{sample.effective_request.native_protocol} · 输出上限 {sample.effective_request.output_cap} · 上游默认采样</span></details></td>
    </tr>)}</tbody></OperationalTable>}
    <RecordTablePager {...samples.pager} label="能力对照样本分页" />
  </Card>;
}
