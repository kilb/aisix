//! The operators' pricing API, `/admin/pricing/*`: the built-in catalogue,
//! the history of the retired cost cards, the discount revisions, the
//! charges, and the pricing status that is the alert surface for what cannot
//! be priced. Cost rates have their own routes ([`super::cost_rates`]).
//!
//! Revisions are append-only: there is no `PUT`, `PATCH` or `DELETE`, and a
//! price changes only by a new `POST` starting now or later. Every write goes
//! through [`super::store`], which refuses a start more than five seconds in
//! the past and writes the revision, the `pricing_version` bump and the
//! audit row in one transaction. A write is one transaction however many
//! revisions it makes, so a catalogue import lands whole or not at all.
//!
//! Charges and costs name upstream channels; this API is for operators only
//! and is never projected to tenants.

use aisix_pricing::CatalogueEntry;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use super::store::{self, CostRevision, DiscountRevision, NewDiscount};
use crate::audit::Actor;
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::usage::{page, page_limit, query_rejected};
use crate::AppState;

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

// ---- the catalogue ----

/// One catalogue entry as the API shows it.
fn entry_view(e: &CatalogueEntry) -> Value {
    json!({
        "key": e.key,
        "aliases": e.aliases,
        "source_url": e.source_url,
        "as_of": e.as_of,
        "effective_from": e.effective_from,
        "notes": e.notes,
        "card": e.card,
    })
}

/// `GET /admin/pricing/catalogue` → `{version, items: [CatalogueEntry]}`:
/// the list prices built into this control plane, in file order, dated
/// entries included.
pub async fn catalogue(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let catalogue = state.prices.catalogue();
    let items: Vec<Value> = catalogue.entries().iter().map(entry_view).collect();
    Ok(Json(
        json!({ "version": catalogue.version(), "items": items }),
    ))
}

// ---- price warnings ----

/// Something an operator should know about a price they wrote, which does
/// not stop it being written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PriceWarning {
    pub code: &'static str,
    pub message: String,
    /// For `superseded_later`: the revision that takes over, and when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<DateTime<Utc>>,
}

// ---- the retired cost cards ----

/// Ends a revision query built on `SELECT … WHERE true`: newest first by
/// `revision_id`, one row past `limit`, which [`page`] tells another page
/// by.
fn newest_revisions_first(sql: &mut QueryBuilder<'_, Postgres>, before: Option<i64>, limit: i64) {
    if let Some(before) = before {
        sql.push(" AND revision_id < ").push_bind(before);
    }
    sql.push(" ORDER BY revision_id DESC LIMIT ")
        .push_bind(limit + 1);
}

#[derive(Deserialize)]
pub struct CostQuery {
    upstream_provider: Option<String>,
    upstream_model: Option<String>,
    provider_key_id: Option<Uuid>,
    limit: Option<i64>,
    /// Only revisions with a smaller `revision_id`: the `next_before` of
    /// the previous page.
    before: Option<i64>,
}

/// `GET /admin/pricing/cost` → `{items: [CostRevision], next_before}`: the
/// per-dimension cost cards written before cost became a rate, newest
/// first, optionally only those of an `upstream_provider`, an
/// `upstream_model` or a `provider_key_id`; paged as [`list_sell`] is. They
/// price nothing now and take no new rows: there is no write route.
pub async fn list_cost(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<CostQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT revision_id, engine_version, upstream_provider, upstream_model, provider_key_id, \
                effective_from, card, source, created_by, created_at \
         FROM pricing_cost_revisions WHERE true",
    );
    if let Some(provider) = q.upstream_provider {
        sql.push(" AND upstream_provider = ").push_bind(provider);
    }
    if let Some(model) = q.upstream_model {
        sql.push(" AND upstream_model = ").push_bind(model);
    }
    if let Some(key) = q.provider_key_id {
        sql.push(" AND provider_key_id = ").push_bind(key);
    }
    newest_revisions_first(&mut sql, q.before, limit);
    let rows: Vec<CostRevision> = sql
        .build_query_as()
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    let (items, next_before) = page(rows, limit, |r| r.revision_id);
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

// ---- discounts ----

#[derive(Deserialize)]
pub struct DiscountQuery {
    tenant_id: Option<Uuid>,
    limit: Option<i64>,
    /// Only revisions with a smaller `revision_id`: the `next_before` of
    /// the previous page.
    before: Option<i64>,
}

