export interface DirectProbeCapability {
  status: "not_tested" | "passed" | "failed" | "error" | "timeout" | "unsupported";
  checks: { json: boolean | null; instruction: boolean | null; arithmetic: boolean | null; sorting: boolean | null };
  total_duration_ms: number | null;
  usage: { input_tokens: number | null; output_tokens: number | null } | null;
  error: string | null;
  note: string | null;
}

/** 控制面单次直连探测响应；缺失计量保持 null，不补成零。 */
export interface DirectProbeResult {
  model_id: string;
  status: "available" | "failed" | "timeout" | "unsupported";
  ttft_ms: number | null;
  total_duration_ms: number;
  http_status: number | null;
  usage: { input_tokens: number | null; output_tokens: number | null } | null;
  output_tokens_per_second: number | null;
  error: string | null;
  note: string | null;
  checked_at: string;
  capability_check: DirectProbeCapability;
  fidelity: { status: "not_tested"; reason: string };
}

export interface DirectProbeTarget {
  id: string;
  name: string;
  provider: string;
  upstream: string;
  account: string;
}

export class DirectProbeNotSent extends Error {}

export type DirectProbeUpdate = { status: "running" } | { status: "failed" | "not_tested"; error: string } | DirectProbeResult;

interface ProbeDoc {
  display_name?: string;
  provider?: string;
  model_name?: string;
  provider_key_id?: string;
  routing?: unknown;
  ensemble?: unknown;
  semantic?: unknown;
}

export interface DirectProbeResource { id: string; doc: ProbeDoc }

/** 从完整资源集合选直连，包含隐藏的公共模型目标；不按模型名推断模态。 */
export function directProbeTargets(models: readonly DirectProbeResource[], accounts: readonly DirectProbeResource[]): DirectProbeTarget[] {
  const accountOf = new Map(accounts.map((account) => [account.id, account.doc.display_name || account.id]));
  const seen = new Set<string>();
  const targets: DirectProbeTarget[] = [];
  for (const { id, doc } of models) {
    if (doc.routing != null || doc.ensemble != null || doc.semantic != null || seen.has(id)) continue;
    seen.add(id);
    targets.push({
      id,
      name: doc.display_name || id,
      provider: doc.provider ?? "",
      upstream: doc.model_name ?? "",
      account: doc.provider_key_id ? accountOf.get(doc.provider_key_id) ?? doc.provider_key_id : "",
    });
  }
  return targets;
}

export function probeDuration(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  return value < 1000 ? `${Math.round(value)} ms` : `${Number((value / 1000).toFixed(2))} s`;
}

/** 速率由后端按完整耗时计算；前端不把 TTFT 后的时间当解码分母。 */
export function probeRate(value: number | null): string {
  return value == null || !Number.isFinite(value) ? "—" : `${Number(value.toFixed(2))} tokens/s`;
}

/** 仅实际执行的调用有测量值；不把调用前拒绝的占位零显示成耗时。 */
export function probeMeasurements(result: DirectProbeResult | null): { ttft: string; duration: string; rate: string } {
  const measured = result?.status === "unsupported" ? null : result;
  return { ttft: probeDuration(measured?.ttft_ms), duration: probeDuration(measured?.total_duration_ms), rate: probeRate(measured?.output_tokens_per_second ?? null) };
}

/** 协议正常完成的提示独立于失败原因；无正文的成功不降级为失败。 */
export function probeResultMessage(result: DirectProbeResult): string {
  return result.error ?? result.note ?? (result.status === "available" ? "本次协议响应成功" : "—");
}

/** 费用上界和请求体来自同一显式选项，默认不发能力抽测。 */
export function probeRequestPlan(capabilityCheck: boolean): { body: { capability_check?: true }; timeoutMs: number; requestsPerModel: 1 | 2 } {
  return capabilityCheck
    ? { body: { capability_check: true }, timeoutMs: 90000, requestsPerModel: 2 }
    : { body: {}, timeoutMs: 45000, requestsPerModel: 1 };
}

