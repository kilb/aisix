import { Check, CircleHelp, LoaderCircle, Play, Square } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import {
  directProbeTargets,
  capabilityChecks,
  capabilityDuration,
  capabilityStatus,
  probeDuration,
  probeMeasurements,
  probeResultMessage,
  probeRequestPlan,
  runDirectProbeBatch,
  savedProbeRows,
  type DirectProbeHistory,
  type DirectProbeRow,
  type DirectProbeResource,
  type DirectProbeCapability,
  type DirectProbeResult,
  type DirectProbeTarget,
  type DirectProbeUpdate,
} from "../lib/directModelProbe";
import { latestProbeHistory, listProbeHistory } from "../lib/directModelProbeApi";
import { fmtBeijing, beijingRange } from "../lib/time";
import { RecordTableToolbar, RecordTablePager, type RecordPageSize } from "../ui/RecordTable";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { Card, CardHeader, Metric } from "./page";
import "./DirectModelProbe.css";

type Status = "not_tested" | "running" | DirectProbeResult["status"];

const STATUS_LABEL: Record<Status, string> = {
  not_tested: "未测试", running: "测试中", available: "可用", failed: "失败", timeout: "超时", unsupported: "不支持",
};
const emptyRows = (targets: readonly DirectProbeTarget[]): DirectProbeRow[] => savedProbeRows(targets, []);
const tokens = (value: number | null | undefined): string => value == null ? "—" : value.toLocaleString("zh-CN");

