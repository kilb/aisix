import type { UpstreamTiming } from "../lib/api";
import { formatCount } from "../lib/chartFormat";

const offset = (value: unknown): string => typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 4294967295 ? `${formatCount(value)} ms` : "—";

/** 只展示同一次上游尝试的原始偏移，不用其他阶段补齐缺失或计算差值。 */
export function UpstreamTimingDetails({ data }: { data?: UpstreamTiming | null }) {
  const rows = [
    { key: "http_send_started_ms", label: "交给 HTTP 客户端", value: offset(data?.http_send_started_ms) },
    { key: "response_headers_ms", label: "收到响应头", value: offset(data?.response_headers_ms) },
    { key: "first_response_byte_ms", label: "首个响应体字节", value: offset(data?.first_response_byte_ms) },
    { key: "first_protocol_frame_ms", label: "首个协议帧", value: offset(data?.first_protocol_frame_ms) },
    { key: "first_canonical_chunk_ms", label: "首个规范化片段", value: offset(data?.first_canonical_chunk_ms) },
    { key: "upstream_first_content_ms", label: "上游首个内容帧", value: offset(data?.upstream_first_content_ms) },
    { key: "first_content_ms", label: "首次内容交付", value: offset(data?.first_content_ms) },
  ];
  return <section className="mt-5 min-w-0 border-t border-border pt-4" aria-label="上游 HTTP 阶段">
    <h3 className="text-caption font-medium">上游 HTTP 阶段</h3>
    <dl className="grid min-w-0 gap-x-6 tablet:grid-cols-2 desktop:grid-cols-3">{rows.map(row => <div key={row.key} data-upstream-timing-field={row.key} className="min-w-0 border-b border-border py-3"><dt className="text-caption text-muted-foreground">{row.label}</dt><dd className="mt-1 text-small tabular-nums">{row.value}</dd></div>)}</dl>
    <p className="mt-3 text-meta text-muted-foreground">各值均相对同一次上游尝试起点，按原始毫秒展示，不计算阶段差值；协议帧与规范化片段未采集时保留未知，不由其他阶段推测。</p>
    <p className="mt-2 text-meta text-muted-foreground">上游首个内容帧记录解析器首次读到完整且含非空生成内容的帧的时点。</p>
    <p className="mt-2 text-meta text-muted-foreground">交给 HTTP 客户端表示开始调用 HTTP 客户端，不代表操作系统实际写包；首次内容交付表示网关交给 HTTP server，不代表客户端已收到或确认。</p>
  </section>;
}
