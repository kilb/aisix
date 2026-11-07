//! The operators' pricing API, `/admin/pricing/*`: the built-in catalogue,
//! the sell, cost and discount revisions, the charges, and the pricing
//! status that is the alert surface for what cannot be priced.
//!
//! Revisions are append-only: there is no `PUT`, `PATCH` or `DELETE`, and a
//! price changes only by a new `POST` starting now or later. Every write goes
//! through [`super::store`], which refuses a start more than five seconds in
//! the past and writes the revision, the `pricing_version` bump and the
//! audit row in one transaction. A write is one transaction however many
//! revisions it makes, so a catalogue import lands whole or not at all.
//!
//! Charges and cost prices name upstream channels; this API is for
//! operators only and is never projected to tenants.

use aisix_pricing::{Catalogue, CatalogueEntry, RateCard};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, NaiveTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{Postgres, QueryBuilder};
use uuid::Uuid;

use super::store::{
    self, CostRevision, DiscountRevision, NewCostPrice, NewDiscount, NewSellPrice, SellRevision,
};
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

/// Any value a field holds, `null` included, as `Some`: a field that is
/// absent stays `None` through `#[serde(default)]`, so the two can be told
/// apart.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// A rate card from a request body, refused at `/card` when it does not
/// decode. Whether it validates is the store's check.
fn rate_card(card: Value) -> Result<RateCard, ApiError> {
    serde_json::from_value(card).map_err(|e| refused("/card", format!("价目卡不合法：{e}")))
}

/// UTC midnight of a catalogue date: when an entry dated that day starts.
fn day_start(day: chrono::NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
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

// ---- sell prices ----

#[derive(Deserialize)]
pub struct SellQuery {
    model_id: Option<Uuid>,
    tenant_id: Option<Uuid>,
    limit: Option<i64>,
    /// Only revisions with a smaller `revision_id`: the `next_before` of
    /// the previous page.
    before: Option<i64>,
}

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

/// `GET /admin/pricing/sell` → `{items: [SellRevision], next_before}`:
/// sell revisions, newest first, optionally only those of a `model_id` or a
/// `tenant_id`, `limit` (default 50, at most 500) at a time.
pub async fn list_sell(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<SellQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT revision_id, engine_version, model_id, tenant_id, effective_from, card, source, \
                created_by, created_at \
         FROM pricing_sell_revisions WHERE true",
    );
    if let Some(model_id) = q.model_id {
        sql.push(" AND model_id = ").push_bind(model_id);
    }
    if let Some(tenant_id) = q.tenant_id {
        sql.push(" AND tenant_id = ").push_bind(tenant_id);
    }
    newest_revisions_first(&mut sql, q.before, limit);
    let rows: Vec<SellRevision> = sql
        .build_query_as()
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    let (items, next_before) = page(rows, limit, |r| r.revision_id);
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

/// `POST /admin/pricing/sell`. Exactly one of `card` (a rate card, or
/// `null` to end a tenant's override) and `catalogue_key` (import a list
/// price, marked up by `markup_bps`).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SellBody {
    model_id: Uuid,
    #[serde(default)]
    tenant_id: Option<Uuid>,
    effective_from: DateTime<Utc>,
    #[serde(default, deserialize_with = "present")]
    card: Option<Value>,
    #[serde(default)]
    catalogue_key: Option<String>,
    #[serde(default)]
    markup_bps: Option<u32>,
}

/// Something an operator should know about a price they wrote, which does
/// not stop it being written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PriceWarning {
    pub code: &'static str,
    pub message: String,
}

/// One sell revision a catalogue import writes.
#[derive(Debug, Clone, PartialEq)]
struct Planned {
    effective_from: DateTime<Utc>,
    card: RateCard,
    source: String,
}

/// What importing a catalogue key comes to: the revisions, the first one
/// starting at the requested instant, and what the operator is told.
#[derive(Debug, Clone, PartialEq)]
struct ImportPlan {
    revisions: Vec<Planned>,
    warnings: Vec<PriceWarning>,
}

/// Whether an entry's list price holds only at some times of day: its notes
/// speak of peak hours. Such a price is the off-peak one, and peak hours
/// have to be priced through the markup.
fn has_time_of_day_price(entry: &CatalogueEntry) -> bool {
    entry
        .notes
        .as_deref()
        .is_some_and(|n| n.to_ascii_lowercase().contains("peak"))
}

