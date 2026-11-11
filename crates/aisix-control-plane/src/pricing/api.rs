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
use std::collections::HashMap;

use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::store::{self, CostRevision, DiscountRevision, NewDiscount};
use crate::audit::Actor;
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::Kind;
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
    /// Every status but this one.
    exclude_status: Option<String>,
    tenant_id: Option<String>,
    api_key_id: Option<String>,
    /// The charge's `requested_model_id`, matched exactly.
    model_id: Option<String>,
    /// The charge's `error_code`; `unknown` matches a charge stored with none.
    error_code: Option<String>,
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
    /// The sum of the charge's re-pricing adjustments; `None` when no run
    /// corrected it.
    adjusted_micro_cny: Option<i64>,
    /// The fields of the usage row read below, `NULL` without one.
    #[serde(skip)]
    ev: Option<Value>,
}

/// The fields of a charge's usage row an operator reads beside it: what
/// the caller sent, how the attempt ended, which upstream account and model
/// served it, and the counters the upstream reported.
const CHARGE_EVENT: &str = "jsonb_build_object(\
    'requested_model', ue.event->'requested_model', \
    'status_code', ue.event->'status_code', \
    'error_class', ue.event->'error_class', \
    'attempt_kind', ue.event->'attempt_kind', \
    'attempt_index', ue.event->'attempt_index', \
    'is_final_attempt', ue.event->'is_final_attempt', \
    'provider_key_id', ue.event->'provider_key_id', \
    'upstream_model', ue.event->'upstream_model', \
    'usage', jsonb_build_object(\
        'prompt_tokens', ue.event->'prompt_tokens', \
        'completion_tokens', ue.event->'completion_tokens', \
        'cached_prompt_tokens', ue.event->'cached_prompt_tokens', \
        'cache_write_tokens', ue.event->'cache_write_tokens', \
        'cache_creation_tokens', ue.event->'cache_creation_tokens', \
        'cache_creation_1h_tokens', ue.event->'cache_creation_1h_tokens', \
        'cache_read_tokens', ue.event->'cache_read_tokens', \
        'reasoning_tokens', ue.event->'reasoning_tokens', \
        'web_search_requests', ue.event->'web_search_requests', \
        'total_tokens', ue.event->'total_tokens'))";

/// A charge as the listing shows it: its row, the model it names, and how
/// its call ended.
#[derive(Debug, Serialize)]
struct ChargeItem {
    #[serde(flatten)]
    row: ChargeRow,
    /// The model's name: its document's, the public model's, or the name the
    /// call sent; `None` when the call named no model.
    model_name: Option<String>,
    /// Whether the model the charge names by id is gone.
    model_deleted: bool,
    /// The call's HTTP status; `None` when the row records none.
    status_code: Option<u16>,
    /// The failed attempt's error class; `None` for a success.
    error_class: Option<String>,
    /// `initial`, `retry` or `fallback`.
    attempt_kind: String,
    /// The attempt's 0-based index within its request.
    attempt_index: u64,
    is_final_attempt: Option<bool>,
    /// The upstream account that served the call, by id; `None` when the
    /// row names none.
    upstream_account_id: Option<String>,
    /// That account's display name, kept while its document or tombstone
    /// is; `None` when the account is gone or the row names none.
    upstream_account_name: Option<String>,
    /// Whether the account the row names is deleted.
    upstream_account_deleted: bool,
    /// The upstream model name the account was asked for.
    upstream_model: Option<String>,
    /// The counters the upstream reported, as the usage row stores them,
    /// only those it carries: the raw side of `lines`, for checking one
    /// against the other. `None` when the charge has no usage row.
    usage: Option<Map<String, Value>>,
}

