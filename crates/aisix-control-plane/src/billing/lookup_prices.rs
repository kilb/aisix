//! `GET /dp/v1/keys/{key_id}/prices`: a gateway asks what a key's tenant pays
//! per token, so it can show its callers the prices they are charged at.
//!
//! The tenant and the group come from the control plane's own key record,
//! never from the caller. Any gateway credential of the environment may ask
//! about any key.
//!
//! A model's price is its baseline in force — the official USD list price,
//! a full rate card — times the rate in force for the key's group and that
//! model (its own override, else the group's default), less the tenant's
//! current discount. Only the `default` tier's base rates are shown, in CNY
//! per token as exact decimal strings (R12: a listing price is the final one,
//! discount applied). A model with no baseline, or with no rate the group can
//! price it at, is absent: the gateway then shows no price rather than a
//! wrong one.
//!
//! The answer names the currency its amounts are in, and every amount is a
//! neutral name (`input`, `output`, …) rather than one carrying a unit. A
//! gateway requires the currency, and an answer without it lists no price at
//! all. The list of models is `items`, deliberately not `models`; the
//! doc comment on [`TenantPrices`] says why. The answer carries no channel
//! information.
//!
//! This is a lookup on its own route: it never touches model traffic.

use std::sync::Arc;

use aisix_pricing::{CustomerQuote, Dimension, Payer, PriceBook, PriceRate, ServiceTier};
use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::access::key_access;
use super::money::CURRENCY;
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::ApiError;
use crate::pricing::store::PriceBookCache;
use crate::resources::Kind;
use crate::AppState;

/// One model's price for one key's group, in CNY per token.
///
/// `input` and `output` are omitted only when the rate is unrepresentable
/// (a discount above 10000 bps), which `priced_models` already refuses to
/// build: a price this cannot read is left out rather than sent as `""`,
/// which a client decoding `Option<String>` would render as blank.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelPrice {
    pub model_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_read: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_write: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_write_1h: Option<String>,
}

/// What a key's tenant pays, now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TenantPrices {
    pub tenant_id: Uuid,
    pub pricing_version: i64,
    /// The currency every amount here is in. A gateway requires it: an
    /// answer without it lists no price rather than a figure of unknown
    /// unit.
    pub currency: &'static str,
    /// The earliest instant a price shown here (or the tenant's discount)
    /// changes; `null` when nothing is scheduled.
    pub valid_until: Option<DateTime<Utc>>,
    /// The granted models this key's group can price.
    ///
    /// `items`, not `models`: a gateway from before the currency field
    /// requires `models` to be an array of prices and has no `currency`, and
    /// the rename makes it fail to decode the answer. It then lists the
    /// models without prices, instead of showing CNY figures under no
    /// currency.
    pub items: Vec<ModelPrice>,
}

// 同一响应的全部分组共享一个与实际收费相同的版本化书本和数据库时刻。
pub(crate) struct QuoteContext {
    pub pricing_version: i64,
    pub as_of: DateTime<Utc>,
    pub book: Arc<PriceBook>,
}