/// The sell revisions importing `key` (`<provider>/<model>`, or an alias)
/// at `effective_from` with `markup_bps` writes:
/// - the entry in effect at `effective_from`, starting then;
/// - each later-dated entry of the same key, starting at its own date with
///   the same markup, so an upstream price change announced in the
///   catalogue carries into the sell price instead of the platform selling
///   below its new cost.
///
/// Each source is `catalogue:<version>:<entry key>@<entry date or ->`. An
/// unknown key is refused at `/catalogue_key`, a key with no entry in effect
/// yet at `/effective_from`, and a markup that takes a rate past the largest
/// one at `/markup_bps`.
fn import_plan(
    catalogue: &Catalogue,
    key: &str,
    effective_from: DateTime<Utc>,
    markup_bps: u32,
) -> Result<ImportPlan, ApiError> {
    let unknown = || refused("/catalogue_key", "价目目录中没有这个模型");
    let (provider, model) = key.split_once('/').ok_or_else(unknown)?;
    // Known at all: some entry of it is in effect at the end of time.
    catalogue
        .get(provider, model, DateTime::<Utc>::MAX_UTC)
        .ok_or_else(unknown)?;
    let first = catalogue
        .get(provider, model, effective_from)
        .ok_or_else(|| refused("/effective_from", "该目录价格在此生效时间之前还没有生效"))?;
    let mut later: Vec<&CatalogueEntry> = catalogue
        .entries()
        .iter()
        .filter(|e| {
            e.key == first.key
                && e.effective_from
                    .is_some_and(|d| day_start(d) > effective_from)
        })
        .collect();
    later.sort_by_key(|e| e.effective_from);

    let mut revisions = Vec::new();
    let mut warnings = Vec::new();
    for (entry, starts) in std::iter::once((first, effective_from)).chain(
        later
            .iter()
            .filter_map(|e| e.effective_from.map(|d| (*e, day_start(d)))),
    ) {
        let card = entry
            .card
            .with_markup(markup_bps)
            .map_err(|e| refused("/markup_bps", format!("加价后的价格超出上限：{e}")))?;
        let dated = entry
            .effective_from
            .map_or_else(|| "-".to_string(), |d| d.to_string());
        revisions.push(Planned {
            effective_from: starts,
            card,
            source: format!("catalogue:{}:{}@{dated}", catalogue.version(), entry.key),
        });
        if has_time_of_day_price(entry) {
            warnings.push(PriceWarning {
                code: "time_of_day_price",
                message: format!(
                    "{} 的目录价格是非高峰时段价格，高峰时段上游按更高价格收费；请通过加价覆盖高峰时段的成本",
                    entry.key
                ),
            });
        }
    }
    warnings.dedup();
    Ok(ImportPlan {
        revisions,
        warnings,
    })
}

/// `POST /admin/pricing/sell` → `201`: the sell revision starting at
/// `effective_from`, plus `warnings` and the `later_revisions` a catalogue
/// import also wrote (both empty otherwise). Three bodies:
/// - `{model_id, tenant_id?, effective_from, card}`: a rate card, the
///   platform's price or, with `tenant_id`, that tenant's override;
/// - `{model_id, tenant_id?, effective_from, catalogue_key, markup_bps?}`:
///   the catalogue's price marked up (see [`import_plan`]);
/// - `{model_id, tenant_id, effective_from, card: null}`: ends the tenant's
///   override from that instant.
///
/// Both `card` and `catalogue_key`, or neither, is `422 /card`; a markup
/// without `catalogue_key` is `422 /markup_bps`.
pub async fn create_sell(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SellBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let (plan, card_given) = match (body.card, body.catalogue_key) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(refused(
                "/card",
                "必须且只能提供 card 或 catalogue_key 其中之一",
            ))
        }
        (Some(card), None) => {
            if body.markup_bps.is_some() {
                return Err(refused(
                    "/markup_bps",
                    "markup_bps 只用于从价目目录导入（catalogue_key）",
                ));
            }
            let card = match card {
                Value::Null => None,
                card => Some(rate_card(card)?),
            };
            (None, card)
        }
        (None, Some(key)) => {
            let plan = import_plan(
                state.prices.catalogue(),
                &key,
                body.effective_from,
                body.markup_bps.unwrap_or(0),
            )?;
            (Some(plan), None)
        }
    };
    let (model_id, tenant_id, effective_from) =
        (body.model_id, body.tenant_id, body.effective_from);
    let rows: Vec<NewSellPrice> = match &plan {
        Some(plan) => plan
            .revisions
            .iter()
            .map(|p| NewSellPrice {
                model_id,
                tenant_id,
                effective_from: p.effective_from,
                card: Some(p.card.clone()),
                source: p.source.clone(),
            })
            .collect(),
        None => vec![NewSellPrice {
            model_id,
            tenant_id,
            effective_from,
            card: card_given,
            source: "manual".to_string(),
        }],
    };
    let written: Vec<SellRevision> = state
        .db
        .tx(move |conn| {
            async move {
                let mut written = Vec::with_capacity(rows.len());
                for row in &rows {
                    written.push(
                        store::insert_sell_price(&mut *conn, row, Actor::Operator(&auth)).await?,
                    );
                }
                Ok::<_, ApiError>(written)
            }
            .boxed()
        })
        .await?;
    let mut written = written.into_iter();
    let first = written
        .next()
        .ok_or_else(|| ApiError::internal("a sell price write wrote no revision"))?;
    let mut view = to_object(&first)?;
    view.insert(
        "warnings".to_string(),
        json!(plan.as_ref().map_or(&[][..], |p| &p.warnings[..])),
    );
    view.insert(
        "later_revisions".to_string(),
        json!(written.collect::<Vec<_>>()),
    );
    Ok((StatusCode::CREATED, Json(Value::Object(view))))
}

