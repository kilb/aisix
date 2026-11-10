import type { Charge } from "./api";
import { reasonLabel } from "./pricingStatus.ts";

/** Error classes of an attempt the upstream failed, as opposed to a request the gateway refused. */
const UPSTREAM_FAILURES = new Set([
  "upstream_status",
  "upstream_decode",
  "upstream_in_band",
  "provider_unavailable",
  "all_candidates_unavailable",
  "timeout",
  "transport",
  "stream_aborted",
  "invalid_credentials",
  "config",
  "invalid_config",
]);

/** An upstream failure that carries no status of its own, by its class. */
const UPSTREAM_WORDS: Record<string, string> = {
  timeout: "上游超时",
  transport: "连接上游失败",
  stream_aborted: "流中断",
  all_candidates_unavailable: "没有可用的上游",
  provider_unavailable: "上游不可用",
};

const EXEMPTIONS: Record<string, string> = {
  response_cache_hit: "缓存命中",
  not_model_usage: "非模型用量",
};

type Attempt = Pick<Charge, "attempt_kind" | "attempt_index">;

/** Which attempt of its request the row is, when it is not the first. */
function attemptText(c: Attempt): string | null {
  if (c.attempt_kind === "retry") return `第 ${c.attempt_index} 次重试`;
  if (c.attempt_kind === "fallback") return `第 ${c.attempt_index + 1} 次尝试（换用备选）`;
  return null;
}

/**
 * Why a row was billed nothing, from how its call ended: an attempt the
 * upstream failed (「调用失败 · 上游 502 · 第 2 次重试」), a request the gateway
 * refused (「请求被拒绝 · 404」), or a call that reported no usage (「无用量」).
 */
export function nothingToBillReason(c: Pick<Charge, "status_code" | "error_class" | "attempt_kind" | "attempt_index">): string {
  const attempt = attemptText(c);
  const cls = c.error_class ?? "";
  const status = c.status_code !== null && c.status_code > 0 ? c.status_code : null;
  if (UPSTREAM_FAILURES.has(cls)) {
    const what = status !== null && !(status >= 200 && status < 300) ? `上游 ${status}` : (UPSTREAM_WORDS[cls] ?? "上游出错");
    return ["调用失败", what, attempt].filter(Boolean).join(" · ");
  }
  if (cls !== "" || (status !== null && status >= 400)) {
    return ["请求被拒绝", status !== null ? String(status) : null, attempt].filter(Boolean).join(" · ");
  }
  return ["无用量", attempt].filter(Boolean).join(" · ");
}

/** The 原因 a charge row shows: why it is unpriced, exempt or billed nothing; "—" for a charged one. */
export function chargeReason(c: Charge): string {
  switch (c.status) {
    case "unpriced": {
      const label = reasonLabel(c.error_code ?? "unknown");
      return c.adjusted_micro_cny !== null ? `${label} · 已通过重计价更正` : label;
    }
    case "exempt":
      return c.exemption ? (EXEMPTIONS[c.exemption] ?? c.exemption) : "—";
    case "nothing_to_bill":
      return nothingToBillReason(c);
    default:
      return c.adjusted_micro_cny !== null ? "已通过重计价更正" : "—";
  }
}
