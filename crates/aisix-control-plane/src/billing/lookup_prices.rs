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

use std::collections::HashMap;

use aisix_pricing::{cny_per_token, CnyPerUsd, RateCard, ServiceTier, ENGINE_VERSION};
use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::access::key_access;
use super::money::CURRENCY;
use super::plans::current_discount;
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::ApiError;
use crate::pricing::baselines::current_baselines;
use crate::pricing::rates::rate_in_force;
use crate::pricing::store::pricing_version;
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

/// One granted model with everything needed to price it, as the engine reads
/// it: its baseline card, the group's rate and the tenant's discount.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedModel {
    pub model_id: Uuid,
    pub name: String,
    /// The baseline's `default` tier base rates.
    pub base: aisix_pricing::Rates,
    pub cny: CnyPerUsd,
    pub discount_bps: u16,
}

/// What every granted model a key's group can price costs, now.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedModels {
    pub pricing_version: i64,
    /// The earliest instant one of these prices changes: a baseline of a
    /// granted model, a rate in the group, or the tenant's discount.
    pub valid_until: Option<DateTime<Utc>>,
    pub models: Vec<PricedModel>,
}

/// A stored `engine_version` as the price book reads it: one no `u32` holds
/// reads as the newest.
fn engine_version(stored: i64) -> u32 {
    u32::try_from(stored).unwrap_or(u32::MAX)
}

/// The `default` tier's base rates of `card`, `None` when the card carries no
/// default tier.
fn base_rates(card: &RateCard) -> Option<aisix_pricing::Rates> {
    card.0
        .get(&ServiceTier::default_tier())
        .map(|t| t.base.clone())
}

/// Every granted model of `tenant_id` that has a baseline and a rate for
/// `group_id` in force, at their prices now.
///
/// `None` for a key that does not resolve, which the route answers `404`. A
/// tenant with no group prices nothing: a grouped key is the only way a
/// customer has one, and its absence is a closed key rather than a guess at a
/// rate.
pub async fn priced_models(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    group_id: Option<Uuid>,
) -> Result<PricedModels, DbError> {
    // Read first. Price writes bump it in their own transaction, and this
    // transaction does not see one snapshot, so a write racing these reads
    // can only leave the version older than the prices, never newer: a
    // gateway that keys its cache on it then refetches, and never keeps
    // stale prices under a fresh version.
    let pricing_version = pricing_version(&mut *conn).await?.unwrap_or(0);
    let granted: Vec<Uuid> =
        sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&mut *conn)
            .await?;
    // Granted models that still exist, with the name they are listed under.
    let granted: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, COALESCE(doc->>'display_name', '') FROM gw_resources \
         WHERE kind = $1 AND NOT deleted AND id = ANY($2) ORDER BY id",
    )
    .bind(Kind::Models.segment())
    .bind(&granted)
    .fetch_all(&mut *conn)
    .await?;
    let Some(group_id) = group_id else {
        return Ok(PricedModels {
            pricing_version,
            valid_until: None,
            models: Vec::new(),
        });
    };
    let ids: Vec<Uuid> = granted.iter().map(|(id, _)| *id).collect();
    let baselines: HashMap<Uuid, RateCard> = current_baselines(&mut *conn, &ids)
        .await?
        .into_iter()
        .filter(|b| engine_version(b.engine_version) <= ENGINE_VERSION)
        .filter_map(|b| {
            let card = serde_json::from_value::<RateCard>(b.card).ok()?;
            Some((b.model_id, card))
        })
        .collect();
    let discount: Option<u16> = match current_discount(&mut *conn, tenant_id).await? {
        None => Some(0),
        // A discount a newer engine wrote cannot be read, so no final price
        // exists: every model of the tenant is absent rather than shown
        // undiscounted.
        Some(d) if engine_version(d.engine_version) > ENGINE_VERSION => None,
        Some(d) => u16::try_from(d.discount_bps)
            .ok()
            .filter(|bps| *bps <= 10_000)
            .map(Some)
            .unwrap_or(None),
    };
    let Some(discount) = discount else {
        return Ok(PricedModels {
            pricing_version,
            valid_until: None,
            models: Vec::new(),
        });
    };
    let mut models = Vec::new();
    for (model_id, name) in granted {
        let Some(base) = baselines.get(&model_id).and_then(base_rates) else {
            continue;
        };
        let Some((rate, _, _)) = rate_in_force(&mut *conn, group_id, Some(model_id)).await? else {
            continue;
        };
        models.push(PricedModel {
            model_id,
            name,
            base,
            cny: rate,
            discount_bps: discount,
        });
    }
    Ok(PricedModels {
        pricing_version,
        valid_until: valid_until(&mut *conn, &ids, tenant_id, group_id).await?,
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
) -> Result<Option<DateTime<Utc>>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT min(effective_from) FROM ( \
           SELECT effective_from FROM pricing_baselines \
            WHERE model_id = ANY($1) AND effective_from > now() \
           UNION ALL \
           SELECT effective_from FROM pricing_group_rates \
            WHERE group_id = $2 AND model_id = ANY($1) AND effective_from > now() \
           UNION ALL \
           SELECT effective_from FROM pricing_group_rates \
            WHERE group_id = $2 AND model_id IS NULL AND effective_from > now() \
           UNION ALL \
           SELECT effective_from FROM pricing_discount_revisions \
            WHERE tenant_id = $3 AND effective_from > now() \
         ) s",
    )
    .bind(models)
    .bind(group_id)
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// One model's price for the gateway's answer: each rate in CNY per token at
/// `cny`, less `discount_bps`.
fn model_price(model: &PricedModel) -> ModelPrice {
    let show = |r: aisix_pricing::Rate| cny_per_token(r, model.cny, model.discount_bps);
    ModelPrice {
        model_id: model.model_id,
        input: show(model.base.input),
        output: show(model.base.output),
        input_cache_read: model.base.cache_read.and_then(show),
        input_cache_write: model.base.cache_write.and_then(show),
        input_cache_write_1h: model.base.cache_write_1h.and_then(show),
    }
}

/// The prices of `key_id`'s tenant; `None` for an unknown or revoked key.
pub async fn tenant_prices(
    conn: &mut PgConnection,
    key_id: Uuid,
) -> Result<Option<TenantPrices>, DbError> {
    let Some(access) = key_access(&mut *conn, key_id).await? else {
        return Ok(None);
    };
    let priced = priced_models(&mut *conn, access.tenant_id, access.group_id).await?;
    Ok(Some(TenantPrices {
        tenant_id: access.tenant_id,
        pricing_version: priced.pricing_version,
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
    let found = state
        .lookups
        .tx(move |conn| async move { tenant_prices(conn, key_id).await }.boxed())
        .await?;
    found.map(Json).ok_or_else(ApiError::not_found)
}

#[cfg(test)]
mod tests {
    use aisix_pricing::Rate;

    use super::*;

    fn priced(base: aisix_pricing::Rates, cny: CnyPerUsd, discount_bps: u16) -> PricedModel {
        PricedModel {
            model_id: Uuid::nil(),
            name: "m".to_string(),
            base,
            cny,
            discount_bps,
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
