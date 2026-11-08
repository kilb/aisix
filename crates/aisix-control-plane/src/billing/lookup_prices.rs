//! `GET /dp/v1/keys/{key_id}/prices`: a gateway asks what a key's tenant pays
//! per token, so it can show its callers the prices they are charged at.
//!
//! The tenant comes from the control plane's own key record, never from the
//! caller. Any gateway credential of the environment may ask about any key.
//! The answer lists the tenant's granted models that have a price in force:
//! the tenant's override card when one is current, otherwise the platform
//! card less the tenant's current discount (an override is not discounted).
//! Only the `default` tier's base rates are shown, in USD per token as exact
//! decimal strings. It carries no channel information.
//!
//! This is a lookup on its own route: it never touches model traffic.

use std::collections::HashMap;

use aisix_pricing::{Rate, RateCard, ServiceTier, ENGINE_VERSION};
use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::access::key_access;
use super::plans::{current_discount, current_revisions};
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::ApiError;
use crate::pricing::store::{pricing_version, SellRevision};
use crate::resources::Kind;
use crate::AppState;

/// Rates are micro-USD per million units, which is pico-USD per unit; basis
/// points add 10^4 more.
const PER_TOKEN_DENOMINATOR: u128 = 10_000_000_000_000_000;

/// `rate × kept_bps / 10 000` pico-USD per unit, as the exact decimal in USD
/// per unit: no trailing zeros, no `.` for a whole amount.
pub fn per_token_usd(rate: Rate, kept_bps: u32) -> String {
    let n = u128::from(rate.0) * u128::from(kept_bps);
    let (whole, frac) = (n / PER_TOKEN_DENOMINATOR, n % PER_TOKEN_DENOMINATOR);
    if frac == 0 {
        return whole.to_string();
    }
    let frac = format!("{frac:016}");
    format!("{whole}.{}", frac.trim_end_matches('0'))
}

/// One model's price for one tenant, in USD per token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelPrice {
    pub model_id: Uuid,
    pub input: String,
    pub output: String,
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
    /// The earliest instant a price shown here (or the tenant's discount)
    /// changes; `null` when nothing is scheduled.
    pub valid_until: Option<DateTime<Utc>>,
    pub models: Vec<ModelPrice>,
}

/// A stored `engine_version` as the price book reads it: one no `u32` holds
/// reads as the newest.
fn engine_version(stored: i64) -> u32 {
    u32::try_from(stored).unwrap_or(u32::MAX)
}

/// What one sell revision in force says, as the price book reads it.
enum Sold {
    /// A usable card.
    Card(RateCard),
    /// An override that ended: the platform price applies.
    Ended,
    /// Unreadable (a newer engine, or a card that does not decode): the
    /// engine prices nothing from it and never gives way to another price.
    Invalid,
}

fn read_sell(revision: &SellRevision) -> Sold {
    if engine_version(revision.engine_version) > ENGINE_VERSION {
        return Sold::Invalid;
    }
    match &revision.card {
        None if revision.tenant_id.is_some() => Sold::Ended,
        None => Sold::Invalid,
        Some(card) => match serde_json::from_value(card.clone()) {
            Ok(card) => Sold::Card(card),
            Err(_) => Sold::Invalid,
        },
    }
}

/// The `default` tier's base rates of `card` at `kept_bps`.
fn model_price(model_id: Uuid, card: &RateCard, kept_bps: u32) -> Option<ModelPrice> {
    let base = &card.0.get(&ServiceTier::default_tier())?.base;
    let show = |r: Rate| per_token_usd(r, kept_bps);
    Some(ModelPrice {
        model_id,
        input: show(base.input),
        output: show(base.output),
        input_cache_read: base.cache_read.map(show),
        input_cache_write: base.cache_write.map(show),
        input_cache_write_1h: base.cache_write_1h.map(show),
    })
}

