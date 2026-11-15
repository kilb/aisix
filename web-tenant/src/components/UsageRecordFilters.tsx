import { Select } from "../ui/Select";
import { useId, useState, type InputHTMLAttributes, type ReactNode } from "react";
import { ChevronDown, Filter, RotateCcw } from "lucide-react";
import { Button } from "../ui/Button";
import type { UsageRecordFilterKey, UsageRecordFilterValues } from "../lib/usageRecordFilters";
export { EMPTY_USAGE_RECORD_FILTERS } from "../lib/usageRecordFilters";
export type { UsageRecordFilterKey, UsageRecordFilterValues } from "../lib/usageRecordFilters";
import "./UsageRecordFilters.css";

export interface UsageRecordFiltersProps {
  value: UsageRecordFilterValues;
  onChange: (patch: Partial<UsageRecordFilterValues>) => void;
  scope: "admin" | "tenant";
  disabled?: boolean;
  /** 调用方只公开当前服务端已支持且有权限使用的字段。 */
  fields?: readonly UsageRecordFilterKey[];
  children?: ReactNode;
}

const DEFAULT_FIELDS: readonly UsageRecordFilterKey[] = ["outcome", "http_status", "cache_hit", "service_tier", "operation", "final_attempt", "error_class"];
const OPERATIONS = [
  ["chat", "对话补全"], ["completions", "文本补全"], ["messages", "Messages"], ["responses", "Responses"],
  ["count_tokens", "Token 计数"], ["embeddings", "向量"], ["rerank", "重排"],
  ["image_generation", "图像生成"], ["image_edit", "图像编辑"], ["transcription", "音频转写"],
  ["translation", "音频翻译"], ["speech", "语音合成"], ["video_generation", "视频生成"], ["realtime", "实时会话"],
  ["files", "文件管理"], ["batches", "批量任务管理"], ["batch_completion", "批量任务结算"], ["fine_tuning", "微调任务"],
  ["mcp", "MCP"], ["a2a", "A2A"], ["passthrough", "透传"], ["systemone", "System One"], ["decisions", "决策"],
] as const;
const TENANT_ERRORS = [
  ["model_not_found", "模型不存在"], ["client_disconnected", "客户端断开"], ["invalid_request", "请求无效"],
  ["rate_limited", "请求限流"], ["budget_exceeded", "额度不足"], ["content_filtered", "内容拦截"],
  ["upstream_unavailable", "上游暂不可用"], ["failed", "其他失败"],
] as const;

/** 文本条件录完后提交，避免状态码尚未输入完整就发起无效请求。 */
function FilterTextInput({ value, onCommit, ...props }: { value: string; onCommit: (value: string) => void } & Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "onChange">) {
  const [draft, setDraft] = useState(value);
  const commit = () => { if (draft.trim() !== value) onCommit(draft.trim()); };
  return <input {...props} value={draft} onChange={event => setDraft(event.target.value)} onBlur={event => { if (event.currentTarget.reportValidity()) commit(); }} onKeyDown={event => {
    if (event.key === "Enter") { event.preventDefault(); if (event.currentTarget.reportValidity()) commit(); }
    if (event.key === "Escape") { event.preventDefault(); setDraft(value); }
  }} />;
}

function SelectedValue({ value, choices }: { value: string; choices: readonly string[] }) {
  return value && !choices.includes(value) ? <option value={value}>当前条件：{value}</option> : null;
}

