type Limits = { concurrency?: number; rpm?: number };
type TenantCaps = { key_concurrency_max?: number; key_rpm_max?: number };

export function keyLimitText(
  key: { rate_limit: Limits; effective_rate_limit?: Limits },
  dimension: keyof Limits,
  caps?: TenantCaps | null,
): string {
  const own = key.rate_limit[dimension];
  let effective = key.effective_rate_limit?.[dimension];
  // 旧控制面只返回密钥自身的配置；组织读成功后才能判断是否不限。
  if (key.effective_rate_limit === undefined) {
    if (!caps) return "未知";
    const cap = dimension === "concurrency" ? caps.key_concurrency_max : caps.key_rpm_max;
    effective = own === undefined || cap === undefined ? own ?? cap : Math.min(own, cap);
  }
  if (effective === undefined) return "不限";
  const value = effective.toLocaleString("en-US");
  if (own === undefined) return `${value}（沿用组织上限）`;
  if (own > effective) return `${value}（密钥配置 ${own.toLocaleString("en-US")}）`;
  return value;
}
