import { perThousandOfPerMillion } from "./money.ts";

/**
 * How a price card's rates read in the Dashboard. A card stores every rate
 * in USD per million units (`aisix_pricing::Rate`). A token is one unit; so
 * is one web search (`web_search`, billed per search the provider reports),
 * which providers price per thousand: a card's `"web_search": "10000"` is
 * $10 per 1,000 searches, $0.01 a search. A rate per call is therefore shown
 * and typed per thousand, and every other rate per million tokens.
 */

/** The dimensions a card meters per call rather than per token. */
const PER_CALL: ReadonlySet<string> = new Set(["web_search"]);

/** Whether `dimension`'s rate is shown and typed per thousand calls. */
export function isPerCall(dimension: string): boolean {
  return PER_CALL.has(dimension);
}

/** A card's rate, `perMillion` as the card stores it, as a page shows it: `$2.5 / 百万 tokens`, a search's `$10 / 千次`. */
export function usdRateText(dimension: string, perMillion: string): string {
  return isPerCall(dimension) ? `$${perThousandOfPerMillion(perMillion)} / 千次` : `$${perMillion} / 百万 tokens`;
}

/**
 * The same, in a line of several rates under a heading that already says
 * 美元/百万 tokens: a token rate as its amount alone (`$2.5`), a rate per call
 * with its own unit (`$10 / 千次`).
 */
export function compactRateText(dimension: string, perMillion: string): string {
  return isPerCall(dimension) ? usdRateText(dimension, perMillion) : `$${perMillion}`;
}

/**
 * What each tier of a card bills, where tiers are listed. A batch row is
 * the usage of a finished batch job (`/v1/batches`), and only such a row is
 * priced at `batch`; on a card without one it is unpriced
 * (`missing_tier_price`), never billed at `default`. Any other row is priced
 * at the tier its upstream reports it served, with no tier, `standard` and
 * `auto` read as `default` (`ServiceTier::normalize`).
 */
export const TIER_HINT =
  "标准档用于普通调用；批量档只用于批处理任务（Batch API）的用量，普通调用不会按它计费，卡上没有批量档时批处理任务的用量记为未计价，不会按标准档计费；flex、priority、fast 等档位只在上游报告以该档位服务时使用，卡上没有该档位时这样的调用记为未计价。";
