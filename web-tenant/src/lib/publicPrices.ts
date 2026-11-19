import type { TierQuote } from "./priceQuote";

export interface PublicPricedModel {
  id: string;
  name: string;
  input_per_million: string | null;
  output_per_million: string | null;
  cache_read_per_million: string | null;
  prices_by_tier: Record<string, TierQuote> | null;
}

export interface PublicPriceList {
  currency: "CNY";
  as_of: string;
  available: boolean;
  models: PublicPricedModel[];
}

const PUBLIC_PRICES_URL = `${import.meta.env.BASE_URL.replace(/\/$/, "")}/api/public/prices`;

export async function loadPublicPrices(signal: AbortSignal): Promise<PublicPriceList> {
  const response = await fetch(PUBLIC_PRICES_URL, { credentials: "omit", signal });
  if (!response.ok) throw new Error("模型价格暂时无法加载");
  const quote = (await response.json()) as PublicPriceList;
  if (quote?.currency !== "CNY" || !Array.isArray(quote.models) || typeof quote.available !== "boolean") {
    throw new Error("模型价格暂时无法加载");
  }
  return quote;
}