/** 客观题的 false 与未返回结果的 null 保持不同，不从总状态补造单项结论。 */
export function capabilityChecks(capability: DirectProbeCapability): { key: string; label: string; outcome: string }[] {
  return ([
    ["json", "严格 JSON"], ["instruction", "指令遵循"], ["arithmetic", "算术"], ["sorting", "排序"],
  ] as const).map(([key, label]) => ({ key, label, outcome: capability.checks[key] === true ? "通过" : capability.checks[key] === false ? "未通过" : "未测试" }));
}

export function capabilityStatus(capability: DirectProbeCapability | null): string {
  if (!capability) return "未测试";
  return ({ not_tested: "未测试", passed: "通过", failed: "未通过", error: "调用失败", timeout: "超时", unsupported: "不适用" } as const)[capability.status];
}

export function capabilityDuration(capability: DirectProbeCapability): string {
  return probeDuration(capability.status === "not_tested" || capability.status === "unsupported" ? null : capability.total_duration_ms);
}

/** 停止只影响尚未发送的队列；每个已发请求恰好等待一次，不重试。 */
export async function runDirectProbeBatch({ targets, signal, probe, onUpdate }: {
  targets: readonly DirectProbeTarget[];
  signal: AbortSignal;
  probe: (id: string) => Promise<DirectProbeResult>;
  onUpdate: (id: string, state: DirectProbeUpdate) => void;
}): Promise<void> {
  let next = 0;
  let refused = false;
  async function worker(): Promise<void> {
    while (!signal.aborted && !refused && next < targets.length) {
      const target = targets[next++];
      if (!target) return;
      onUpdate(target.id, { status: "running" });
      try {
        onUpdate(target.id, await probe(target.id));
      } catch (error) {
        if (error instanceof DirectProbeNotSent) {
          refused = true;
          onUpdate(target.id, { status: "not_tested", error: error.message });
          continue;
        }
        // 网络或会话异常不展示未知错误文本，避免泄露地址、凭证或原始响应。
        onUpdate(target.id, {
          status: "failed",
          error: "未取得探测结果，请检查控制面连接或会话状态。已发送的请求仍可能在上游完成。",
        });
      }
    }
  }
  await Promise.all(Array.from({ length: Math.min(3, targets.length) }, () => worker()));
}

/** 历史 id/配置版本按十进制字符串传输，不经 JS number。 */
export interface DirectProbeHistory {
  id: string;
  model_id: string;
  provider_key_id: string | null;
  model_version: string;
  provider_key_version: string | null;
  target: { name: string; account: string | null };
  actor: { id: string; email: string | null };
  mode: "baseline" | "capability";
  started_at: string;
  finished_at: string | null;
  state: "running" | "completed" | "interrupted";
  stale: boolean;
  result: DirectProbeResult | null;
}

export interface DirectProbeRow {
  target: DirectProbeTarget;
  status: "not_tested" | "running" | DirectProbeResult["status"];
  result: DirectProbeResult | null;
  error: string | null;
  history: DirectProbeHistory | null;
}

/** 每条记录保留同一轮基础与能力结果，不借用其它轮次填空。 */
export function savedProbeRows(targets: readonly DirectProbeTarget[], records: readonly DirectProbeHistory[]): DirectProbeRow[] {
  const saved = new Map<string, DirectProbeHistory>();
  for (const record of records) if (!saved.has(record.model_id)) saved.set(record.model_id, record);
  return targets.map((target) => {
    const history = saved.get(target.id) ?? null;
    return {
      target, history, result: history?.result ?? null,
      status: history?.state === "running" ? "running" : history?.result?.status ?? "not_tested",
      error: history?.state === "interrupted" ? "本次测试中断，未取得最终结果；已发调用仍可能完成。" : null,
    };
  });
}
