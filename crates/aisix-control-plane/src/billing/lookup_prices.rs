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

/// The `default` tier's base rates of `revision`, at `kept_bps`. `None` when
/// the revision ends an override, is of a newer engine than this one, or
/// holds no usable `default` tier.
fn model_price(revision: &SellRevision, kept_bps: u32) -> Option<ModelPrice> {
    if revision.engine_version > i64::from(ENGINE_VERSION) {
        return None;
    }
    let card: RateCard = serde_json::from_value(revision.card.clone()?).ok()?;
    let base = &card.0.get(&ServiceTier::default_tier())?.base;
    let show = |r: Rate| per_token_usd(r, kept_bps);
    Some(ModelPrice {
        model_id: revision.model_id,
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
    let tenant: Option<(Uuid, Vec<Uuid>)> = sqlx::query_as(
        "SELECT t.id, t.allowed_model_ids FROM tenant_keys k JOIN tenants t ON t.id = k.tenant_id \
         WHERE k.id = $1 AND k.revoked_at IS NULL",
    )
    .bind(key_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((tenant_id, granted)) = tenant else {
        return Ok(None);
    };
    // Granted models that still exist.
    let granted: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM gw_resources WHERE kind = $1 AND NOT deleted AND id = ANY($2) ORDER BY id",
    )
    .bind(Kind::Models.segment())
    .bind(&granted)
    .fetch_all(&mut *conn)
    .await?;

    let kept_bps = match current_discount(&mut *conn, tenant_id).await? {
        Some(d) => 10_000u32.saturating_sub(u32::try_from(d.discount_bps).unwrap_or(10_000)),
        None => 10_000,
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
            // An override with a usable card wins, undiscounted; one that
            // ended it (no card) leaves the platform price.
            overrides
                .get(id)
                .and_then(|r| model_price(r, 10_000))
                .or_else(|| platform.get(id).and_then(|r| model_price(r, kept_bps)))
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
        pricing_version: pricing_version(conn).await?.unwrap_or(0),
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
    // One transaction, so every query shares one `now()`.
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