impl QuoteContext {
    pub async fn load(conn: &mut PgConnection, cache: &PriceBookCache) -> Result<Self, DbError> {
        let as_of = sqlx::query_scalar("SELECT now()")
            .fetch_one(&mut *conn)
            .await?;
        let (pricing_version, book) = cache.current_with_version(conn).await?;
        Ok(Self {
            pricing_version,
            as_of,
            book,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PricedModel {
    pub model_id: Uuid,
    pub name: String,
    pub quote: CustomerQuote,
}

impl PricedModel {
    pub fn base(&self) -> &[PriceRate] {
        self.quote
            .prices_by_tier
            .get(&ServiceTier::default_tier())
            .map(|tier| tier.base.as_slice())
            .unwrap_or(&[])
    }
}

pub struct PricedModels {
    pub valid_until: Option<DateTime<Utc>>,
    pub models: Vec<PricedModel>,
}

/// 仅投影已授权且可公开调用的具体名称；内部账号模型和模板从不进入响应。
pub(crate) async fn priced_models(
    conn: &mut PgConnection,
    context: &QuoteContext,
    tenant_id: Uuid,
    group_id: Option<Uuid>,
) -> Result<PricedModels, DbError> {
    let granted = crate::tenants::scope_model_ids(&mut *conn, tenant_id).await?;
    let granted: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, doc->>'display_name' FROM gw_resources \
         WHERE kind = $1 AND NOT deleted AND id = ANY($2) \
         AND COALESCE(doc->>'internal', 'false') <> 'true' \
         AND COALESCE(doc->>'display_name', '') <> '' \
         AND position('*' in doc->>'display_name') = 0 ORDER BY id",
    )
    .bind(Kind::Models.segment())
    .bind(&granted)
    .fetch_all(&mut *conn)
    .await?;
    let Some(group_id) = group_id else {
        return Ok(PricedModels {
            valid_until: None,
            models: Vec::new(),
        });
    };
    let ids: Vec<Uuid> = granted.iter().map(|(id, _)| *id).collect();
    let payer = Payer {
        tenant_id: tenant_id.to_string(),
        group_id: Some(group_id.to_string()),
    };
    let mut models = Vec::new();
    for (model_id, name) in granted {
        if let Ok(quote) = context
            .book
            .customer_quote(&model_id.to_string(), &payer, context.as_of)
        {
            models.push(PricedModel {
                model_id,
                name,
                quote,
            });
        }
    }
    Ok(PricedModels {
        valid_until: valid_until(conn, &ids, tenant_id, group_id, context.as_of).await?,
        models,
    })
}

/// The earliest instant a price of one of `models` in `group_id`, or
/// `tenant_id`'s discount, changes: the least scheduled `effective_from`
/// among the three, or `None` when none is.
async fn valid_until(
    conn: &mut PgConnection,
    models: &[Uuid],
    tenant_id: Uuid,
    group_id: Uuid,
    as_of: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT min(effective_from) FROM ( \
           SELECT effective_from FROM pricing_baselines \
            WHERE model_id = ANY($1) AND effective_from > $4 \
           UNION ALL \
           SELECT effective_from FROM pricing_group_rates \
            WHERE group_id = $2 AND model_id = ANY($1) AND effective_from > $4 \
           UNION ALL \
           SELECT effective_from FROM pricing_group_rates \
            WHERE group_id = $2 AND model_id IS NULL AND effective_from > $4 \
           UNION ALL \
           SELECT effective_from FROM pricing_discount_revisions \
            WHERE tenant_id = $3 AND effective_from > $4 \
         ) s",
    )
    .bind(models)
    .bind(group_id)
    .bind(tenant_id)
    .bind(as_of)
    .fetch_one(&mut *conn)
    .await?)
}

/// One model's price for the gateway's answer: each rate in CNY per token at
/// `cny`, less `discount_bps`.
fn model_price(model: &PricedModel) -> ModelPrice {
    // 兼容字段只保留独立配置的维度；全部来自同一份最终 CNY 报价。
    let show = |dimension| {
        model
            .base()
            .iter()
            .find(|rate| rate.dimension == dimension && rate.fallback_from.is_none())
            .and_then(PriceRate::per_token)
    };
    ModelPrice {
        model_id: model.model_id,
        input: show(Dimension::Input),
        output: show(Dimension::Output),
        input_cache_read: show(Dimension::CacheRead),
        input_cache_write: show(Dimension::CacheWrite),
        input_cache_write_1h: show(Dimension::CacheWrite1h),
    }
}

/// The prices of `key_id`'s tenant; `None` for an unknown or revoked key.
pub async fn tenant_prices(
    conn: &mut PgConnection,
    key_id: Uuid,
    cache: &PriceBookCache,
) -> Result<Option<TenantPrices>, DbError> {
    let Some(access) = key_access(&mut *conn, key_id).await? else {
        return Ok(None);
    };
    let context = QuoteContext::load(&mut *conn, cache).await?;
    let priced = priced_models(&mut *conn, &context, access.tenant_id, access.group_id).await?;
    Ok(Some(TenantPrices {
        tenant_id: access.tenant_id,
        pricing_version: context.pricing_version,
        currency: CURRENCY,
        valid_until: priced.valid_until,
        items: priced.models.iter().map(model_price).collect(),
    }))
}

/// `GET /dp/v1/keys/{key_id}/prices`.
pub async fn prices(
    _dp: DpIdentity,
    State(state): State<AppState>,
    Path(key_id): Path<String>,
) -> Result<Json<TenantPrices>, ApiError> {
    let key_id = Uuid::parse_str(&key_id).map_err(|_| ApiError::not_found())?;
    // One transaction, so every query shares one `now()` (not one data
    // snapshot: see the order of the reads in `priced_models`).
    let cache = state.prices.clone();
    let found = state
        .lookups
        .tx(move |conn| async move { tenant_prices(conn, key_id, &cache).await }.boxed())
        .await?;
    found.map(Json).ok_or_else(ApiError::not_found)
}

#[cfg(test)]
mod tests {
    use aisix_pricing::{
        BaselineRow, CnyPerUsd, DiscountRow, GroupRateRow, PriceRows, Rate, ENGINE_VERSION,
    };

