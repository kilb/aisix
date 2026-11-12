import type { PublicModelRouting } from "../lib/api";
import { routingPolicy, routingValueOf } from "../lib/routingModes";

// 只接受后端保存的路由文档；默认值由调用方读取 schema 后显式给出。
export function RoutePolicy({ routing, defaultStrategy, compact = false }: {
  routing: PublicModelRouting | null | undefined;
  defaultStrategy?: string | null;
  compact?: boolean;
}) {
  const strategy = typeof routing?.strategy === "string" ? routing.strategy : defaultStrategy;
  if (!routing) return <span className="text-caption text-muted">尚无路由文档</span>;
  if (!strategy) return <span className="text-caption text-muted">策略默认值未确认</span>;
  const policy = routingPolicy(routingValueOf(routing, strategy));
  const on429 = routing.retry_on_429 === true || routing.fallback_on_statuses?.includes(429);
  return (
    <div className="min-w-0" aria-label="当前路由策略">
      <span className="tag border-line bg-well text-ink">{policy.title}</span>
      <code className="ml-2 text-meta text-muted">{strategy}</code>
      {strategy === "consistent_hash" && <span className="ml-2 text-caption text-ink-soft">{policy.affinityLabel}</span>}
      {!compact && <>
        <p className="hint mt-2 text-caption">priority 越大越先用，同级再按当前策略分配。{policy.help}备组用较低 priority 和正权重；存在正权重候选时，weight=0 的目标可能被过滤。</p>
        <p className="hint text-caption">只有错误分类和路由配置允许时才切换，冷却独立决定目标的暂时可用性。{on429 ? "已配置 429 参与重试和切换。" : "429 默认不参与重试和切换。"}{Array.isArray(routing.fallback_on_statuses) && routing.fallback_on_statuses.length > 0 ? `额外重试状态码：${routing.fallback_on_statuses.join("、")}。` : ""}{routing.max_fallbacks !== undefined ? `max_fallbacks=${routing.max_fallbacks} 限制整条候选链，可能到不了低优先级备组。` : "未设置 max_fallbacks；仍受重试与请求超时预算限制。"}</p>
        <p className="hint text-caption">全部目标冷却时仍可能尝试冷却目标；「全部不可用时失败」针对后台健康检查。响应交付后不重新派发；Responses 会话续接优先原归属，可能覆盖普通优先级与哈希顺序。</p>
      </>}
    </div>
  );
}
