import { useEffect, useState } from "react";
import { ApiError, charges, usageEvents, type Charge, type StoredUsageEvent } from "../lib/api";
import { logValue } from "../lib/callLog";
import { formatDuration } from "../lib/chartFormat";
import { errorText } from "../lib/fmt";
import { fmtMicroCny } from "../lib/money";
import { fmtBeijing } from "../lib/time";
import { chargeReason } from "../lib/chargeReason";
import { Button } from "../ui/Button";
import { CopyButton } from "../ui/CopyButton";
import { Loading } from "../ui/DataTable";
import { ChargeCheck } from "../tabs/billing/Charges";
import { RequestTimingDetails } from "./RequestTimingDetails";
import { UpstreamTimingDetails } from "./UpstreamTimingDetails";
import { UsageIoAuditDetails } from "./UsageIoAuditDetails";
import { ESTIMATED_THROUGHPUT_NOTE, OUTPUT_THROUGHPUT_NOTE, formatThroughput } from "./UsagePerformance";
import { Card, CardHeader, PageHeader } from "./page";
import "./CallLogDetails.css";

interface LogResult {
  eventId: string;
  stored: StoredUsageEvent | null;
  charge: Charge | null;
  eventError: string | null;
  chargeError: string | null;
}

const numberValue = (value: unknown) => typeof value === "number" && Number.isFinite(value) ? value : null;
const timeValue = (value: unknown) => typeof value === "string" && Number.isFinite(Date.parse(value)) ? fmtBeijing(value) : logValue(value);

function Fields({ items }: { items: [string, unknown, string?][] }) {
  return <dl className="call-log-fields">{items.map(([label, value, title]) => <div key={label}><dt title={title}>{label}</dt><dd>{logValue(value)}</dd></div>)}</dl>;
}

/** 正文与 JSON 只展示已保存内容，长行换行且复制保持完整。 */
function LogText({ label, text, collapsed = false }: { label: string; text: string; collapsed?: boolean }) {
  const [copyError, setCopyError] = useState<string | null>(null);
  return <section className="call-log-text" aria-label={label}>
    <div className="call-log-text-heading"><h3>{label}</h3><CopyButton text={text} label={`复制${label}`} onFailed={cause => setCopyError(errorText(cause))} /></div>
    {copyError && <p className="note crit" role="alert">复制失败：{copyError}；可选中文字手动复制。</p>}
    <details open={!collapsed}><summary>展开 / 收起{label}</summary><pre tabIndex={0}>{text}</pre></details>
  </section>;
}