/** 每轮固定完整直连清单；调用函数只由真实控制面 API 提供。 */
export function DirectModelProbe({ models, accounts, loadError, canProbe, probe, section = "current" }: {
  section?: "current" | "history";
  models: readonly DirectProbeResource[] | null;
  accounts: readonly DirectProbeResource[] | null;
  loadError: boolean;
  canProbe: boolean;
  probe: (id: string, capabilityCheck?: boolean) => Promise<DirectProbeResult>;
}) {
  const headingId = useId();
  const costId = useId();
  const capabilityHelpId = useId();
  const targets = useMemo(() => directProbeTargets(models ?? [], accounts ?? []), [models, accounts]);
  const [rows, setRows] = useState<DirectProbeRow[] | null>(null);
  const [saved, setSaved] = useState<DirectProbeHistory[]>([]);
  const [historyError, setHistoryError] = useState(false);
  const [historyRevision, setHistoryRevision] = useState(0);
  const [search, setSearch] = useState("");
  const [resultPage, setResultPage] = useState(1);
  const [resultPageSize, setResultPageSize] = useState<RecordPageSize>(25);
  const [capabilityCheck, setCapabilityCheck] = useState(false);
  const [runCapabilityCheck, setRunCapabilityCheck] = useState<boolean | null>(null);
  const [running, setRunning] = useState(false);
  const [stopped, setStopped] = useState(false);
  const [elapsed, setElapsed] = useState<number | null>(null);
  const [startedAt, setStartedAt] = useState<string | null>(null);
  const active = useRef<AbortController | null>(null);
  const mounted = useRef(false);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      active.current?.abort();
    };
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    async function load(): Promise<void> {
      try {
        const records = await latestProbeHistory(controller.signal);
        if (controller.signal.aborted) return;
        setSaved(records);
        setHistoryError(false);
        if (records.some((record) => record.state === "running")) timer = setTimeout(() => void load(), 2000);
      } catch {
        if (!controller.signal.aborted) setHistoryError(true);
      }
    }
    void load();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [historyRevision, models, accounts]);

  const savedByModel = new Map(saved.map((record) => [record.model_id, record]));
  const displayed = rows?.map((row) => {
    const record = savedByModel.get(row.target.id);
    return row.result && record?.result?.checked_at === row.result.checked_at ? { ...row, history: record } : row;
  }) ?? savedProbeRows(targets, saved);
  const filtered = displayed.filter(({ target }) => `${target.name} ${target.account} ${target.upstream}`.toLocaleLowerCase().includes(search.toLocaleLowerCase()));
  const resultPages = Math.max(1, Math.ceil(filtered.length / resultPageSize));
  const shownPage = Math.min(resultPage, resultPages);
  const visibleRows = filtered.slice((shownPage - 1) * resultPageSize, shownPage * resultPageSize);
  const counts: Record<Status, number> = { not_tested: 0, running: 0, available: 0, failed: 0, timeout: 0, unsupported: 0 };
  for (const row of displayed) counts[row.status]++;
  const completed = displayed.length - counts.not_tested - counts.running;
  const shownCapabilityMode = running ? runCapabilityCheck === true : capabilityCheck;
  const expectedRequests = (running ? displayed.length : targets.length) * probeRequestPlan(shownCapabilityMode).requestsPerModel;

  async function start(): Promise<void> {
    // ref 在重绘前加锁，双击也不会创建第二轮付费请求。
    if (!canProbe || active.current || models === null || loadError || targets.length === 0) return;
    const controller = new AbortController();
    active.current = controller;
    const started = performance.now();
    setRunCapabilityCheck(capabilityCheck);
    setRows(emptyRows(targets));
    setStartedAt(fmtBeijing(new Date().toISOString()));
    setElapsed(0);
    setStopped(false);
    setRunning(true);
    const timer = window.setInterval(() => {
      if (mounted.current) setElapsed(performance.now() - started);
    }, 1000);
    try {
      await runDirectProbeBatch({
        targets,
        signal: controller.signal,
        probe: (id) => probe(id, capabilityCheck),
        onUpdate: (id: string, update: DirectProbeUpdate) => {
          if (!mounted.current) return;
          setRows((previous) => previous?.map((row) => row.target.id !== id ? row : {
            ...row,
            status: update.status,
            result: "model_id" in update ? update : null,
            error: "error" in update ? update.error : null,
          }) ?? null);
        },
      });
    } finally {
      window.clearInterval(timer);
      if (active.current === controller) active.current = null;
      if (mounted.current) {
        setElapsed(performance.now() - started);
        setRunning(false);
        setHistoryRevision((previous) => previous + 1);
      }
    }
  }

  function stop(): void {
    active.current?.abort();
    setStopped(true);
  }

  const unavailableReason = loadError ? "模型清单读取失败，请先刷新后再测试。" : models === null ? "正在读取完整模型清单。" : targets.length === 0 ? "当前没有直连模型。" : undefined;

  return (
    <>
    <div id="model-tests-current" role="tabpanel" aria-labelledby="model-tests-current-tab" hidden={section !== "current"}>
    <Card className="direct-probe" aria-labelledby={headingId}>
      <CardHeader
        title={<span id={headingId}>直连模型测试</span>}
        description="基础探测检查当前协议的连通与响应；可选轻量能力抽测提供小样本线索，不代表长期可用率或全面模型水平。"
        actions={canProbe ? running ? (
          <Button type="button" onClick={stop} disabled={stopped}>
            <Square aria-hidden />{stopped ? "正在等待已发请求" : "停止未发送的测试"}
          </Button>
        ) : (
          <Button type="button" onClick={() => void start()} disabled={unavailableReason !== undefined} aria-describedby={costId}>
            <Play aria-hidden />{rows === null && saved.length === 0 ? "测试所有直连模型" : "重新测试所有直连模型"}
          </Button>
        ) : undefined}
      />
      {canProbe && <div className="direct-probe-options">
        <label className="direct-probe-capability-toggle">
          <input
            type="checkbox"
            checked={capabilityCheck}
            disabled={running}
            aria-describedby={capabilityHelpId}
            onChange={(event) => { if (!active.current) setCapabilityCheck(event.target.checked); }}
          />
          <span>轻量能力抽测</span>
        </label>
        <p id={capabilityHelpId}>默认关闭。开启后，对基础探测可用且返回可见正文的文本目标额外发出 1 次联合客观题，检查严格 JSON、指令遵循、算术与排序；向量模型不追加。抽测不能认证模型身份或全面模型水平。</p>
      </div>}
      <p className="direct-probe-cost" id={costId}>
        {models === null ? "正在读取完整直连模型清单。" : <>{running ? "本轮预计" : "预计"}最多发出 <strong>{expectedRequests}</strong> 次模型请求，覆盖 <strong>{running ? displayed.length : targets.length}</strong> 个直连目标（含列表中隐藏的内部目标）。每个目标先发 1 次基础探测{shownCapabilityMode ? "，基础可用且返回可见正文的文本目标额外 1 次能力抽测" : ""}；上游可能收费，必要的凭据交换另计。并发 3 个，每次模型调用最多 30 秒，每个目标整轮最多 55 秒；能力抽测使用整轮剩余预算，凭据交换耗时也计入整轮。</>}
        {!canProbe && <span> 仅管理员可发起测试。</span>}
      </p>
      {loadError && <p className="note crit" role="alert">模型清单读取失败，请刷新页面后再发起测试，避免遗漏模型。</p>}
      {models === null ? !loadError && <Loading /> : (
        <>
          <div className="direct-probe-metrics">
            <div><p>{rows === null ? "最近已取得结果" : "本轮完成"}</p><Metric size="figure">{completed}<span className="direct-probe-denominator"> / {displayed.length}</span></Metric></div>
            <div><p>可用</p><Metric size="figure">{counts.available}</Metric></div>
            <div><p>失败 / 超时</p><Metric size="figure">{counts.failed}<span className="direct-probe-denominator"> / {counts.timeout}</span></Metric></div>
            <div><p>本轮总耗时</p><Metric size="figure">{probeDuration(rows === null ? null : elapsed)}</Metric></div>
          </div>
          <div className="direct-probe-progress" role="status" aria-live="polite">
            <p>{running ? stopped ? "已停止排队，等待已发请求结束；这些调用仍可能收费。" : `正在测试 ${counts.running} 个模型，还有 ${counts.not_tested} 个等待发送。` : rows === null ? saved.length > 0 ? "以下为已保存的最近测试结果，基础与能力来自同一轮。" : "尚无已保存记录；历史保存启用前的测试未留存。" : stopped ? "本轮已停止，未发送的模型保留为未测试。" : counts.not_tested > 0 ? "本轮已结束，部分请求未发送。请查看结果说明后重试。" : "本轮测试完成。"}</p>
            <p>未测试 {counts.not_tested} · 不支持 {counts.unsupported}{rows !== null && startedAt ? ` · 开始于 ${startedAt}` : ""}</p>
          </div>
          <progress className="direct-probe-bar" max={Math.max(1, displayed.length)} value={completed} aria-label={`本轮已完成 ${completed} 个，共 ${displayed.length} 个模型`} />
          {historyError && <p className="note crit" role="alert">已保存结果读取失败，请刷新记录后重试；这不代表模型未测试。</p>}
          <RecordTableToolbar search={{ value: search, onChange: (value) => { setSearch(value); setResultPage(1); }, label: "筛选最近测试结果", placeholder: "搜索模型、上游账号或上游模型" }}>
            <Button type="button" onClick={() => { setRows(null); setHistoryRevision((previous) => previous + 1); }} disabled={running}>刷新记录</Button>
          </RecordTableToolbar>
          {displayed.length === 0 ? <p className="direct-probe-empty">当前没有直连模型。</p> : (
            <DataTable label={rows ? "本次直连模型测试结果" : "最近已保存测试结果"} scroll>
              <thead><tr>
                <th>直连模型 / 上游账号</th><th>基础状态</th><th className="num">基础首字延迟</th><th className="num">基础总耗时</th>
                <th className="num">基础输入 / 输出 tokens</th><th className="num">基础端到端速率</th><th>能力抽测</th><th>基础结果说明</th><th>记录 / 配置</th>
              </tr></thead>
              <tbody>{visibleRows.map(({ target, status, result, error, history }) => {
                const measurements = probeMeasurements(result);
                return <tr key={target.id}>
                  <td className="direct-probe-model"><strong>{target.name}</strong><span>{target.account || "—"}</span><code>{target.upstream || target.id}</code></td>
                  <td><span className="direct-probe-status" data-status={status}>
                    {status === "running" ? <LoaderCircle className="animate-spin motion-reduce:animate-none" aria-hidden /> : status === "available" ? <Check aria-hidden /> : <CircleHelp aria-hidden />}
                    {STATUS_LABEL[status]}
                  </span></td>
                  <td className="num">{measurements.ttft}</td>
                  <td className="num">{measurements.duration}</td>
                  <td className="num">{tokens(result?.usage?.input_tokens)} / {tokens(result?.usage?.output_tokens)}</td>
                  <td className="num">{measurements.rate}</td>
                  <td className="direct-probe-capability">
                    <CapabilityResult
                      capability={result?.capability_check ?? null}
                      requested={history ? history.mode === "capability" : runCapabilityCheck ?? capabilityCheck}
                      started={rows !== null || history !== null}
                      pending={status === "running"}
                    />
                  </td>
                  <td className="direct-probe-detail">
                    {error ? <span>{error}</span> : result ? <span>{probeResultMessage(result)}</span> : status === "not_tested" ? <span>{running && !stopped ? "等待发送" : "尚未发送请求"}</span> : status === "running" ? <span>等待上游响应</span> : <span>—</span>}
                    {result?.http_status != null && status !== "available" && <span className="direct-probe-http">HTTP {result.http_status}</span>}
                    {result && <time dateTime={result.checked_at}>{fmtBeijing(result.checked_at)}</time>}
                  </td>
                  <td className="direct-probe-detail">{history ? <>
                    <span>{history.state === "running" ? "运行中" : history.state === "interrupted" ? "运行中断" : "已保存"} · {history.mode === "capability" ? "基础 + 能力" : "仅基础"}</span>
                    <span>{history.actor.email ?? history.actor.id}</span>
                    <span>{history.stale ? "配置已变更，旧样本已过期" : "配置版本一致；仅代表本次样本"}</span>
                  </> : <span>{rows ? "本轮结果；刷新记录可查看存档" : "尚无已保存记录"}</span>}</td>
                </tr>;
              })}</tbody>
            </DataTable>
          )}
          <RecordTablePager page={shownPage} pageSize={resultPageSize} onPageChange={setResultPage} onPageSizeChange={(size) => { setResultPageSize(size); setResultPage(1); }} total={filtered.length} label="最近测试结果分页" />
          <p className="direct-probe-method">普通直连采用流式文本探测，显式向量模型走向量接口；一次文本探测失败不代表图像、音频等其它能力不可用。从控制平面发起本次探测开始计时，包含地址解析、连接及必要的凭据交换；测量控制平面到指定上游，不代表客户经网关请求的耗时。基础指标仅来自首次探测，能力抽测的额外耗时和用量单独列出。首字只计算首个非空可见文本，角色、推理或用量事件不算首字；协议正常结束但无正文时仍可用，首字显示 —。端到端输出速率 = 上游报告的输出 tokens ÷ 完整请求耗时（含等待，输出计数可能包含推理 tokens）；未报告用量时显示 —。</p>
        </>
      )}
    </Card>
    </div>
    {/* 两个面板始终挂载；切换历史只隐藏当前测试，不终止排队或丢失游标。 */}
    <div id="model-tests-history" role="tabpanel" aria-labelledby="model-tests-history-tab" hidden={section !== "history"}>
      <Card><ProbeHistory targets={targets} revision={historyRevision} /></Card>
    </div>
    </>
  );
}