/// A text field of `ev`, `None` when it is missing or empty.
fn ev_text(ev: &Value, key: &str) -> Option<String> {
    ev.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The name a model id is shown by, and whether the model is still there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelName {
    name: String,
    live: bool,
}

/// The names of the models `ids` name: a model document's `display_name`,
/// live or deleted (a tombstone is kept for a day), else the public model's
/// name, retired or not (a generated routing document's id is its public
/// model's). A model is live while its document is.
async fn model_names(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, ModelName>, DbError> {
    let mut names = HashMap::new();
    if ids.is_empty() {
        return Ok(names);
    }
    let docs: Vec<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "SELECT id, doc->>'display_name', NOT deleted FROM gw_resources \
         WHERE kind = $1 AND id = ANY($2)",
    )
    .bind(Kind::Models.segment())
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let public: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM public_models WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(&mut *conn)
            .await?;
    for (id, name) in public {
        names.insert(id, ModelName { name, live: false });
    }
    for (id, name, live) in docs {
        let name = name
            .filter(|n| !n.is_empty())
            .or_else(|| names.get(&id).map(|m| m.name.clone()))
            .unwrap_or_default();
        names.insert(id, ModelName { name, live });
    }
    Ok(names)
}

/// The display names of the upstream accounts `ids` name, live or deleted
/// (a tombstone is kept for a day), and whether each is still live.
async fn account_names(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, ModelName>, DbError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let docs: Vec<(Uuid, Option<String>, bool)> = sqlx::query_as(
        "SELECT id, doc->>'display_name', NOT deleted FROM gw_resources \
         WHERE kind = $1 AND id = ANY($2)",
    )
    .bind(Kind::ProviderKeys.segment())
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(docs
        .into_iter()
        .map(|(id, name, live)| {
            let name = name.filter(|n| !n.is_empty()).unwrap_or_default();
            (id, ModelName { name, live })
        })
        .collect())
}

/// The counters a usage row carries, numbers only, absent ones left out.
fn reported_usage(ev: &Value) -> Option<Map<String, Value>> {
    let usage = ev.get("usage")?.as_object()?;
    Some(
        usage
            .iter()
            .filter(|(_, v)| v.is_number())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// The name and the deleted flag of the model `requested_model_id` names, by
/// `names`, else by the name the call sent.
fn shown_model(
    names: &HashMap<Uuid, ModelName>,
    requested_model_id: &str,
    sent: Option<String>,
) -> (Option<String>, bool) {
    if requested_model_id.is_empty() {
        return (sent, false);
    }
    let known = Uuid::parse_str(requested_model_id)
        .ok()
        .and_then(|id| names.get(&id));
    match known {
        Some(m) if !m.name.is_empty() => (Some(m.name.clone()), !m.live),
        Some(m) => (sent, !m.live),
        None => (sent, true),
    }
}

/// The model ids `raw` holds, parsed, without repeats.
fn model_ids<'a>(raw: impl Iterator<Item = &'a str>) -> Vec<Uuid> {
    let mut ids: Vec<Uuid> = raw.filter_map(|r| Uuid::parse_str(r).ok()).collect();
    ids.sort();
    ids.dedup();
    ids
}

/// `GET /admin/pricing/charges` → `{items: [Charge], next_before}`: the
/// charge of one `event_id` (zero or one item), or charges by `status` (or
/// every status but `exclude_status`), `tenant_id`, `api_key_id`, `model_id`
/// (the charge's `requested_model_id`) and `error_code` (`unknown` for a
/// charge stored without one), newest first by `seq`, paged as
/// [`crate::usage::list_events`] is. A charge's tenant and member are the
/// ones its key belongs to, never what its usage row claims.
///
/// Each charge carries `cost_micro_cny`, what its row cost the platform at
/// the serving account's cost rate: `null` when the cost is unknown, never
/// 0. `cost_micro_usd` is a row priced before cost became a rate.
/// `adjusted_micro_cny` is the sum of its re-pricing adjustments, `null`
/// when no run corrected it. `model_name` names its model even once the
/// model is deleted (`model_deleted`); `status_code`, `error_class`,
/// `attempt_kind`, `attempt_index` and `is_final_attempt` say how its call
/// ended. `upstream_account_id`, `upstream_account_name` (`null` once the
/// account is gone; `upstream_account_deleted`) and `upstream_model` say
/// which upstream served it, and `usage` holds the counters the upstream
/// reported (only those the row carries), the raw side of `lines`. These
/// are operators' fields: no tenant view reads them.
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
    if let Some(status) = q.exclude_status.as_deref() {
        if !STATUSES.contains(&status) {
            return Err(refused("exclude_status", "未知的计费状态"));
        }
    }
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(format!(
        "SELECT pc.seq, pc.event_id, pc.tenant_id, pc.api_key_id, pc.user_id, \
                pc.requested_model_id, pc.occurred_at, pc.price_instant, pc.status, \
                pc.charge_micro_cny, pc.cost_micro_usd, pc.cost_source, pc.group_id, \
                pc.baseline_revision_id, pc.rate_revision_id, pc.cny_per_usd_e4, \
                pc.rate_is_model_override, pc.discount_revision_id, pc.discount_bps, \
                pc.service_tier, pc.requested_service_tier, pc.long_context_above, pc.estimated, \
                pc.exemption, pc.error_code, pc.lines, pc.warnings, pc.engine_version, \
                pc.priced_at, pc.cost_micro_cny, \
                (SELECT SUM(pa.delta_micro_cny)::bigint FROM pricing_adjustments_cny pa \
                 WHERE pa.event_id = pc.event_id) AS adjusted_micro_cny, \
                CASE WHEN ue.event_id IS NOT NULL THEN {CHARGE_EVENT} END AS ev \
         FROM pricing_charges_cny pc LEFT JOIN usage_events ue ON ue.event_id = pc.event_id \
         WHERE true"
    ));
    if let Some(before) = q.before {
        sql.push(" AND pc.seq < ").push_bind(before);
    }
    if let Some(event_id) = q.event_id {
        sql.push(" AND pc.event_id = ").push_bind(event_id);
    }
    if let Some(status) = q.status {
        sql.push(" AND pc.status = ").push_bind(status);
    }
    if let Some(status) = q.exclude_status {
        sql.push(" AND pc.status <> ").push_bind(status);
    }
    if let Some(tenant_id) = q.tenant_id {
        sql.push(" AND pc.tenant_id = ").push_bind(tenant_id);
    }
    if let Some(api_key_id) = q.api_key_id {
        sql.push(" AND pc.api_key_id = ").push_bind(api_key_id);
    }
    if let Some(model_id) = q.model_id {
        sql.push(" AND pc.requested_model_id = ")
            .push_bind(model_id);
    }
    match q.error_code.as_deref() {
        None => {}
        Some("unknown") => {
            sql.push(" AND pc.error_code IS NULL");
        }
        Some(code) => {
            sql.push(" AND pc.error_code = ")
                .push_bind(code.to_string());
        }
    }
    sql.push(" ORDER BY pc.seq DESC LIMIT ")
        .push_bind(limit + 1);
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let rows: Vec<ChargeRow> = sql
        .build_query_as()
        .fetch_all(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let (rows, next_before) = page(rows, limit, |r| r.seq);
    let names = model_names(
        &mut conn,
        &model_ids(rows.iter().map(|r| r.requested_model_id.as_str())),
    )
    .await?;
    let account_ids = model_ids(
        rows.iter()
            .filter_map(|r| r.ev.as_ref())
            .filter_map(|ev| ev.get("provider_key_id").and_then(Value::as_str)),
    );
    let accounts = account_names(&mut conn, &account_ids).await?;
    let items: Vec<ChargeItem> = rows
        .into_iter()
        .map(|row| {
            let ev = row.ev.clone().unwrap_or(Value::Null);
            let (model_name, model_deleted) = shown_model(
                &names,
                &row.requested_model_id,
                ev_text(&ev, "requested_model"),
            );
            let upstream_account_id = ev_text(&ev, "provider_key_id");
            let account = upstream_account_id
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .and_then(|id| accounts.get(&id));
            ChargeItem {
                upstream_account_name: account.map(|a| a.name.clone()).filter(|n| !n.is_empty()),
                upstream_account_deleted: upstream_account_id.is_some()
                    && account.is_none_or(|a| !a.live),
                upstream_account_id,
                upstream_model: ev_text(&ev, "upstream_model"),
                usage: reported_usage(&ev),
                model_name,
                model_deleted,
                status_code: ev
                    .get("status_code")
                    .and_then(Value::as_u64)
                    .and_then(|c| u16::try_from(c).ok())
                    .filter(|c| *c > 0),
                error_class: ev_text(&ev, "error_class"),
                attempt_kind: ev_text(&ev, "attempt_kind").unwrap_or_else(|| "initial".into()),
                attempt_index: ev.get("attempt_index").and_then(Value::as_u64).unwrap_or(0),
                is_final_attempt: ev.get("is_final_attempt").and_then(Value::as_bool),
                row,
            }
        })
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

// ---- status ----

/// `{total, by_code}` of `(code, count)` pairs.
fn counted(by_code: Vec<(String, i64)>) -> Value {
    let total: i64 = by_code.iter().map(|(_, n)| n).sum();
    let by_code: Map<String, Value> = by_code
        .into_iter()
        .map(|(code, n)| (code, json!(n)))
        .collect();
    json!({"total": total, "by_code": by_code})
}

/// `GET /admin/pricing/status` → `{pricing_version, invalid_revisions:
/// [{table, revision_id, reason}], unpriced_last_24h: {total, by_code},
/// unpriced_details: {items, truncated}, corrected_last_24h: {total,
/// by_code}}`: the
/// alert surface. `invalid_revisions` are the revisions the current price
/// book cannot use (`table` is `baseline`, `rate`, `cost` or `discount`);
/// their rows are stored unpriced. `by_code` counts the charges stored
/// unpriced in the last 24 hours (by when they were priced) under each
/// reason, the control plane's own (`unknown_api_key`, `tenant_mismatch`,
/// `undecodable_usage`) as well as the engine's. A charge an applied
/// re-pricing run has corrected since is counted under `corrected_last_24h`
/// instead. `unpriced_details` breaks the uncorrected ones down by reason
/// and model,
/// with the served tier for `missing_tier_price` and the group for
/// `missing_rate`: `{code, count, latest_at, model: {id, name, deleted},
/// service_tier, group: {id, name, display_name}}`, at most
/// [`store::MAX_UNPRICED_DETAILS`] of them, the most frequent first
/// (`truncated` when there were more; `unpriced_last_24h.by_code` still
/// counts every reason in full). `pricing_version` is null when the stored
/// version is missing or malformed.
pub async fn status(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let prices = state.prices.clone();
    let (version, invalid, by_code, corrected, details, truncated) = state
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
                let corrected = store::corrected_by_code(&mut *conn).await?;
                let (rows, truncated) = store::unpriced_details(&mut *conn).await?;
                let names = model_names(
                    &mut *conn,
                    &model_ids(rows.iter().map(|r| r.requested_model_id.as_str())),
                )
                .await?;
                let mut group_ids: Vec<Uuid> = rows.iter().filter_map(|r| r.group_id).collect();
                group_ids.sort();
                group_ids.dedup();
                let groups: HashMap<Uuid, (String, String)> =
                    sqlx::query_as::<_, (Uuid, String, String)>(
                        "SELECT id, name, display_name FROM provider_groups WHERE id = ANY($1)",
                    )
                    .bind(&group_ids)
                    .fetch_all(&mut *conn)
                    .await?
                    .into_iter()
                    .map(|(id, name, display)| (id, (name, display)))
                    .collect();
                let details: Vec<Value> = rows
                    .into_iter()
                    .map(|r| {
                        let (name, deleted) =
                            shown_model(&names, &r.requested_model_id, r.sent_model.clone());
                        json!({
                            "code": r.code,
                            "count": r.count,
                            "latest_at": r.latest_at,
                            "model": {
                                "id": (!r.requested_model_id.is_empty())
                                    .then_some(r.requested_model_id.as_str()),
                                "name": name,
                                "deleted": deleted,
                            },
                            "service_tier": r.service_tier,
                            "group": r.group_id.map(|id| {
                                let (name, display) =
                                    groups.get(&id).cloned().unwrap_or_default();
                                json!({"id": id, "name": name, "display_name": display})
                            }),
                        })
                    })
                    .collect();
                Ok::<_, DbError>((version, invalid, by_code, corrected, details, truncated))
            }
            .boxed()
        })
        .await?;
    Ok(Json(json!({
        "pricing_version": version,
        "invalid_revisions": invalid,
        "unpriced_last_24h": counted(by_code),
        "unpriced_details": {"items": details, "truncated": truncated},
        "corrected_last_24h": counted(corrected),
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
