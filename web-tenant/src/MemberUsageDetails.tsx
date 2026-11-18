import type { MemberUsage } from "./lib/memberUsage";
import { memberChargeLabel } from "./lib/memberUsagePresentation";
import { formatCount } from "./lib/chartFormat";
import { DataTable } from "./ui/DataTable";
import { Metric } from "./components/page";
import "./Members.css";

const QUANTITY_LABEL: Record<string, string> = {
  input: "输入", output: "输出", cache_read: "缓存读", cache_write: "缓存写",
  cache_write_1h: "1 小时缓存写", cache_write_12h: "12 小时缓存写", reasoning: "推理", audio_input: "音频输入",
  audio_output: "音频输出", image_input: "图片输入", image_output: "图片输出", web_search: "联网搜索",
};

/** 已记录部分与未知部分分开展示，缺失明细不合计为零。 */
export function MemberUsageDetails({ usage }: { usage: MemberUsage }) {
  const quantities = Object.entries(usage.quantities);
  return <>
    <dl className="member-usage-summary">
      <div><dt>已计费费用</dt><dd><Metric>{memberChargeLabel(usage)}</Metric></dd></div>
      <div><dt>已记录调用</dt><dd><Metric>{formatCount(usage.requests)}</Metric></dd></div>
    </dl>
    <h3 className="text-title font-bold">计费用量</h3>
    {quantities.length === 0 ? <p className="hint">{usage.unknown_quantity_requests > 0 ? "计费用量未知" : "暂无已记录的计费用量"}</p> : <DataTable label="计费用量">
      <thead><tr><th>用量类型</th><th className="right">用量</th></tr></thead>
      <tbody>{quantities.map(([dimension, quantity]) => <tr key={dimension}>
        <td data-label="用量类型">{QUANTITY_LABEL[dimension] ?? dimension}</td>
        <td className="right tabular" data-label="用量">{formatCount(quantity)}{dimension === "web_search" ? " 次" : dimension in QUANTITY_LABEL ? " token" : ""}</td>
      </tr>)}</tbody>
    </DataTable>}
    <div className="grid gap-2 text-caption text-fg-3" role="note" aria-label="费用与用量说明">
      {usage.unknown_quantity_requests > 0 && <p>{formatCount(usage.unknown_quantity_requests)} 次用量明细未记录；上方仅显示已知部分。</p>}
      {usage.unpriced_requests > 0 && <p>{formatCount(usage.unpriced_requests)} 次未计价，费用尚未包含。</p>}
      {usage.pending_requests > usage.unpriced_requests && <p>另有 {formatCount(usage.pending_requests - usage.unpriced_requests)} 次待计价。</p>}
      {usage.estimated_requests > 0 && <p>{formatCount(usage.estimated_requests)} 次含估算用量：上游未完整返回用量，部分 token 由网关估算，费用按该用量计费。</p>}
    </div>
  </>;
}