/** 能力结论和调用错误分开；未取得或未执行的检查不补成未通过。 */
function CapabilityResult({ capability, requested, started, pending }: {
  capability: DirectProbeCapability | null;
  requested: boolean;
  started: boolean;
  pending: boolean;
}) {
  const hasChecks = capability !== null && Object.values(capability.checks).some((value) => value !== null);
  const measured = capability !== null && capability.status !== "not_tested" && capability.status !== "unsupported";
  return <div className="direct-probe-capability-result">
    <strong>{pending && requested ? "等待结果" : capabilityStatus(capability)}</strong>
    {capability ? <>
      {hasChecks && <dl>{capabilityChecks(capability).map((check) => <div key={check.key}><dt>{check.label}</dt><dd>{check.outcome}</dd></div>)}</dl>}
      {measured && <p>额外耗时 {capabilityDuration(capability)}<br />额外输入 / 输出 tokens：{tokens(capability.usage?.input_tokens)} / {tokens(capability.usage?.output_tokens)}</p>}
      {capability.error && <p>{capability.error}</p>}
      {capability.note && <p>{capability.note}</p>}
    </> : <p>{!requested ? "本轮未启用" : pending ? "等待基础探测和能力抽测完成" : !started ? "尚未发起抽测" : "未取得能力结果，无法判断抽测是否执行"}</p>}
  </div>;
}

