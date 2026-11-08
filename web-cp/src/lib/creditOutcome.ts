/**
 * 直接入账没有幂等键：请求可能已经落库，而应答丢了（连接中断、代理的 5xx）。
 * 这种失败的结果是未知的，调用方不能把表单交还给操作员重试。
 * 只看 `status` 的形状，不引入 `ApiError` 本身，这样可以被 node:test 直接加载。
 */
export function isUnknownOutcome(e: unknown): boolean {
  const status = typeof e === "object" && e !== null ? (e as { status?: unknown }).status : undefined;
  // 不是 ApiError（网络层的异常、解析失败）：不知道请求有没有到达后端。
  if (typeof status !== "number") return true;
  return status === 0 || status >= 500;
}

export type CreditOutcome<T> = { kind: "done"; value: T } | { kind: "refused"; error: unknown } | { kind: "unknown"; error: unknown };

/** Sends the credit once and classifies how it ended: done, a definite refusal, or an unknown result. */
export async function sendCredit<T>(post: () => Promise<T>): Promise<CreditOutcome<T>> {
  try {
    return { kind: "done", value: await post() };
  } catch (error) {
    return isUnknownOutcome(error) ? { kind: "unknown", error } : { kind: "refused", error };
  }
}