/// `value` as a JSON object, to add fields to.
fn to_object(value: &impl Serialize) -> Result<Map<String, Value>, ApiError> {
    match serde_json::to_value(value).map_err(ApiError::internal)? {
        Value::Object(fields) => Ok(fields),
        _ => Err(ApiError::internal(
            "a revision did not serialize as an object",
        )),
    }
}

// ---- cost prices ----

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

/// `GET /admin/pricing/cost` → `{items: [CostRevision], next_before}`: cost
/// revisions, newest first, optionally only those of an
/// `upstream_provider`, an `upstream_model` or a `provider_key_id`; paged as
/// [`list_sell`] is.
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

/// `POST /admin/pricing/cost`: what an upstream model costs the platform,
/// or, with `provider_key_id`, what it costs through that key. A card is
/// required.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostBody {
    upstream_provider: String,
    upstream_model: String,
    #[serde(default)]
    provider_key_id: Option<Uuid>,
    effective_from: DateTime<Utc>,
    #[serde(default)]
    card: Option<Value>,
}

/// `POST /admin/pricing/cost` → `201 CostRevision`. An empty provider or
/// model is `422` at its path, and a missing or `null` card `422 /card`.
pub async fn create_cost(
    auth: OperatorAuth,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<CostBody>,
) -> Result<(StatusCode, Json<CostRevision>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    if body.upstream_provider.trim().is_empty() {
        return Err(refused("/upstream_provider", "上游提供方不能为空"));
    }
    if body.upstream_model.trim().is_empty() {
        return Err(refused("/upstream_model", "上游模型不能为空"));
    }
    let card = match body.card {
        None | Some(Value::Null) => return Err(refused("/card", "成本价格必须带价目卡")),
        Some(card) => rate_card(card)?,
    };
    let row = NewCostPrice {
        upstream_provider: body.upstream_provider,
        upstream_model: body.upstream_model,
        provider_key_id: body.provider_key_id,
        effective_from: body.effective_from,
        card,
        source: "manual".to_string(),
    };
    let revision = state
        .db
        .tx(move |conn| {
            async move { store::insert_cost_price(conn, &row, Actor::Operator(&auth)).await }
                .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(revision)))
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
    charge_micro_usd: Option<i64>,
    cost_micro_usd: Option<i64>,
    cost_source: Option<Value>,
    sell_revision_id: Option<i64>,
    tenant_override: bool,
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
}

