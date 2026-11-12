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
  type DirectProbeResource,
  type DirectProbeCapability,
  type DirectProbeResult,
  type DirectProbeTarget,
  type DirectProbeUpdate,
} from "../lib/directModelProbe";
import { Button } from "../ui/Button";
import { DataTable, Loading } from "../ui/DataTable";
import { Card, CardHeader, Metric } from "./page";
import "./DirectModelProbe.css";

type Status = "not_tested" | "running" | DirectProbeResult["status"];
interface ProbeRow {
  target: DirectProbeTarget;
  status: Status;
  result: DirectProbeResult | null;
  error: string | null;
}

const STATUS_LABEL: Record<Status, string> = {
  not_tested: "未测试", running: "测试中", available: "可用", failed: "失败", timeout: "超时", unsupported: "不支持",
};
const emptyRows = (targets: readonly DirectProbeTarget[]): ProbeRow[] => targets.map((target) => ({ target, status: "not_tested", result: null, error: null }));
const tokens = (value: number | null | undefined): string => value == null ? "—" : value.toLocaleString("zh-CN");

/** 每轮固定完整直连清单；调用函数只由真实控制面 API 提供。 */
export function DirectModelProbe({ models, accounts, loadError, canProbe, probe }: {
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
  const [rows, setRows] = useState<ProbeRow[] | null>(null);
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

  const displayed = rows ?? emptyRows(targets);
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
    setStartedAt(new Date().toLocaleString("zh-CN"));
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
      }
    }
  }

  function stop(): void {
    active.current?.abort();
    setStopped(true);
  }

  const unavailableReason = loadError ? "模型清单读取失败，请先刷新后再测试。" : models === null ? "正在读取完整模型清单。" : targets.length === 0 ? "当前没有直连模型。" : undefined;

  return (
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
            <Play aria-hidden />{rows === null ? "测试所有直连模型" : "重新测试所有直连模型"}
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
            <div><p>本轮完成</p><Metric size="figure">{completed}<span className="direct-probe-denominator"> / {displayed.length}</span></Metric></div>
            <div><p>可用</p><Metric size="figure">{counts.available}</Metric></div>
            <div><p>失败 / 超时</p><Metric size="figure">{counts.failed}<span className="direct-probe-denominator"> / {counts.timeout}</span></Metric></div>
            <div><p>本轮总耗时</p><Metric size="figure">{probeDuration(elapsed)}</Metric></div>
          </div>
          <div className="direct-probe-progress" role="status" aria-live="polite">
            <p>{running ? stopped ? "已停止排队，等待已发请求结束；这些调用仍可能收费。" : `正在测试 ${counts.running} 个模型，还有 ${counts.not_tested} 个等待发送。` : rows === null ? "尚未发起测试。" : stopped ? "本轮已停止，未发送的模型保留为未测试。" : counts.not_tested > 0 ? "本轮已结束，部分请求未发送。请查看结果说明后重试。" : "本轮测试完成。"}</p>
            <p>未测试 {counts.not_tested} · 不支持 {counts.unsupported}{startedAt ? ` · 开始于 ${startedAt}` : ""}</p>
          </div>
          <progress className="direct-probe-bar" max={Math.max(1, displayed.length)} value={completed} aria-label={`本轮已完成 ${completed} 个，共 ${displayed.length} 个模型`} />
          {displayed.length === 0 ? <p className="direct-probe-empty">当前没有直连模型。</p> : (
            <DataTable label="本次直连模型测试结果" scroll>
              <thead><tr>
                <th>直连模型 / 上游账号</th><th>本次状态</th><th className="num">基础首字延迟</th><th className="num">基础总耗时</th>
                <th className="num">基础输入 / 输出 tokens</th><th className="num">基础端到端速率</th><th>能力抽测</th><th>基础结果说明</th>
              </tr></thead>
              <tbody>{displayed.map(({ target, status, result, error }) => {
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
                      requested={runCapabilityCheck ?? capabilityCheck}
                      started={rows !== null}
                      pending={status === "running"}
                    />
                  </td>
                  <td className="direct-probe-detail">
                    {error ? <span>{error}</span> : result ? <span>{probeResultMessage(result)}</span> : status === "not_tested" ? <span>{running && !stopped ? "等待发送" : "尚未发送请求"}</span> : status === "running" ? <span>等待上游响应</span> : <span>—</span>}
                    {result?.http_status != null && status !== "available" && <span className="direct-probe-http">HTTP {result.http_status}</span>}
                    {result && <time dateTime={result.checked_at}>{new Date(result.checked_at).toLocaleString("zh-CN")}</time>}
                  </td>
                </tr>;
              })}</tbody>
            </DataTable>
          )}
          <p className="direct-probe-method">普通直连采用流式文本探测，显式向量模型走向量接口；一次文本探测失败不代表图像、音频等其它能力不可用。从控制平面发起本次探测开始计时，包含地址解析、连接及必要的凭据交换；测量控制平面到指定上游，不代表客户经网关请求的耗时。基础指标仅来自首次探测，能力抽测的额外耗时和用量单独列出。首字只计算首个非空可见文本，角色、推理或用量事件不算首字；协议正常结束但无正文时仍可用，首字显示 —。端到端输出速率 = 上游报告的输出 tokens ÷ 完整请求耗时（含等待，输出计数可能包含推理 tokens）；未报告用量时显示 —。</p>
        </>
      )}
    </Card>
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
