import type { PublicModelRouting } from "./api.ts";

export interface RoutingModeValue {
  strategy: string;
  affinity: "default" | "api_key" | "custom";
}

export type RoutingMode = "api_key" | "round_robin" | "failover" | "preserve";

export function routingValueOf(routing: PublicModelRouting, defaultStrategy: string): RoutingModeValue {
  const source = routing.hash_on?.[0];
  const apiKey = routing.hash_on?.length === 1 && source !== null && typeof source === "object" &&
    Object.keys(source).length === 1 && "type" in source && source.type === "api_key";
  return {
    strategy: typeof routing.strategy === "string" && routing.strategy !== "" ? routing.strategy : defaultStrategy,
    affinity: routing.hash_on === undefined ? "default" : apiKey ? "api_key" : "custom",
  };
}

export function routingModeOf(value: RoutingModeValue): RoutingMode {
  if (value.strategy === "consistent_hash" && value.affinity === "api_key") return "api_key";
  if (value.strategy === "round_robin" || value.strategy === "failover") return value.strategy;
  return "preserve";
}

// 只有显式选推荐项才将亲和来源改成 API key 资源 ID；其它字段由原表单保存。
export function selectRoutingMode<T extends RoutingModeValue>(value: T, mode: RoutingMode): T {
  if (mode === "preserve") return value;
  if (mode === "api_key") return { ...value, strategy: "consistent_hash", affinity: "api_key" };
  return { ...value, strategy: mode };
}

const policies: Record<string, { title: string; help: string }> = {
  round_robin: { title: "同级按请求加权轮询", help: "同优先级按权重轮询，接受同一 API key 的不同请求分到不同目标。" },
  consistent_hash: { title: "同级加权一致性哈希", help: "权重分配亲和键空间，不保证请求数、Token 数或耗时按比例分配；热点 key 仍可能集中在一个目标。" },
  failover: { title: "同级固定顺序", help: "同优先级按目标声明顺序尝试；权重不代表流量份额。" },
  least_cost: { title: "最低成本", help: "同优先级先尝试定价最低的目标，没有价格的目标排在后面。" },
  least_latency: { title: "最低延迟", help: "同优先级先尝试近期延迟较低的目标，无采样目标优先获得探测机会。" },
  least_busy: { title: "最少负载", help: "同优先级按进行中请求数除以权重排序。" },
};

// 表单和已保存摘要共用策略解释，避免把推荐项误当成当前配置。
export function routingPolicy(value: RoutingModeValue) {
  return {
    ...(policies[value.strategy] ?? { title: value.strategy, help: "保留已存策略及参数。" }),
    affinityLabel: value.affinity === "api_key" ? "API key ID 亲和" :
      value.affinity === "custom" ? "自定义亲和来源链" : "网关默认：请求头 → API key ID",
  };
}
