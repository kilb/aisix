import { call, type PricedModel } from "./api";

export type Dimension = "input" | "output" | "cache_read" | "cache_write" | "cache_write_1h" |
  "reasoning" | "audio_input" | "audio_output" | "image_input" | "image_output" | "web_search";

export interface PriceRate {
  dimension: Dimension;
  unit: "token" | "request";
  price_cny_per_million: string | null;
  fallback_from?: Dimension;
}

export interface TierQuote {
  base: PriceRate[];
  long_context: { above_input_tokens: number; rates: PriceRate[] }[];
}

export interface QuotedModel extends PricedModel {
  cny_per_usd: string;
  effective_multiplier: string;
  prices_by_tier: Record<string, TierQuote>;
}

export interface QuotedGroup {
  id: string;
  display_name: string;
  description: string;
  models: QuotedModel[];
}

export interface PriceQuoteList {
  currency: "CNY";
  groups: QuotedGroup[];
  discount_bps: number | null;
  pricing_version: number;
  as_of: string;
  valid_until: string | null;
}

export async function currentPrices(): Promise<PriceQuoteList> {
  return (await call<PriceQuoteList>("GET", "/prices")).data;
}

export const dimensionLabels: Record<Dimension, string> = {
  input: "输入", output: "输出", cache_read: "缓存读", cache_write: "缓存写",
  cache_write_1h: "1 小时缓存写", reasoning: "推理", audio_input: "音频输入",
  audio_output: "音频输出", image_input: "图像输入", image_output: "图像输出", web_search: "联网搜索",
};

// 组织优惠只格式化为百分比；接口给出的客户价已经计入优惠。
export function discountPercent(bps: number): string {
  const value = BigInt(bps);
  const fraction = (value % 100n).toString().padStart(2, "0").replace(/0+$/, "");
  return `${value / 100n}${fraction ? `.${fraction}` : ""}%`;
}
