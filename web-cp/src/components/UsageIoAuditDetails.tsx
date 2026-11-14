import { isSupportedUsageIoAudit, type TextMeasurement, type UsageAuditData } from "../lib/usageIoAudit";

const count = new Intl.NumberFormat("en-US");
const state = (complete: boolean | null | undefined) => complete == null ? "完整性未知" : complete ? "完整" : "部分";
const recorded = (value: number | null | undefined) => value == null ? "未记录" : count.format(value);

function bodyValue(bytes: number | null | undefined, complete: boolean | null | undefined): string {
  return bytes == null ? "未采集" : `${count.format(bytes)} 字节 · ${state(complete)}`;
}

function textValue(text: TextMeasurement | null | undefined): string {
  return text == null ? "未采集" : `${count.format(text.utf8_bytes)} UTF-8 字节 · ${count.format(text.unicode_scalars)} Unicode 字符 · ${state(text.complete)}`;
}

function referenceValue(value: number | null | undefined, text: TextMeasurement | null | undefined): string {
  return value == null || text == null ? "不可用" : `${count.format(value)} · ${state(text.complete)}`;
}

/** 按记录边界展示独立长度参考，不推断完整计费文本或修改计费用量。 */
export function UsageIoAuditDetails({ data }: { data: UsageAuditData }) {
  const audit = isSupportedUsageIoAudit(data.io_audit) ? data.io_audit : undefined;
  const upstream = audit?.boundary === "upstream_attempt";
  const legacy = audit?.boundary === "client_exchange";
  const scope = upstream ? "上游" : legacy ? "客户端" : "";
  const reference = audit && data.io_token_reference?.method === "utf8_bytes_div4_v1" ? data.io_token_reference : undefined;
  // 旧口径只在完整读取请求后记录正文长度，新口径须显式携带完整性。
  const requestComplete = legacy && audit.request_complete === undefined ? true : audit?.request_complete;
  const transportScope = audit?.transport === "http_entity"
    ? "上游 HTTP 应用层正文；响应按解压后计，含 JSON、SSE 或媒体包装，不含 HTTP 头、TLS 或重传，不代表压缩后网络流量。"
    : audit?.transport === "websocket_payload"
    ? "上游 WebSocket 消息载荷，含 JSON 或媒体包装，不含 WebSocket 帧头、TLS 或重传，不代表压缩后网络流量。"
    : audit?.transport === "mixed"
    ? "上游 HTTP 正文与 WebSocket 消息载荷混合合计，含各自的包装，HTTP 响应按解压后计；不含协议头、TLS 或重传，不代表压缩后网络流量。"
    : "上游传输类型未记录或暂不支持；不将其视为 HTTP 正文或压缩后网络流量。";
  const rows = [
    { key: "request-body", label: `${scope}请求正文`, value: bodyValue(audit?.request_body_bytes, requestComplete) },
    { key: "response-body", label: `${scope}响应正文`, value: bodyValue(audit?.response_body_bytes, audit?.response_complete) },
    { key: "input-text", label: `${scope}输入文本`, value: textValue(audit?.input_text) },
    { key: "output-text", label: `${scope}输出文本`, value: textValue(audit?.output_text) },
    { key: "input-reference", label: "输入 Token 参考", value: referenceValue(reference?.input_tokens, audit?.input_text) },
    { key: "output-reference", label: "输出 Token 参考", value: referenceValue(reference?.output_tokens, audit?.output_text) },
    { key: "recorded-input", label: "记录输入 Token", value: recorded(data.usage_input_tokens) },
    { key: "recorded-output", label: "记录输出 Token", value: recorded(data.usage?.completion_tokens) },
    { key: "usage-source", label: "用量来源", value: data.usage_estimated === true ? "包含网关估算" : data.usage_estimated === false ? "已记录用量" : "来源未记录" },
  ];
  return <details className="io-audit-details mt-4 min-w-0 border-t border-border pt-3 lg:col-span-2" aria-label="输入输出与 Token 参考">
    <summary className="cursor-pointer text-caption font-medium">输入输出与 Token 参考</summary>
    {upstream ? <p className="mt-3 text-meta text-muted-foreground">本次上游尝试{audit.exchange_count != null && audit.exchange_count > 1 ? ` · ${count.format(audit.exchange_count)} 次上游交换合计` : ""}；请求转换后、客户端响应处理前采集。“完整”表示该项观测已结束，字符按 Unicode 码点计。{audit.exchange_count != null && audit.exchange_count > 1 ? "包含多次上游交换，暂不提供单次 Token 对照。" : ""}</p>
      : legacy ? <p className="mt-3 text-meta text-muted-foreground">历史客户端交换统计，仅统计客户端可见文本；“完整”表示这部分观测已结束，不代表完整计费输入。字符按 Unicode 码点计。</p>
      : data.io_audit != null ? <p className="mt-3 text-meta text-muted-foreground">此记录的统计版本或边界暂不支持，长度与参考未展示。</p>
      : <p className="mt-3 text-meta text-muted-foreground">此记录未采集输入输出长度。</p>}
    <dl className="mt-3 grid min-w-0 gap-x-6 gap-y-2 text-caption sm:grid-cols-2">
      {rows.map(row => <div key={row.key} data-io-field={row.key} className="min-w-0">
        <dt className="text-muted-foreground">{row.label}</dt>
        <dd className="mt-1 break-words tabular-nums">{row.value}</dd>
      </div>)}
    </dl>
    {audit ? <><p className="mt-3 text-meta text-muted-foreground">Token 参考未校准：UTF-8 字节数 ÷ 4 向上取整，不用于计费。{legacy ? "未包含工具定义、结构描述、多模态、隐藏推理与存储上下文。" : "文本参考仅覆盖本次已观测的可解码内容；工具/结构描述、媒体、隐藏推理或托管上下文是否可见取决于上游协议，部分或未知不代表完整计费用量。"}</p>
    <p className="mt-2 text-meta text-muted-foreground">{legacy ? "大小仅含正文，不含协议头和加密开销；压缩请求按压缩后大小记录，流式响应含事件包装。“响应完整”仅表示已交给 HTTP 服务，不保证客户端收到。" : `正文大小与文本长度分开统计。${transportScope}`}</p></> : null}
  </details>;
}