/// The prices of `key_id`'s tenant; `None` for an unknown or revoked key.
pub async fn tenant_prices(
    conn: &mut PgConnection,
    key_id: Uuid,
) -> Result<Option<TenantPrices>, DbError> {
    let Some(access) = key_access(&mut *conn, key_id).await? else {
        return Ok(None);
    };
    let tenant_id = access.tenant_id;
    let granted: Vec<Uuid> =
        sqlx::query_scalar("SELECT allowed_model_ids FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(&mut *conn)
            .await?;
    // Read first. Price writes bump it in their own transaction, and this
    // transaction does not see one snapshot, so a write racing these reads
    // can only leave the version older than the prices, never newer: a
    // gateway that keys its cache on it then refetches, and never keeps
    // stale prices under a fresh version.
    let pricing_version = pricing_version(&mut *conn).await?.unwrap_or(0);
    // Granted models that still exist.
    let granted: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gw_resources WHERE kind = $1 AND NOT deleted AND id = ANY($2) ORDER BY id",
    )
    .bind(Kind::Models.segment())
    .bind(&granted)
    .fetch_all(&mut *conn)
    .await?;

    // The discount in force, as the engine reads it: `None` when the
    // tenant has none, `Err` when it is unreadable, which leaves every
    // platform-priced model unpriced. An override is never discounted.
    let discount: Result<u32, ()> = match current_discount(&mut *conn, tenant_id).await? {
        None => Ok(10_000),
        Some(d) if engine_version(d.engine_version) > ENGINE_VERSION => Err(()),
        Some(d) => u32::try_from(d.discount_bps)
            .ok()
            .and_then(|bps| 10_000u32.checked_sub(bps))
            .ok_or(()),
    };
    let overrides: HashMap<Uuid, SellRevision> =
        current_revisions(&mut *conn, &granted, Some(tenant_id))
            .await?
            .into_iter()
            .map(|r| (r.model_id, r))
            .collect();
    let platform: HashMap<Uuid, SellRevision> = current_revisions(&mut *conn, &granted, None)
        .await?
        .into_iter()
        .map(|r| (r.model_id, r))
        .collect();
    let models = granted
        .iter()
        .filter_map(|id| {
            // The tenant's override in force wins, undiscounted; one that
            // ended gives way to the platform card less the discount; an
            // unreadable revision, either one, leaves the model unpriced.
            match overrides.get(id).map(read_sell) {
                Some(Sold::Card(card)) => return model_price(*id, &card, 10_000),
                Some(Sold::Invalid) => return None,
                Some(Sold::Ended) | None => {}
            }
            match read_sell(platform.get(id)?) {
                Sold::Card(card) => model_price(*id, &card, discount.ok()?),
                Sold::Ended | Sold::Invalid => None,
            }
        })
        .collect();

    let valid_until: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT min(effective_from) FROM ( \
           SELECT effective_from FROM pricing_sell_revisions \
            WHERE model_id = ANY($1) AND (tenant_id IS NULL OR tenant_id = $2) \
              AND effective_from > now() \
           UNION ALL \
           SELECT effective_from FROM pricing_discount_revisions \
            WHERE tenant_id = $2 AND effective_from > now() \
         ) s",
    )
    .bind(&granted)
    .bind(tenant_id)
    .fetch_one(&mut *conn)
    .await?;

    Ok(Some(TenantPrices {
        tenant_id,
        pricing_version,
        valid_until,
        models,
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
    // snapshot: see the order of the reads in `tenant_prices`).
    let found = state
        .db
        .tx(move |conn| async move { tenant_prices(conn, key_id).await }.boxed())
        .await?;
    found.map(Json).ok_or_else(ApiError::not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_token_prices_are_exact() {
        assert_eq!(per_token_usd(Rate::parse("6").unwrap(), 10_000), "0.000006");
        assert_eq!(per_token_usd(Rate::parse("6").unwrap(), 8_000), "0.0000048");
        assert_eq!(per_token_usd(Rate::parse("0").unwrap(), 10_000), "0");
        // The smallest unit: 1 micro-USD per million tokens at 1 bp.
        assert_eq!(per_token_usd(Rate(1), 1), "0.0000000000000001");
        // The cap is USD 1 per token.
        assert_eq!(per_token_usd(Rate::MAX, 10_000), "1");
        assert_eq!(per_token_usd(Rate::MAX, 9_999), "0.9999");
    }
}