/// `GET /admin/pricing/charges` → `{items: [Charge], next_before}`: the
/// charge of one `event_id` (zero or one item), or charges by `status`,
/// `tenant_id` and `api_key_id`, newest first by `seq`, paged as
/// [`crate::usage::list_events`] is. A charge's tenant and member are the
/// ones its key belongs to, never what its usage row claims.
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
                price_instant, status, charge_micro_usd, cost_micro_usd, cost_source, \
                sell_revision_id, tenant_override, discount_revision_id, discount_bps, \
                service_tier, requested_service_tier, long_context_above, estimated, exemption, \
                error_code, lines, warnings, engine_version, priced_at \
         FROM pricing_charges WHERE true",
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
/// price book cannot use (`table` is `sell`, `cost` or `discount`); their
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
                let by_code: Vec<(String, i64)> = sqlx::query_as(
                    "SELECT COALESCE(error_code, 'unknown'), count(*)::bigint FROM pricing_charges \
                     WHERE status = 'unpriced' AND priced_at >= now() - interval '24 hours' \
                     GROUP BY 1 ORDER BY 1",
                )
                .fetch_all(&mut *conn)
                .await?;
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
    use aisix_pricing::{Rate, ServiceTier};

    use super::*;

    /// A catalogue with an announced price change (`openai/m1` from
    /// 2026-12-01), a key only priced from 2027-01-01, and an off-peak price
    /// with an alias.
    fn catalogue() -> Catalogue {
        let entry = |key: &str, extra: &str, input: &str| {
            format!(
                r#"{{"key":"{key}","source_url":"https://openai.com/api/pricing","as_of":"2026-11-01",{extra}"card":{{"default":{{"base":{{"input":"{input}","output":"2"}}}}}}}}"#
            )
        };
        let entries = [
            entry("openai/m1", "", "1"),
            entry("openai/m1", r#""effective_from":"2026-12-01","#, "3"),
            entry("openai/m1", r#""effective_from":"2027-02-01","#, "5"),
            entry("openai/soon", r#""effective_from":"2027-01-01","#, "1"),
            entry(
                "openai/offpeak",
                r#""aliases":["offpeak-alias"],"notes":"Standard (off-peak) price; peak windows bill double.","#,
                "1",
            ),
        ];
        Catalogue::from_json(&format!(
            r#"{{"version":"2026-11-06","entries":[{}]}}"#,
            entries.join(",")
        ))
        .expect("the test catalogue loads")
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn input(card: &RateCard) -> Rate {
        card.0[&ServiceTier::default_tier()].base.input
    }

    /// `(starts, input rate, source)` of each planned revision.
    fn summary(plan: &ImportPlan) -> Vec<(DateTime<Utc>, Rate, &str)> {
        plan.revisions
            .iter()
            .map(|p| (p.effective_from, input(&p.card), p.source.as_str()))
            .collect()
    }

    fn path_of(e: &ApiError) -> Vec<&str> {
        e.details.iter().map(|d| d.path.as_str()).collect()
    }

    #[test]
    fn an_import_carries_every_announced_price_change_with_the_same_markup() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        let plan = import_plan(&c, "openai/m1", now, 5_000).expect("a plan");
        // 1, 3 and 5 USD per million, each × 1.5.
        assert_eq!(
            summary(&plan),
            vec![
                (now, Rate(1_500_000), "catalogue:2026-11-06:openai/m1@-"),
                (
                    at("2026-12-01T00:00:00Z"),
                    Rate(4_500_000),
                    "catalogue:2026-11-06:openai/m1@2026-12-01"
                ),
                (
                    at("2027-02-01T00:00:00Z"),
                    Rate(7_500_000),
                    "catalogue:2026-11-06:openai/m1@2027-02-01"
                ),
            ]
        );
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn an_import_starts_from_the_entry_in_effect_then() {
        let c = catalogue();
        // On the day of the change, the change itself is in effect.
        let plan = import_plan(&c, "openai/m1", at("2026-12-01T00:00:00Z"), 0).expect("a plan");
        assert_eq!(
            summary(&plan),
            vec![
                (
                    at("2026-12-01T00:00:00Z"),
                    Rate(3_000_000),
                    "catalogue:2026-11-06:openai/m1@2026-12-01"
                ),
                (
                    at("2027-02-01T00:00:00Z"),
                    Rate(5_000_000),
                    "catalogue:2026-11-06:openai/m1@2027-02-01"
                ),
            ]
        );
        let plan = import_plan(&c, "openai/m1", at("2027-03-01T00:00:00Z"), 0).expect("a plan");
        assert_eq!(plan.revisions.len(), 1);
        assert_eq!(input(&plan.revisions[0].card), Rate(5_000_000));
    }

    #[test]
    fn an_import_names_a_known_key_in_effect_at_its_start() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        for key in ["openai/nope", "nope", "anthropic/m1", "openai/"] {
            let e = import_plan(&c, key, now, 0).expect_err(key);
            assert_eq!(path_of(&e), ["/catalogue_key"], "{key}");
        }
        let e = import_plan(&c, "openai/soon", now, 0).expect_err("not in effect yet");
        assert_eq!(path_of(&e), ["/effective_from"]);
        let plan =
            import_plan(&c, "openai/soon", at("2027-01-01T00:00:00Z"), 0).expect("in effect");
        assert_eq!(plan.revisions.len(), 1);
        // A markup no rate can hold.
        let e = import_plan(&c, "openai/m1", now, u32::MAX).expect_err("too large");
        assert_eq!(path_of(&e), ["/markup_bps"]);
    }

    #[test]
    fn an_off_peak_list_price_is_imported_with_a_warning() {
        let c = catalogue();
        let now = at("2026-11-06T09:30:00Z");
        let plan = import_plan(&c, "openai/offpeak-alias", now, 0).expect("a plan");
        assert_eq!(
            summary(&plan),
            vec![(
                now,
                Rate(1_000_000),
                "catalogue:2026-11-06:openai/offpeak@-"
            )]
        );
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.warnings[0].code, "time_of_day_price");
        assert!(plan.warnings[0].message.contains("openai/offpeak"));
    }

    #[test]
    fn only_the_builtin_entries_that_speak_of_peak_hours_warn() {
        let builtin = Catalogue::builtin().expect("the builtin catalogue");
        let warned: Vec<&str> = builtin
            .entries()
            .iter()
            .filter(|e| has_time_of_day_price(e))
            .map(|e| e.key.as_str())
            .collect();
        assert!(!warned.is_empty());
        assert!(
            warned.iter().all(|k| k.starts_with("deepseek/")),
            "{warned:?}"
        );
    }
}