/// `GET /admin/pricing/discounts` → `{items: [DiscountRevision],
/// next_before}`: discount revisions, newest first, optionally only those of
/// a `tenant_id`; paged as [`list_sell`] is.
pub async fn list_discounts(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<DiscountQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT revision_id, engine_version, tenant_id, effective_from, discount_bps, created_by, \
                created_at \
         FROM pricing_discount_revisions WHERE true",
    );
    if let Some(tenant_id) = q.tenant_id {
        sql.push(" AND tenant_id = ").push_bind(tenant_id);
    }
    newest_revisions_first(&mut sql, q.before, limit);
    let rows: Vec<DiscountRevision> = sql
        .build_query_as()
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    let (items, next_before) = page(rows, limit, |r| r.revision_id);
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscountBody {
    tenant_id: Uuid,
    effective_from: DateTime<Utc>,
    /// Read wide, so any whole number out of range is refused at its path.
    discount_bps: i64,
}

/// `POST /admin/pricing/discounts` with `{tenant_id, effective_from,
/// discount_bps}` → `201 DiscountRevision`. Basis points outside 0 to 10000
/// are `422 /discount_bps`.
pub async fn create_discount(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<DiscountBody>,
) -> Result<(StatusCode, Json<DiscountRevision>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let discount_bps = u16::try_from(body.discount_bps)
        .ok()
        .filter(|bps| *bps <= 10_000)
        .ok_or_else(|| refused("/discount_bps", "折扣必须在 0 到 10000 个基点之间"))?;
    let row = NewDiscount {
        tenant_id: body.tenant_id,
        effective_from: body.effective_from,
        discount_bps,
    };
    let revision = state
        .db
        .tx(move |conn| {
            async move { store::insert_discount(conn, &row, Actor::Operator(&auth)).await }.boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(revision)))
}

// ---- charges ----

/// The statuses a charge can have.
const STATUSES: [&str; 4] = ["charged", "exempt", "nothing_to_bill", "unpriced"];

#[derive(Deserialize)]
pub struct ChargesQuery {
    event_id: Option<String>,
    status: Option<String>,
    tenant_id: Option<String>,
    api_key_id: Option<String>,
    limit: Option<i64>,
    /// Only charges with a smaller `seq`: the `next_before` of the previous
    /// page.
    before: Option<i64>,
}

/// One `pricing_charges` row, `lines` and `warnings` as stored JSON.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct ChargeRow {
    seq: i64,
    event_id: String,
    tenant_id: String,
    api_key_id: String,
    user_id: String,
    requested_model_id: String,
    occurred_at: DateTime<Utc>,
    price_instant: Option<DateTime<Utc>>,
    status: String,
    charge_micro_cny: Option<i64>,
    cost_micro_usd: Option<i64>,
    cost_source: Option<Value>,
    group_id: Option<Uuid>,
    baseline_revision_id: Option<i64>,
    rate_revision_id: Option<i64>,
    cny_per_usd_e4: Option<i64>,
    rate_is_model_override: bool,
    discount_revision_id: Option<i64>,
    discount_bps: i64,
    service_tier: Option<String>,
    requested_service_tier: Option<String>,
    long_context_above: Option<i64>,
    estimated: bool,
    exemption: Option<String>,
    error_code: Option<String>,
    lines: Value,
    warnings: Value,
    engine_version: i64,
    priced_at: DateTime<Utc>,
    /// What the row cost the platform at its account's cost rate; `None`
    /// when unknown.
    cost_micro_cny: Option<i64>,
}