/** 真实后端游标分页；切换条件从第一页开始，不为历史伪造总量。 */
function ProbeHistory({ targets, revision }: { targets: readonly DirectProbeTarget[]; revision: number }) {
  const [model, setModel] = useState("");
  const [mode, setMode] = useState<"" | "baseline" | "capability">("");
  const [status, setStatus] = useState<"" | DirectProbeResult["status"]>("");
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState<RecordPageSize>(50);
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const [items, setItems] = useState<DirectProbeHistory[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const before = cursors[page - 1];
  const invalidRange = Boolean(from && to && from > to);
  function reset(): void { setPage(1); setCursors([undefined]); }
  useEffect(() => { setPage(1); setCursors([undefined]); }, [revision]);
  useEffect(() => {
    const controller = new AbortController();
    if (invalidRange) { setBusy(false); setItems([]); setNext(null); return; }
    setBusy(true);
    setError(null);
    void listProbeHistory({
      model_id: model || undefined, mode: mode || undefined, status: status || undefined,
      from: from ? beijingRange(from, from).from : undefined,
      to: to ? beijingRange(to, to).to : undefined,
      limit: pageSize, before,
    }, controller.signal).then((result) => {
      if (!controller.signal.aborted) { setItems(result.items); setNext(result.next_before); }
    }).catch(() => {
      if (!controller.signal.aborted) { setError("测试历史读取失败，请刷新记录后重试。"); setItems([]); setNext(null); }
    }).finally(() => { if (!controller.signal.aborted) setBusy(false); });
    return () => controller.abort();
  }, [model, mode, status, from, to, before, pageSize, revision, invalidRange]);
  function changePage(nextPage: number): void {
    if (nextPage === page + 1 && next) {
      setCursors((previous) => [...previous.slice(0, page), next]); setPage(nextPage);
    } else if (nextPage === page - 1 && nextPage >= 1) setPage(nextPage);
  }
  return <section className="direct-probe-history" aria-label="测试历史">
    <h3>测试历史</h3>
    <p className="direct-probe-method">从历史保存启用后保留每一轮结果。基础与能力各自保留状态和用量，不计入正式调用统计。</p>
    <RecordTableToolbar>
      <label>测试目标<select aria-label="历史测试目标" value={model} onChange={(event) => { setModel(event.target.value); reset(); }}><option value="">全部目标</option>{targets.map((target) => <option value={target.id} key={target.id}>{target.name}</option>)}</select></label>
      <label>测试模式<select value={mode} onChange={(event) => { setMode(event.target.value as typeof mode); reset(); }}><option value="">全部模式</option><option value="baseline">仅基础</option><option value="capability">基础 + 能力</option></select></label>
      <label>基础状态<select value={status} onChange={(event) => { setStatus(event.target.value as typeof status); reset(); }}><option value="">全部状态</option>{(["available", "failed", "timeout", "unsupported"] as const).map((value) => <option value={value} key={value}>{STATUS_LABEL[value]}</option>)}</select></label>
      <label>开始日期（北京）<input type="date" value={from} onChange={(event) => { setFrom(event.target.value); reset(); }} /></label>
      <label>结束日期（北京）<input type="date" value={to} onChange={(event) => { setTo(event.target.value); reset(); }} /></label>
    </RecordTableToolbar>
    {invalidRange && <p role="alert" className="note crit">开始日期不能晚于结束日期。</p>}
    {error && <p role="alert" className="note crit">{error}</p>}
    {busy ? <Loading /> : !invalidRange && !error && items.length === 0 ? <p className="direct-probe-empty">此筛选下暂无已保存的测试记录。历史保存启用前的测试未留存。</p> : items.length > 0 ? <DataTable label="已保存测试历史" scroll>
      <thead><tr><th>时间 / 目标</th><th>模式 / 操作者</th><th>运行 / 配置</th><th>基础结果</th><th>能力抽测</th></tr></thead>
      <tbody>{items.map((record) => <tr key={record.id}>
        <td className="direct-probe-detail"><time dateTime={record.started_at}>{fmtBeijing(record.started_at)}</time><strong>{record.target.name}</strong><span>{record.target.account ?? "—"}</span><code>{record.model_id}</code></td>
        <td className="direct-probe-detail"><span>{record.mode === "capability" ? "基础 + 能力" : "仅基础"}</span><span>{record.actor.email ?? record.actor.id}</span></td>
        <td className="direct-probe-detail"><span>{record.state === "completed" ? "已保存" : record.state === "running" ? "测试中" : "运行中断，最终结果未知"}</span><span>{record.stale ? "配置已变更，旧样本已过期" : "配置版本一致；仅代表本次样本"}</span><span>模型 v{record.model_version} / 账号 v{record.provider_key_version ?? "—"}</span></td>
        <td className="direct-probe-detail">{record.result ? <><strong>{STATUS_LABEL[record.result.status]}</strong><span>{probeResultMessage(record.result)}</span>{record.result.http_status != null && <span>HTTP {record.result.http_status}</span>}<span>首字 {probeMeasurements(record.result).ttft} · 总耗时 {probeMeasurements(record.result).duration}</span><span>输入 / 输出 {tokens(record.result.usage?.input_tokens)} / {tokens(record.result.usage?.output_tokens)} tokens</span><span>端到端速率 {probeMeasurements(record.result).rate}</span></> : <span>尚未取得基础结果</span>}</td>
        <td className="direct-probe-capability"><CapabilityResult capability={record.result?.capability_check ?? null} requested={record.mode === "capability"} started pending={record.state === "running"} /></td>
      </tr>)}</tbody>
    </DataTable> : null}
    <RecordTablePager page={page} pageSize={pageSize} onPageChange={changePage} onPageSizeChange={(size) => { setPageSize(size); reset(); }} loadedCount={(page - 1) * pageSize + items.length} hasNext={next !== null} busy={busy || invalidRange} label="测试历史分页" />
  </section>;
}
