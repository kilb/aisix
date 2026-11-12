import { useState } from "react";
import type { TabProps } from "../App";
import { ApiMetricsPanel } from "../components/ApiMetricsPanel";
import { PageHeader } from "../components/page";
import { metricViewOf } from "../lib/analysisNavigation";
import { useHashQuery } from "../lib/useHashQuery";
import { beijingInputValue, parseBeijingInput } from "../lib/time";

/** 独立分析页保持同一指标容器，切换指标只更新 view。 */
export function Analytics(_props: TabProps) {
  const [query, update] = useHashQuery();
  const [fallback] = useState(() => {
    const to = new Date();
    return { from: new Date(to.getTime() - 86_400_000).toISOString(), to: to.toISOString() };
  });
  const from = query.get("from") ?? fallback.from;
  const to = query.get("to") ?? fallback.to;
  const dateValue = (value: string) => Number.isFinite(Date.parse(value)) ? beijingInputValue(new Date(value)) : value;
  const changeDate = (key: string, value: string) => update({ from, to, [key]: parseBeijingInput(value) ?? value });
  const calls = new URLSearchParams();
  for (const key of ["model_id", "api_key_id", "tenant_id", "from", "to"]) {
    const value = key === "from" ? from : key === "to" ? to : query.get(key);
    if (value) calls.set(key, value);
  }
  return <>
    <PageHeader title="用量分析" eyebrow="ANALYTICS · API 请求" description="按完整时间范围统计，切换指标保留筛选。时间均为北京时间；最多查询 92 天或 1500 个时间桶，超出范围请调整，不裁剪历史。" />
    <div className="mb-5 flex flex-wrap items-end gap-3">
      <label className="f filter"><span>开始时间（北京时间）</span><input type="datetime-local" step="1" value={dateValue(from)} onChange={e => changeDate("from", e.target.value)} /></label>
      <label className="f filter"><span>结束时间（北京时间）</span><input type="datetime-local" step="1" value={dateValue(to)} onChange={e => changeDate("to", e.target.value)} /></label>
      {([["tenant_id", "组织 ID"], ["model_id", "模型 ID"], ["api_key_id", "密钥 ID"], ["member_id", "成员 ID"]] as const).map(([key, label]) => <label className="f filter" key={key}><span>{label}</span><input value={query.get(key) ?? ""} onChange={e => update({ from, to, [key]: e.target.value })} /></label>)}
      <a className="inline-flex min-h-11 min-w-11 items-center" href={`#calls?${calls}`}>查看调用记录</a>
    </div>
    <ApiMetricsPanel scope="admin" enabled from={from} to={to} view={metricViewOf(query.get("view"))} onViewChange={view => update({ view, from, to })} modelId={query.get("model_id") || undefined} apiKeyId={query.get("api_key_id") || undefined} tenantId={query.get("tenant_id") || undefined} memberId={query.get("member_id") || undefined} />
  </>;
}
