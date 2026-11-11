//! 当前客户报价：仅包含已计入组织优惠的精确 CNY 单价。

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::engine::{discount_at, model_pricing};
use crate::{
    cny_per_million, CnyPerUsd, Dimension, Payer, PriceBook, PricingError, Rates, ServiceTier,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteUnit {
    Token,
    Request,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PriceRate {
    pub dimension: Dimension,
    pub unit: QuoteUnit,
    pub price_cny_per_million: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_from: Option<Dimension>,
}

impl PriceRate {
    /// 旧 DP 合同按单 token 报价，只移动小数点，不重新计算或舍入。
    pub fn per_token(&self) -> Option<String> {
        let text = self.price_cny_per_million.as_ref()?;
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = format!(
            "{}{}{}",
            "0".repeat(6_usize.saturating_sub(whole.len())),
            whole,
            fraction
        );
        let point = whole.len().saturating_sub(6);
        let integer = digits[..point].trim_start_matches('0');
        let fraction = digits[point..].trim_end_matches('0');
        let integer = if integer.is_empty() { "0" } else { integer };
        Some(if fraction.is_empty() {
            integer.to_owned()
        } else {
            format!("{integer}.{fraction}")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LongContextQuote {
    pub above_input_tokens: u64,
    pub rates: Vec<PriceRate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierQuote {
    pub base: Vec<PriceRate>,
    pub long_context: Vec<LongContextQuote>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerQuote {
    pub discount_bps: u16,
    pub prices_by_tier: BTreeMap<ServiceTier, TierQuote>,
}

fn rates_quote(rates: &Rates, cny: CnyPerUsd, discount_bps: u16) -> Vec<PriceRate> {
    Dimension::ALL
        .into_iter()
        .map(|dimension| {
            let resolved = rates.rate_with_source(dimension);
            PriceRate {
                dimension,
                unit: if dimension == Dimension::WebSearch {
                    QuoteUnit::Request
                } else {
                    QuoteUnit::Token
                },
                price_cny_per_million: resolved
                    .and_then(|(rate, _)| cny_per_million(rate, cny, discount_bps)),
                fallback_from: resolved.and_then(|(_, source)| source),
            }
        })
        .collect()
}

impl PriceBook {
    /// 与真实收费相同的组织优惠；非法当前修订从不回退为原价。
    pub fn customer_discount_bps(
        &self,
        tenant_id: &str,
        at: DateTime<Utc>,
    ) -> Result<u16, PricingError> {
        discount_at(self, tenant_id, at).map(|discount| discount.map_or(0, |d| d.bps))
    }

    /// 按调用者请求的平台入口模型报价，服务账号不参与客户价。
    pub fn customer_quote(
        &self,
        model_id: &str,
        payer: &Payer,
        at: DateTime<Utc>,
    ) -> Result<CustomerQuote, PricingError> {
        let (baseline, rate) = model_pricing(self, model_id, payer, at)?;
        let discount_bps = self.customer_discount_bps(&payer.tenant_id, at)?;
        let prices_by_tier = baseline
            .card
            .0
            .iter()
            .map(|(tier, rates)| {
                (
                    tier.clone(),
                    TierQuote {
                        base: rates_quote(&rates.base, rate.cny, discount_bps),
                        long_context: rates
                            .long_context
                            .iter()
                            .map(|band| LongContextQuote {
                                above_input_tokens: band.above_input_tokens,
                                rates: rates_quote(&band.rates, rate.cny, discount_bps),
                            })
                            .collect(),
                    },
                )
            })
            .collect();
        Ok(CustomerQuote {
            discount_bps,
            prices_by_tier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        price_event, BaselineRow, BillableUsage, DiscountRow, GroupRateRow, PriceRows,
        ENGINE_VERSION,
    };
    use serde_json::json;

    fn book() -> (PriceBook, Payer, DateTime<Utc>) {
        let at = DateTime::UNIX_EPOCH;
        let book = PriceBook::from_rows(PriceRows {
            baselines: vec![BaselineRow {
                revision_id: 1,
                engine_version: ENGINE_VERSION,
                model_id: "m".to_owned(),
                effective_from: at,
                card: json!({"default":{"base":{"input":"0.15","output":"0"},
                    "long_context":[{"above_input_tokens":200000,"rates":{"input":"0.3","output":"0"}}]},
                    "batch":{"base":{"input":"0.075","output":"0"},
                    "long_context":[{"above_input_tokens":200000,"rates":{"input":"0.15","output":"0"}}]}}),
            }],
            rates: vec![GroupRateRow {
                revision_id: 2,
                engine_version: ENGINE_VERSION,
                group_id: "g".to_owned(),
                model_id: None,
                effective_from: at,
                cny_per_usd_e4: Some(72000),
            }],
            cost_rates: Vec::new(),
            discounts: vec![DiscountRow {
                revision_id: 3,
                engine_version: ENGINE_VERSION,
                tenant_id: "t".to_owned(),
                effective_from: at,
                discount_bps: 1000,
            }],
        });
        (
            book,
            Payer {
                tenant_id: "t".to_owned(),
                group_id: Some("g".to_owned()),
            },
            at,
        )
    }

    #[test]
    fn quoting_never_rounds_a_rate_before_an_event_is_charged() {
        let (book, payer, at) = book();
        let quote = book.customer_quote("m", &payer, at).expect("quote");
        let base = &quote.prices_by_tier[&ServiceTier::default_tier()].base;
        let input = base
            .iter()
            .find(|r| r.dimension == Dimension::Input)
            .expect("input");
        assert_eq!(input.price_cny_per_million.as_deref(), Some("0.972"));
        assert_eq!(input.per_token().as_deref(), Some("0.000000972"));
        let usage = BillableUsage {
            event_id: "one".to_owned(),
            requested_model_id: "m".to_owned(),
            occurred_at: at,
            prompt_tokens: 1,
            ..BillableUsage::default()
        };
        let charged = price_event(&book, &usage, &payer, at).expect("charge");
        assert_eq!(charged.charge.0, 1);
        assert_eq!(charged.discount.map(|d| d.bps), Some(1000));
    }

    #[test]
    fn whole_prompt_uses_the_base_at_the_boundary_and_the_next_band_above_it() {
        let (book, payer, at) = book();
        let quote = book.customer_quote("m", &payer, at).expect("quote");
        let tier = &quote.prices_by_tier[&ServiceTier::default_tier()];
        let long = tier.long_context[0]
            .rates
            .iter()
            .find(|r| r.dimension == Dimension::Input)
            .expect("input");
        assert_eq!(long.price_cny_per_million.as_deref(), Some("1.944"));
        for (tokens, band, expected) in [(200000, None, 194400), (200001, Some(200000), 388802)] {
            let usage = BillableUsage {
                event_id: "boundary".to_owned(),
                requested_model_id: "m".to_owned(),
                occurred_at: at,
                prompt_tokens: tokens,
                ..BillableUsage::default()
            };
            let charged = price_event(&book, &usage, &payer, at).expect("charge");
            assert_eq!(charged.long_context_above, band);
            assert_eq!(charged.charge.0, expected);
        }
    }

    #[test]
    fn per_token_preserves_zero_and_the_full_twenty_decimal_precision() {
        for (million, token) in [
            ("0", "0"),
            ("0.00000000000001", "0.00000000000000000001"),
            ("100000000", "100"),
            ("1234567.1234", "1.2345671234"),
        ] {
            let rate = PriceRate {
                dimension: Dimension::Input,
                unit: QuoteUnit::Token,
                price_cny_per_million: Some(million.to_owned()),
                fallback_from: None,
            };
            assert_eq!(rate.per_token().as_deref(), Some(token));
        }
    }
}
