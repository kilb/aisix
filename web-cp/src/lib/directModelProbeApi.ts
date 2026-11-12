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
