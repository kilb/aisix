import type { PublicModelRouting } from "../lib/api";

const policies: Record<string, { title: string; help: string }> = {
  round_robin: { title: "加权轮询", help: "同优先级按权重轮询；失败后尝试后续目标。" },
  consistent_hash: { title: "一致性哈希", help: "同一亲和键尽量保持在同一健康目标；失败后沿哈希环切换。" },
  failover: { title: "顺序故障切换", help: "同优先级按目标顺序尝试，失败后切换；不随机，也不按权重分配。" },
  least_cost: { title: "最低成本", help: "同优先级先尝试定价最低的目标，没有价格的目标排在后面。" },
  least_latency: { title: "最低延迟", help: "同优先级先尝试近期延迟较低的目标，无采样目标优先获得探测机会。" },
  least_busy: { title: "最少负载", help: "同优先级按进行中请求数除以权重排序。" },
};

// 只接受后端保存的路由文档；默认值由调用方读取 schema 后显式给出。
export function RoutePolicy({ routing, defaultStrategy, compact = false }: {
  routing: PublicModelRouting | null | undefined;
  defaultStrategy?: string | null;
  compact?: boolean;
}) {
  const strategy = typeof routing?.strategy === "string" ? routing.strategy : defaultStrategy;
  if (!routing) return <span className="text-caption text-muted">尚无路由文档</span>;
  if (!strategy) return <span className="text-caption text-muted">策略默认值未确认</span>;
  const policy = policies[strategy];
  const apiKey = JSON.stringify(routing.hash_on) === JSON.stringify([{ type: "api_key" }]);
  const on429 = routing.retry_on_429 === true || routing.fallback_on_statuses?.includes(429);
  return (
    <div className="min-w-0" aria-label="当前路由策略">
      <span className="tag border-line bg-well text-ink">{policy?.title ?? strategy}</span>
      <code className="ml-2 text-meta text-muted">{strategy}</code>
      {strategy === "consistent_hash" && <span className="ml-2 text-caption text-ink-soft">{apiKey ? "API key 亲和" : routing.hash_on ? "自定义亲和键" : "请求头 → API key ID"}</span>}
      {!compact && <p className="hint mt-2 text-caption">优先级高的目标先用。{policy?.help ?? "使用已保存的策略。"}{on429 ? "429 也参与重试和切换。" : "429 默认不参与重试和切换。"}{Array.isArray(routing.fallback_on_statuses) && routing.fallback_on_statuses.length > 0 ? `额外重试状态码：${routing.fallback_on_statuses.join("、")}。` : ""}</p>}
    </div>
  );
}
