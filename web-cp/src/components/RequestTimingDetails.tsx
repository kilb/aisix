import type { RequestTiming } from "../lib/api";
import { formatCount, formatDuration } from "../lib/chartFormat";

const duration = (value: number | null | undefined) => value == null ? "未记录" : formatDuration(value);

/** 各阶段按独立窗口展示，不用总耗时反推历史缺失或相加重叠窗口。 */
export function RequestTimingDetails({ data }: { data?: RequestTiming | null }) {
  const bytes = data?.request_body_bytes;
  const complete = data?.request_body_complete;
  const rows = [
    { key: "body_read_ms", label: "请求体读取窗口", value: duration(data?.body_read_ms) },
    { key: "json_decode_ms", label: "首次 JSON 解析", value: duration(data?.json_decode_ms) },
    { key: "pre_handler_ms", label: "到达至处理入口", value: duration(data?.pre_handler_ms) },
    { key: "request_body_bytes", label: "请求正文读取量", value: bytes == null ? "未记录" : `已读取 ${formatCount(bytes)} 字节` },
    { key: "request_body_complete", label: "请求体读取状态", value: complete == null ? "未记录" : complete ? "已读至结束" : "未完成" },
  ];
  return <section className="mt-5 min-w-0 border-t border-border pt-4" aria-label="请求入口阶段">
    <h3 className="text-caption font-medium">请求入口阶段</h3>
    {data == null && <p className="hint mt-2">此记录未采集请求入口阶段，历史缺失不按零显示。</p>}
    <dl className="call-log-fields">{rows.map(row => <div key={row.key} data-request-timing-field={row.key}><dt>{row.label}</dt><dd>{row.value}</dd></div>)}</dl>
    <p className="mt-3 text-meta text-muted-foreground">读取窗口从实际开始读取请求体计时，到读取结束、失败或取消为止；未完成时，字节数仅为已读取量。JSON 解析仅记录实际执行的首次解析。</p>
    <p className="mt-2 text-meta text-muted-foreground">到达至处理入口从请求到达起计；部分接口先进入 handler 再读取请求体，阶段可能重叠，不能直接相加。</p>
    <p className="mt-2 text-meta text-muted-foreground">首 Token 延迟从请求到达起计，包含上传和处理时间；上游首帧延迟只对应本次上游尝试，两者起点不同。</p>
  </section>;
}