/** 精确读取一条事件，与其计价结果并行；列表不会逐行预取详情。 */
export function CallLogDetails({ eventId, backHref }: { eventId: string; backHref: string }) {
  const [result, setResult] = useState<LogResult | null>(null);
  const [reads, setReads] = useState(0);
  useEffect(() => {
    let live = true;
    void Promise.allSettled([usageEvents.get(eventId), charges.list({ event_id: eventId, limit: 1 })]).then(([event, priced]) => {
      if (!live) return;
      setResult({
        eventId,
        stored: event.status === "fulfilled" ? event.value : null,
        charge: priced.status === "fulfilled" ? priced.value.items[0] ?? null : null,
        eventError: event.status === "rejected" ? event.reason instanceof ApiError && event.reason.status === 404 ? null : errorText(event.reason) : null,
        chargeError: priced.status === "rejected" ? errorText(priced.reason) : null,
      });
    });
    return () => { live = false; };
  }, [eventId, reads]);
  const data = result?.eventId === eventId ? result : null;
  const stored = data?.stored;
  const charge = data?.charge;
  const event = stored?.event;
  const message = event?.error_message ?? charge?.error_message;
  const errorMessage = typeof message === "string" && message.length > 0 ? message : null;

  return <div className="call-log-page">
    <PageHeader title="调用日志详情" dataLead="查看本次计费尝试的结果、错误信息与已保存事件。" description="仅展示已采集内容；未采集的请求或响应正文无法补回。"
      actions={<><a className="btn btn-secondary call-log-back" href={backHref}>返回调用记录</a><Button className="call-log-refresh" icon="refresh" aria-label="刷新调用日志" title="刷新调用日志" onClick={() => { setResult(null); setReads(value => value + 1); }} /></>} />
    <p className="call-log-event-id">事件 <code>{eventId}</code></p>
    {!data && <Loading />}
    {data && <>
      {data.eventError && <p className="note crit" role="alert">事件读取失败：{data.eventError}</p>}
      {data.chargeError && <p className="note crit" role="alert">计价记录读取失败：{data.chargeError}</p>}
      {!stored && !charge && !data.eventError && !data.chargeError && <p className="note" role="status">未找到此调用记录，记录可能已过保留期或事件编号不存在。</p>}
      {(stored || charge) && <>
        <Card className="call-log-result" aria-label="调用结果与错误">
          <CardHeader title="调用结果与错误" />
          <Fields items={[
            ["HTTP 状态", event?.status_code ?? charge?.status_code],
            ["错误类别", event?.error_class ?? charge?.error_class],
            ["操作", event?.operation ?? charge?.operation],
            ["尝试类型", event ? event.attempt_kind : charge?.attempt_kind],
            ["尝试编号（从 0 开始）", event ? event.attempt_index : charge?.attempt_index],
            ["最终尝试", event?.is_final_attempt ?? charge?.is_final_attempt],
            ["已派发上游", event?.upstream_dispatched],
            ["缓存状态", event?.cache_status],
            ["结束原因", event?.finish_reason],
          ]} />
          {errorMessage ? <LogText key={`${eventId}-error`} label="错误信息" text={errorMessage} /> : <p className="hint mt-4">此事件未记录错误信息。</p>}
        </Card>
        <Card className="call-log-record" aria-label="调用详情">
        <section className="call-log-section" aria-label="请求与实际目标">
          <CardHeader title="请求与实际目标" />
          <Fields items={[
            ["请求 ID", stored?.request_id ?? event?.request_id],
            ["Trace ID", event?.trace_id],
            ["请求模型", event?.requested_model],
            ["请求模型 ID", event?.requested_model_id ?? charge?.requested_model_id],
            ["实际目标", event?.attempt_model],
            ["实际模型 ID", event?.model_id],
            ["上游模型", event?.upstream_model ?? charge?.upstream_model],
            ["提供方返回模型", event?.provider_model_version],
            ["上游账号", charge?.upstream_account_name],
            ["上游账号 ID", event?.provider_key_id ?? charge?.upstream_account_id],
            ["上游请求 ID", event?.provider_request_id],
            ["请求档位", event?.requested_service_tier ?? charge?.requested_service_tier],
            ["实际档位", event?.service_tier ?? charge?.service_tier],
            ["租户 ID", stored?.team_id ?? event?.team_id ?? charge?.tenant_id],
            ["成员 ID", event?.user_id ?? charge?.user_id],
            ["密钥 ID", stored?.api_key_id ?? event?.api_key_id ?? charge?.api_key_id],
            ["网关 ID", stored?.dp_id],
            ["上报批次", stored?.batch_id],
          ]} />
        </section>
        <section className="call-log-section" aria-label="时间与性能">
          <CardHeader title="时间与性能" description="时间均为北京时间；未记录的实测指标不补零。" />
          <Fields items={[
            ["完成时间", timeValue(stored?.occurred_at ?? charge?.occurred_at)],
            ["接收时间", timeValue(stored?.received_at)],
            ["首 Token 延迟", formatDuration(numberValue(event?.first_token_latency_ms ?? charge?.first_token_latency_ms))],
            ["上游尝试耗时", formatDuration(numberValue(event?.upstream_latency_ms ?? charge?.attempt_duration_ms))],
            ["客户端总耗时", formatDuration(numberValue(event?.downstream_latency_ms))],
            ["上游首帧延迟", formatDuration(numberValue(event?.upstream_ttft_ms))],
            ["输出 TPS", formatThroughput(charge), OUTPUT_THROUGHPUT_NOTE],
          ]} />
          {charge?.usage_estimated === true && <p className="hint">{ESTIMATED_THROUGHPUT_NOTE}</p>}
          <RequestTimingDetails data={event ? event.request_timing : charge?.request_timing} />
          <UpstreamTimingDetails data={event ? event.upstream_timing : charge?.upstream_timing} />
        </section>
        <section className="call-log-section" aria-label="用量与计费">
          <CardHeader title="用量与计费" />
          {charge ? <>
            <Fields items={[
              ["计价状态", { charged: "已计价", exempt: "免计费", nothing_to_bill: "无需计费", unpriced: "未计价" }[charge.status]],
              ["计价原因", chargeReason(charge)],
              ["售价（元）", charge.charge_micro_cny === null ? "未知" : fmtMicroCny(charge.charge_micro_cny)],
              ["成本（元）", charge.cost_micro_cny === null ? "未知" : fmtMicroCny(charge.cost_micro_cny)],
            ]} />
            <div className="call-log-charge-check"><ChargeCheck c={charge} /></div>
          </> : <><p className="hint">此事件没有可用的计价记录。</p>{event && <>
            <Fields items={[["原始输入 Token", event.prompt_tokens], ["原始输出 Token", event.completion_tokens], ["原始缓存命中 Token（输入子集）", event.cached_prompt_tokens], ["原始缓存读取 Token（追加）", event.cache_read_tokens], ["原始缓存创建 Token（追加）", event.cache_creation_tokens], ["原始推理 Token", event.reasoning_tokens], ["上游总 Token", event.total_tokens]]} />
            <UsageIoAuditDetails data={event} />
          </>}</>}
        </section>
        <section className="call-log-section" aria-label="已保存事件">
          <CardHeader title="已保存事件" description="已保存的事件字段与错误信息经过脱敏。正文长度不代表保存了正文内容。" />
          {stored ? <LogText key={`${eventId}-json`} label="事件 JSON" text={JSON.stringify(stored, null, 2)} collapsed /> : !data.eventError && <p className="hint">原始事件未保存或已过保留期；以上仍展示可用的计价记录。</p>}
        </section>
        </Card>
      </>}
    </>}
  </div>;
}