/** 只负责受控筛选，翻页复位与真实服务端查询由记录页面统一执行。 */
export function UsageRecordFilters({ value, onChange, scope, disabled = false, fields = DEFAULT_FIELDS, children }: UsageRecordFiltersProps) {
  const id = useId();
  const [expanded, setExpanded] = useState(false);
  const visibleFields = fields.filter(field => field !== "http_status" || scope === "admin");
  const has = (field: UsageRecordFilterKey) => visibleFields.includes(field);
  const active = visibleFields.filter(field => value[field] !== "").length;
  const advanced = visibleFields.filter(field => field !== "outcome" && value[field] !== "").length;
  const choose = (field: UsageRecordFilterKey, next: string) => onChange({ [field]: next });
  const clear = () => onChange(Object.fromEntries(visibleFields.map(field => [field, ""])) as Partial<UsageRecordFilterValues>);
  return <section className="usage-record-filters" aria-label="调用结果筛选">
    <div className="usage-record-filter-primary">
      {has("outcome") && <label className="usage-record-filter-outcome"><span id={`${id}-outcome-label`}>调用结果</span><Select aria-labelledby={`${id}-outcome-label`} disabled={disabled} value={value.outcome} onValueChange={selectedValue => choose("outcome", selectedValue)}>
        <option value="">全部结果</option><option value="succeeded">成功</option><option value="failed">失败</option><option value="retried">已重试</option><option value="unknown">结果未知</option>
        {SelectedValue({ value: value.outcome, choices: ["succeeded", "failed", "retried", "unknown"] })}
      </Select></label>}
      {children}
      <Button kind="quiet" className="usage-record-filter-more" disabled={disabled} aria-controls={`${id}-advanced`} aria-expanded={expanded} onClick={() => setExpanded(value => !value)}><Filter size={16} aria-hidden /><span>更多筛选{advanced > 0 ? `（${advanced}）` : ""}</span><ChevronDown size={16} aria-hidden /></Button>
      {active > 0 && <Button kind="quiet" iconOnly aria-label="清除调用结果筛选" title="清除调用结果筛选" disabled={disabled} onClick={clear}><RotateCcw size={16} aria-hidden /></Button>}
    </div>
        <fieldset id={`${id}-advanced`} hidden={!expanded} disabled={disabled} className="usage-record-filter-grid">
          <legend className="vh">更多调用条件</legend>
          {has("http_status") && scope === "admin" && <label><span id={`${id}-status-label`}>HTTP 状态码</span><FilterTextInput aria-labelledby={`${id}-status-label`} key={`status/${value.http_status}`} inputMode="numeric" pattern="[1-5][0-9]{2}" maxLength={3} placeholder="例如 400、429、503" value={value.http_status} onCommit={next => choose("http_status", next)} /></label>}
          {has("error_class") && <label><span id={`${id}-error-label`}>失败类型</span>{scope === "admin" ? <FilterTextInput aria-labelledby={`${id}-error-label`} key={`error/${value.error_class}`} placeholder="填写准确的错误类型" value={value.error_class} onCommit={next => choose("error_class", next)} /> : <Select aria-labelledby={`${id}-error-label`} value={value.error_class} onValueChange={selectedValue => choose("error_class", selectedValue)}><option value="">全部类型</option>{TENANT_ERRORS.map(([key, label]) => <option key={key} value={key}>{label}</option>)}{SelectedValue({ value: value.error_class, choices: TENANT_ERRORS.map(([key]) => key) })}</Select>}</label>}
          {has("cache_hit") && <label><span id={`${id}-cache-label`}>响应缓存</span><Select aria-labelledby={`${id}-cache-label`} value={value.cache_hit} onValueChange={selectedValue => choose("cache_hit", selectedValue)}><option value="">全部缓存状态</option><option value="true">命中</option><option value="false">未命中（含绕过／禁用）</option>{SelectedValue({ value: value.cache_hit, choices: ["true", "false"] })}</Select></label>}
          {has("service_tier") && <label><span id={`${id}-tier-label`}>实际服务档位</span><FilterTextInput aria-labelledby={`${id}-tier-label`} key={`tier/${value.service_tier}`} list={`${id}-tiers`} placeholder="例如 default、fast" value={value.service_tier} onCommit={next => choose("service_tier", next)} /><datalist id={`${id}-tiers`}>{["default", "priority", "fast", "flex", "batch"].map(tier => <option key={tier} value={tier} />)}</datalist></label>}
          {has("operation") && <label><span id={`${id}-operation-label`}>API 操作</span><Select aria-labelledby={`${id}-operation-label`} value={value.operation} onValueChange={selectedValue => choose("operation", selectedValue)}><option value="">全部操作</option>{OPERATIONS.map(([key, label]) => <option key={key} value={key}>{label}</option>)}{SelectedValue({ value: value.operation, choices: OPERATIONS.map(([key]) => key) })}</Select></label>}
          {has("final_attempt") && <label><span id={`${id}-attempt-label`}>尝试记录</span><Select aria-labelledby={`${id}-attempt-label`} value={value.final_attempt} onValueChange={selectedValue => choose("final_attempt", selectedValue)}><option value="">全部尝试</option><option value="true">最终尝试</option><option value="false">非最终尝试</option>{SelectedValue({ value: value.final_attempt, choices: ["true", "false"] })}</Select></label>}
        </fieldset>
    <p className="usage-record-filter-note">筛选作用于全部匹配记录；文本条件按 Enter 或移出输入框后应用。结果未知不计入成功或失败。</p>
  </section>;
}
