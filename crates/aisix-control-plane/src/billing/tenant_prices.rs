//! `GET /api/prices`: what each of a tenant's assigned groups charges, in CNY
//! per million tokens, at the prices in force now (D16, R12).
//!
//! A model's price is its official USD baseline times the rate in force for
//! the group and that model (its own override, else the group's default),
//! less the tenant's current discount. A listing price is therefore the
//! final one: a tenant is shown what it is charged, never a list price it
//! must discount itself.
//!
//! The answer is built field by field from named columns, so no routing name,
//! provider, provider account or upstream model reaches it (the crate's
//! whitelist rule). A group a tenant is not assigned is not listed, and a
//! retired group is not either. A model the group can price at nothing — no
//! baseline, or no rate that covers it — is absent rather than shown at 0,
//! so nothing reads as free that is merely unpriced.

use axum::extract::State;
use axum::Json;
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::lookup_prices::{priced_models, PricedModel, QuoteContext};
use super::money::CURRENCY;
use crate::auth::MemberAuth;
use crate::error::ApiError;
use crate::groups::GroupRow;
use crate::http::EVERY_MEMBER;
use crate::tenants;
use crate::AppState;

/// The models `priced_models` returned for one group, in CNY per million
/// tokens, ordered by name then id (the input order already breaks ties by
/// id, and the sort is stable). A model whose rate cannot be rendered is left
/// out, never shown as 0.
fn models_of(models: &[PricedModel]) -> Vec<Value> {
    use aisix_pricing::Dimension;
    let mut rows: Vec<(&str, Value)> = Vec::new();
    for model in models {
        let explicit = |dimension| {
            model
                .base()
                .iter()
                .find(|rate| rate.dimension == dimension && rate.fallback_from.is_none())
                .and_then(|rate| rate.price_cny_per_million.as_ref())
        };
        let (Some(input), Some(output)) = (explicit(Dimension::Input), explicit(Dimension::Output))
        else {
            continue;
        };
        let mut row = json!({
            "model_id": model.model_id,
            "name": model.name,
            "input_per_million": input,
            "output_per_million": output,
            "prices_by_tier": model.quote.prices_by_tier,
        });
        for (field, dimension) in [
            ("cache_read_per_million", Dimension::CacheRead),
            ("cache_write_per_million", Dimension::CacheWrite),
            ("cache_write_1h_per_million", Dimension::CacheWrite1h),
        ] {
            if let Some(text) = explicit(dimension) {
                row[field] = json!(text);
            }
        }
        rows.push((model.name.as_str(), row));
    }
    rows.sort_by(|a, b| a.0.cmp(b.0));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// One group's entry: its display name and description, and its models.
fn group_view(group: &GroupRow, models: &[PricedModel]) -> Value {
    json!({
        "id": group.id,
        "display_name": group.display_name,
        "description": group.description,
        "models": models_of(models),
    })
}

/// The live groups `tenant_id` is assigned, by display name then id.
async fn assigned_groups(
    conn: &mut PgConnection,
    tenant_id: Uuid,
) -> Result<Vec<GroupRow>, ApiError> {
    let names = tenants::find(conn, tenant_id)
        .await?
        .allowed_provider_groups;
    crate::groups::groups_of(conn, &names)
        .await
        .map_err(ApiError::from)
}

/// `GET /api/prices` → `{currency: "CNY", groups: [{id, display_name,
/// description, models: [...]}]}`, one entry per live group the tenant is
/// assigned.
///
/// A model is priced through the same lookup the gateway's `/dp/v1` prices
/// answer comes from ([`priced_models`]), so a tenant and its gateway cannot
/// be shown two different prices for one call. The route reads on
/// `state.db`: it is a browser route, not a gateway lookup.
pub async fn prices(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    // The same list the route's `ROUTE_ROLES` row names, so a role added there
    // cannot leave this handler behind.
    auth.require(EVERY_MEMBER)?;
    let tenant_id = auth.tenant_id;
    let cache = state.prices.clone();
    let body = state
        .db
        .tx(move |conn| {
            async move {
                let context = QuoteContext::load(&mut *conn, &cache).await?;
                let discount_bps = context
                    .book
                    .customer_discount_bps(&tenant_id.to_string(), context.as_of)
                    .ok();
                let groups = assigned_groups(&mut *conn, tenant_id).await?;
                let mut views = Vec::with_capacity(groups.len());
                let mut valid_until = None;
                for group in &groups {
                    let priced =
                        priced_models(&mut *conn, &context, tenant_id, Some(group.id)).await?;
                    if let Some(until) = priced.valid_until {
                        valid_until = Some(
                            valid_until.map_or(until, |current: chrono::DateTime<chrono::Utc>| {
                                current.min(until)
                            }),
                        );
                    }
                    views.push(group_view(group, &priced.models));
                }
                Ok::<_, ApiError>(json!({ "currency": CURRENCY, "groups": views,
                    "discount_bps": discount_bps, "pricing_version": context.pricing_version,
                    "as_of": context.as_of, "valid_until": valid_until }))
            }
            .boxed()
        })
        .await?;
    Ok(Json(body))
}
