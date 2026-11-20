import type { RequestTiming } from "../lib/api";
import { formatCount, formatDuration } from "../lib/chartFormat";

const duration = (value: number | null | undefined) => value == null ? "未记录" : formatDuration(value);

/** 各阶段按独立窗口展示，不用总耗时反推历史缺失或相加重叠窗口。 */
export function RequestTimingDetails({ data }: { data?: RequestTiming | null }) {
  const bytes = data?.request_body_bytes;
  const complete = data?.request_body_complete;
  const rows = [
    { key: "body_read_ms", label: "请求体读取窗口", value: duration(data?.body_read_ms) },
    { key: "json_decode_ms", label: "入口 JSON 解码", value: duration(data?.json_decode_ms) },
    { key: "pre_handler_ms", label: "到达至处理入口", value: duration(data?.pre_handler_ms) },
    { key: "request_body_bytes", label: "请求正文读取量", value: bytes == null ? "未记录" : `已读取 ${formatCount(bytes)} 字节` },
    { key: "request_body_complete", label: "请求体读取状态", value: complete == null ? "未记录" : complete ? "已读至结束" : "未完成" },
  ];
  return <section className="tenant-call-section" aria-label="请求入口阶段">
    <h3>请求入口阶段</h3>
    {data == null && <p className="hint">此记录未采集请求入口阶段。</p>}
    <dl className="tenant-call-facts">{rows.map(row => <div key={row.key} data-request-timing-field={row.key}><dt>{row.label}</dt><dd>{row.value}</dd></div>)}</dl>
    <p className="hint">读取窗口从网关开始读请求体起计，直到读完、出错或调用方断开。未完成时显示当时已读取的字节数。</p>
    <p className="hint">到达至处理入口从网关收到请求起计。部分接口先进入处理入口再读请求体，两个窗口可能重叠。</p>
    <p className="hint">请求首内容从网关收到请求起计，包含读取请求体与处理时间。上游首帧从本次上游尝试开始计时。</p>
  </section>;
}
