import { routingModeOf, routingPolicy, selectRoutingMode, type RoutingMode, type RoutingModeValue } from "../lib/routingModes";
import { Field } from "./Field";

export interface RoutingModeFieldProps {
  value: RoutingModeValue;
  onChange: (next: RoutingModeValue) => void;
  disabled?: boolean;
  strategies?: readonly string[] | null;
  error?: string | null;
  hashError?: string | null;
  storedHashOn?: unknown;
}

export function RoutingModeField({ value: draft, onChange, disabled, strategies, error, hashError, storedHashOn }: RoutingModeFieldProps) {
  // 调用方可以传完整草稿，但本控件只能更新自己负责的两个字段。
  const value: RoutingModeValue = { strategy: draft.strategy, affinity: draft.affinity };
  const mode = routingModeOf(value);
  const policy = routingPolicy(value);
  const allowed = (strategy: string) => strategies === undefined || strategies?.includes(strategy) === true;
  const otherStrategies = (strategies ?? []).filter((strategy) => !["consistent_hash", "round_robin", "failover"].includes(strategy));
  const changed = (choice: string) => {
    if (choice.startsWith("strategy:")) onChange({ ...value, strategy: choice.slice("strategy:".length) });
    else onChange(selectRoutingMode(value, choice as RoutingMode));
  };
  return <>
    <Field label="同级分配方式" error={error} hint={<>
      <span className="block">当前：{policy.title}（{value.strategy}）{value.strategy === "consistent_hash" ? `，${policy.affinityLabel}` : ""}。{policy.help}</span>
      <span className="block">priority 越大越先用；备组设较低 priority 和正权重。weight=0 可能被过滤，不能代替备组。仅显式选择推荐项并保存才改变亲和配置。</span>
      <span className="block">错误切换与冷却独立；max_fallbacks 限制整条候选链，可能到不了低优先级备组。Responses 会话续接优先原归属。</span>
    </>}>
      {(aria) => <select value={mode} disabled={disabled || strategies === null} onChange={(event) => changed(event.target.value)} {...aria}>
        {mode === "preserve" && <option value="preserve">保留当前配置：{policy.title}{value.strategy === "consistent_hash" ? ` · ${policy.affinityLabel}` : ""}</option>}
        <option value="api_key" disabled={!allowed("consistent_hash")}>推荐：同级加权一致性哈希 · API key ID 亲和</option>
        <option value="round_robin" disabled={!allowed("round_robin")}>同级按请求加权轮询</option>
        <option value="failover" disabled={!allowed("failover")}>同级固定顺序</option>
        {otherStrategies.map((strategy) => <option key={strategy} value={`strategy:${strategy}`}>{routingPolicy({ ...value, strategy }).title}（{strategy}）</option>)}
      </select>}
    </Field>
    {value.strategy === "consistent_hash" && <Field label="亲和来源" error={hashError} hint="API key ID 是稳定的密钥资源标识，不使用密钥明文。权重分配 key 空间，不保证请求份额；目标或权重变化可能改变映射。">
      {(aria) => <select value={value.affinity} disabled={disabled} onChange={(event) => onChange({ ...value, affinity: event.target.value as RoutingModeValue["affinity"] })} {...aria}>
        <option value="default">网关默认：请求头 → API key ID</option>
        <option value="api_key">API key ID 亲和</option>
        {(storedHashOn !== undefined || value.affinity === "custom") && <option value="custom">保留已存自定义来源链</option>}
      </select>}
    </Field>}
  </>;
}