/// `GET /admin/pricing/charges` → `{items: [Charge], next_before}`: the
/// charge of one `event_id` (zero or one item), or charges by `status`,
/// `tenant_id` and `api_key_id`, newest first by `seq`, paged as
/// [`crate::usage::list_events`] is. A charge's tenant and member are the
/// ones its key belongs to, never what its usage row claims.
///
/// Each charge carries `cost_micro_cny`, what its row cost the platform at
/// the serving account's cost rate: `null` when the cost is unknown, never
/// 0. `cost_micro_usd` is a row priced before cost became a rate.
pub async fn list_charges(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<ChargesQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    if let Some(status) = q.status.as_deref() {
        if !STATUSES.contains(&status) {
            return Err(refused("status", "未知的计费状态"));
        }
    }
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT seq, event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, \
                price_instant, status, charge_micro_cny, cost_micro_usd, cost_source, \
                group_id, baseline_revision_id, rate_revision_id, cny_per_usd_e4, \
                rate_is_model_override, discount_revision_id, discount_bps, \
                service_tier, requested_service_tier, long_context_above, estimated, exemption, \
                error_code, lines, warnings, engine_version, priced_at, cost_micro_cny \
         FROM pricing_charges_cny WHERE true",
    );
    if let Some(before) = q.before {
        sql.push(" AND seq < ").push_bind(before);
    }
    if let Some(event_id) = q.event_id {
        sql.push(" AND event_id = ").push_bind(event_id);
    }
    if let Some(status) = q.status {
        sql.push(" AND status = ").push_bind(status);
    }
    if let Some(tenant_id) = q.tenant_id {
        sql.push(" AND tenant_id = ").push_bind(tenant_id);
    }
    if let Some(api_key_id) = q.api_key_id {
        sql.push(" AND api_key_id = ").push_bind(api_key_id);
    }
    sql.push(" ORDER BY seq DESC LIMIT ").push_bind(limit + 1);
    let rows: Vec<ChargeRow> = sql
        .build_query_as()
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    let (items, next_before) = page(rows, limit, |r| r.seq);
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

// ---- status ----

/// `GET /admin/pricing/status` → `{pricing_version, invalid_revisions:
/// [{table, revision_id, reason}], unpriced_last_24h: {total, by_code}}`:
/// the alert surface. `invalid_revisions` are the revisions the current
/// price book cannot use (`table` is `baseline`, `rate`, `cost` or
/// `discount`); their
/// rows are stored unpriced. `by_code` counts the charges stored unpriced in
/// the last 24 hours (by when they were priced) under each reason, the
/// control plane's own (`unknown_api_key`, `tenant_mismatch`,
/// `undecodable_usage`) as well as the engine's. `pricing_version` is null
/// when the stored version is missing or malformed.
pub async fn status(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let prices = state.prices.clone();
    let (version, invalid, by_code) = state
        .db
        .tx(move |conn| {
            async move {
                let version = store::pricing_version(&mut *conn).await?;
                let book = prices.current(&mut *conn).await?;
                let invalid: Vec<Value> = book
                    .invalid_revisions()
                    .iter()
                    .map(|r| {
                        json!({
                            "table": r.table,
                            "revision_id": r.revision_id,
                            "reason": r.reason,
                        })
                    })
                    .collect();
                let by_code = store::unpriced_by_code(&mut *conn).await?;
                Ok::<_, DbError>((version, invalid, by_code))
            }
            .boxed()
        })
        .await?;
    let total: i64 = by_code.iter().map(|(_, n)| n).sum();
    let by_code: Map<String, Value> = by_code
        .into_iter()
        .map(|(code, n)| (code, json!(n)))
        .collect();
    Ok(Json(json!({
        "pricing_version": version,
        "invalid_revisions": invalid,
        "unpriced_last_24h": {"total": total, "by_code": by_code},
    })))
}

#[cfg(test)]
mod tests {
    use aisix_pricing::{Catalogue, CatalogueEntry};

    use super::*;

    /// A catalogue with an off-peak price carrying an alias.
    fn catalogue() -> Catalogue {
        let entries = r#"{"key":"openai/offpeak","source_url":"https://openai.com/api/pricing","as_of":"2026-11-01","aliases":["offpeak-alias"],"notes":"Standard (off-peak) price; peak windows bill double.","card":{"default":{"base":{"input":"1","output":"2"}}}}"#;
        Catalogue::from_json(&format!(
            r#"{{"version":"2026-11-06","entries":[{entries}]}}"#
        ))
        .expect("the test catalogue loads")
    }

    #[test]
    fn an_entry_view_names_its_key_and_its_card() {
        let c = catalogue();
        let entry: &CatalogueEntry = c.entries().first().expect("one entry");
        let view = entry_view(entry);
        assert_eq!(view["key"], "openai/offpeak");
        assert_eq!(view["card"]["default"]["base"]["input"], "1");
        assert_eq!(view["source_url"], "https://openai.com/api/pricing");
    }
}
