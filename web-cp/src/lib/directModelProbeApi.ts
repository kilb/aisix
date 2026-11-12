import { ApiError, call } from "./api";
import { DirectProbeNotSent, probeRequestPlan, type DirectProbeResult } from "./directModelProbe";

/** 只由显式点击触发；沿用共用 cookie 和会话失效处理，不自动重试。 */
export async function probeDirectModel(id: string, capabilityCheck = false): Promise<DirectProbeResult> {
  const plan = probeRequestPlan(capabilityCheck);
  try {
    return (await call<DirectProbeResult>(
      "POST",
      `/admin/models/${encodeURIComponent(id)}/probe`,
      plan.body,
      undefined,
      AbortSignal.timeout(plan.timeoutMs),
    )).data;
  } catch (error) {
    if (error instanceof ApiError) {
      if (error.status === 503 && error.code === "busy") {
        throw new DirectProbeNotSent("测试服务繁忙，未调用上游，请稍后重试。");
      }
      if (error.status === 401 || error.status === 403) {
        throw new DirectProbeNotSent("当前会话没有测试权限，未调用上游。请重新登录后重试。");
      }
      if (error.status === 404) {
        throw new DirectProbeNotSent("模型或测试接口已不存在，未调用上游。请刷新模型清单后重试。");
      }
      if (error.status === 429) {
        throw new DirectProbeNotSent("测试请求已被限流，未调用上游，请稍后重试。");
      }
    }
    throw error;
  }
}

export interface ProbeHistoryQuery {
  model_id?: string;
  mode?: "baseline" | "capability";
  status?: DirectProbeResult["status"];
  latest?: boolean;
  before?: string;
  limit?: number;
  from?: string;
  to?: string;
}
export interface ProbeHistoryPage {
  items: import("./directModelProbe").DirectProbeHistory[];
  next_before: string | null;
}

/** 只读存档；筛选和游标均由服务端处理，不产生任何模型调用。 */
export async function listProbeHistory(query: ProbeHistoryQuery = {}, signal?: AbortSignal): Promise<ProbeHistoryPage> {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) if (value !== undefined) params.set(key, String(value));
  return (await call<ProbeHistoryPage>("GET", `/admin/model-probes?${params}`, undefined, undefined, signal)).data;
}

/** 最新结果同样遍历真实游标，目标超过一页也不会遗漏。 */
export async function latestProbeHistory(signal?: AbortSignal): Promise<import("./directModelProbe").DirectProbeHistory[]> {
  const records: import("./directModelProbe").DirectProbeHistory[] = [];
  let before: string | undefined;
  do {
    const page = await listProbeHistory({ latest: true, limit: 100, before }, signal);
    records.push(...page.items);
    before = page.next_before ?? undefined;
  } while (before && !signal?.aborted);
  return records;
}