    use super::*;

    fn priced(base: aisix_pricing::Rates, cny: CnyPerUsd, discount_bps: u16) -> PricedModel {
        let at = DateTime::UNIX_EPOCH;
        let book = PriceBook::from_rows(PriceRows {
            baselines: vec![BaselineRow {
                revision_id: 1,
                engine_version: ENGINE_VERSION,
                model_id: "m".to_owned(),
                effective_from: at,
                card: serde_json::json!({"default": {"base": base}}),
            }],
            rates: vec![GroupRateRow {
                revision_id: 2,
                engine_version: ENGINE_VERSION,
                group_id: "g".to_owned(),
                model_id: None,
                effective_from: at,
                cny_per_usd_e4: Some(i64::from(cny.0)),
            }],
            cost_rates: Vec::new(),
            discounts: vec![DiscountRow {
                revision_id: 3,
                engine_version: ENGINE_VERSION,
                tenant_id: "t".to_owned(),
                effective_from: at,
                discount_bps: i64::from(discount_bps),
            }],
        });
        let payer = Payer {
            tenant_id: "t".to_owned(),
            group_id: Some("g".to_owned()),
        };
        PricedModel {
            model_id: Uuid::nil(),
            name: "m".to_owned(),
            quote: book.customer_quote("m", &payer, at).expect("a quote"),
        }
    }

    #[test]
    fn per_token_prices_are_exact() {
        let base = aisix_pricing::Rates {
            input: Rate::parse("6").unwrap(),
            output: Rate::parse("6").unwrap(),
            cache_read: Some(Rate::parse("6").unwrap()),
            cache_write: None,
            cache_write_1h: None,
            reasoning: None,
            audio_input: None,
            audio_output: None,
            image_input: None,
            image_output: None,
            web_search: None,
        };
        // Rate 1: the CNY figure is the baseline's own.
        let at_one = model_price(&priced(base.clone(), CnyPerUsd::ONE, 0));
        assert_eq!(at_one.input.as_deref(), Some("0.000006"));
        assert_eq!(at_one.input_cache_read.as_deref(), Some("0.000006"));
        assert_eq!(at_one.input_cache_write, None);
        // 6 CNY per USD, half off.
        let at_six = model_price(&priced(base, CnyPerUsd::parse("6").unwrap(), 5_000));
        assert_eq!(at_six.input.as_deref(), Some("0.000018"));
        assert_eq!(at_six.output.as_deref(), Some("0.000018"));
        // The smallest unit: 1 micro-USD per million tokens at rate 0.0001,
        // a two-thousandth of a fen per million.
        let tiny = priced(
            aisix_pricing::Rates {
                input: Rate(1),
                output: Rate(1),
                cache_read: None,
                cache_write: None,
                cache_write_1h: None,
                reasoning: None,
                audio_input: None,
                audio_output: None,
                image_input: None,
                image_output: None,
                web_search: None,
            },
            CnyPerUsd(1),
            9_999,
        );
        // 1 × 1 × 1 bp / 10²⁰: the smallest CNY per token a rate can express.
        assert_eq!(
            model_price(&tiny).input.as_deref(),
            Some("0.00000000000000000001")
        );
        // The cap is USD 1 per token, at rate 1.
        let capped = priced(
            aisix_pricing::Rates {
                input: Rate::MAX,
                output: Rate::MAX,
                cache_read: None,
                cache_write: None,
                cache_write_1h: None,
                reasoning: None,
                audio_input: None,
                audio_output: None,
                image_input: None,
                image_output: None,
                web_search: None,
            },
            CnyPerUsd::ONE,
            0,
        );
        assert_eq!(model_price(&capped).input.as_deref(), Some("1"));
    }
}
