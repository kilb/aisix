//! The price tables and the charges: price revisions, written append-only;
//! the in-memory price book read from them; and the one charge each stored
//! usage row gets.
//!
//! # Prices start now or later
//!
//! A revision never changes once written: there is no `UPDATE` or `DELETE`
//! on a revision table here, and a price changes only by a new revision.
//! Every write refuses an `effective_from` more than [`BACKDATE_TOLERANCE`]
//! before the database's own `now()`, so a row already priced is never
//! priced differently by a revision that arrives later. Correcting past
//! charges is re-pricing, which appends adjustments and never edits a charge.
//!
//! Each write runs in its caller's transaction: the revision, the bump of
//! `cp_settings.pricing_version` and the audit row commit together or not at
//! all. The bump also serializes concurrent price writes on that one row.
//!
//! # The book follows `pricing_version`
//!
//! [`PriceBookCache::current`] reads `pricing_version` on the caller's
//! connection and reloads the book on that same connection only when the
//! version moved. The version and the revisions commit together, so a book
//! loaded after reading version `v` holds at least every revision of `v`; a
//! later commit only makes the next read reload. A version row that is
//! missing or not a whole number never stops intake: the book is then
//! reloaded for every batch, and each such read is logged at ERROR.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use aisix_pricing::{
    Catalogue, ChargeLine, CostPriceRow, CostSource, DiscountRow, Exemption, Outcome, PriceBook,
    PriceRows, PricedEvent, PricingError, PricingWarning, RateCard, RevisionTable, SellPriceRow,
    ENGINE_VERSION,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::types::Json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::db::DbError;
use crate::error::{ApiError, Detail};

/// How far before the database's `now()` a revision may start: the only
/// backdating allowed, for the moment between an operator's choice of
/// "now" and the commit.
pub const BACKDATE_TOLERANCE: TimeDelta = TimeDelta::seconds(5);

/// The most characters of a row's `team_id`, `api_key_id`, `user_id` or
/// `requested_model_id` a charge copies: the same cap usage intake puts on
/// its filter columns. A longer value names no real id, and copied whole it
/// could exceed what an index entry holds, failing the batch it came in.
const MAX_ID_CHARS: usize = 256;

/// The in-memory price book, keyed on `cp_settings.pricing_version`.
pub struct PriceBookCache {
    catalogue: Arc<Catalogue>,
    cached: RwLock<Option<(i64, Arc<PriceBook>)>>,
    /// Held across a reload, so readers that find the book stale together
    /// share one load. It guards no data: a failed load just releases it and
    /// the next reader tries again.
    reload: tokio::sync::Mutex<()>,
    loads: AtomicU64,
}

impl PriceBookCache {
    pub fn new(catalogue: Arc<Catalogue>) -> PriceBookCache {
        PriceBookCache {
            catalogue,
            cached: RwLock::new(None),
            reload: tokio::sync::Mutex::new(()),
            loads: AtomicU64::new(0),
        }
    }

    /// Reads `pricing_version` on `conn`, the caller's transaction. Reloads
    /// the book on the same connection only when the version differs from
    /// the cached one, and logs one ERROR per invalid revision on each
    /// reload. Readers that find it stale at the same time share one reload.
    /// A version that is missing or not a whole number is logged at ERROR and
    /// reloads the book on every call, so intake keeps pricing from what the
    /// tables hold. The cache lock is never held across an await.
    pub async fn current(&self, conn: &mut PgConnection) -> Result<Arc<PriceBook>, DbError> {
        let Some(version) = pricing_version(&mut *conn).await? else {
            tracing::error!(
                "cp_settings.pricing_version is missing or is not a whole number; the price \
                 book is reloaded for every batch until it is restored"
            );
            // A book cached under an earlier version may predate price
            // writes made while the version could not move.
            *self.cached.write().unwrap_or_else(PoisonError::into_inner) = None;
            return self.load(conn).await;
        };
        if let Some(book) = self.cached_at(version) {
            return Ok(book);
        }
        // One reader reloads; the rest wait here and then find its book.
        let _reloading = self.reload.lock().await;
        if let Some(book) = self.cached_at(version) {
            return Ok(book);
        }
        let book = self.load(conn).await?;
        *self.cached.write().unwrap_or_else(PoisonError::into_inner) =
            Some((version, book.clone()));
        Ok(book)
    }

    /// Loads the book on `conn`, logging each invalid revision.
    async fn load(&self, conn: &mut PgConnection) -> Result<Arc<PriceBook>, DbError> {
        let book = Arc::new(load_price_book(conn, self.catalogue.clone()).await?);
        for r in book.invalid_revisions() {
            tracing::error!(
                table = table_name(r.table),
                revision_id = r.revision_id,
                reason = %r.reason,
                "price revision cannot be used; its rows are unpriced"
            );
        }
        self.loads.fetch_add(1, Ordering::Relaxed);
        Ok(book)
    }

    /// The cached book, when it was loaded at `version`.
    fn cached_at(&self, version: i64) -> Option<Arc<PriceBook>> {
        let cached = self.cached.read().unwrap_or_else(PoisonError::into_inner);
        cached
            .as_ref()
            .filter(|(at, _)| *at == version)
            .map(|(_, book)| book.clone())
    }

    /// How many times the book has been loaded, for tests.
    pub fn loads(&self) -> u64 {
        self.loads.load(Ordering::Relaxed)
    }

    pub fn catalogue(&self) -> &Arc<Catalogue> {
        &self.catalogue
    }
}

/// `cp_settings.pricing_version`, read on `conn`; `None` when the row is
/// missing or its value is not a whole number a bigint holds.
pub async fn pricing_version(conn: &mut PgConnection) -> Result<Option<i64>, DbError> {
    // Read only when it is a whole number a bigint holds, so a hand-edited
    // value is "no version" instead of a cast error that would fail the
    // batch on every delivery.
    Ok(sqlx::query_scalar::<_, Option<i64>>(
        "SELECT CASE WHEN jsonb_typeof(value) = 'number' AND value::text ~ '^-?[0-9]{1,18}$' \
                     THEN (value::text)::bigint END \
         FROM cp_settings WHERE key = 'pricing_version'",
    )
    .fetch_optional(conn)
    .await?
    .flatten())
}

/// The table an invalid revision is in, as the alert names it.
fn table_name(table: RevisionTable) -> &'static str {
    match table {
        RevisionTable::Sell => "pricing_sell_revisions",
        RevisionTable::Cost => "pricing_cost_revisions",
        RevisionTable::Discount => "pricing_discount_revisions",
    }
}

type SellTableRow = (i64, i64, Uuid, Option<Uuid>, DateTime<Utc>, Option<Value>);
type CostTableRow = (i64, i64, String, String, Option<Uuid>, DateTime<Utc>, Value);
type DiscountTableRow = (i64, i64, Uuid, DateTime<Utc>, i64);

/// Every revision of the three tables, read on `conn`, as a book. Ids are
/// written as `Uuid::to_string()`, the lowercase hyphenated form the control
/// plane projects and the gateway echoes back; cards stay raw JSON, so one
/// bad or newer card is isolated to its own revision.
pub async fn load_price_book(
    conn: &mut PgConnection,
    catalogue: Arc<Catalogue>,
) -> Result<PriceBook, DbError> {
    let sell: Vec<SellTableRow> = sqlx::query_as(
        "SELECT revision_id, engine_version, model_id, tenant_id, effective_from, card \
         FROM pricing_sell_revisions",
    )
    .fetch_all(&mut *conn)
    .await?;
    let cost: Vec<CostTableRow> = sqlx::query_as(
        "SELECT revision_id, engine_version, upstream_provider, upstream_model, provider_key_id, \
                effective_from, card \
         FROM pricing_cost_revisions",
    )
    .fetch_all(&mut *conn)
    .await?;
    let discounts: Vec<DiscountTableRow> = sqlx::query_as(
        "SELECT revision_id, engine_version, tenant_id, effective_from, discount_bps \
         FROM pricing_discount_revisions",
    )
    .fetch_all(&mut *conn)
    .await?;
    let rows = PriceRows {
        sell: sell
            .into_iter()
            .map(
                |(revision_id, engine, model_id, tenant_id, effective_from, card)| SellPriceRow {
                    revision_id,
                    engine_version: engine_version(engine),
                    model_id: model_id.to_string(),
                    tenant_id: tenant_id.map(|t| t.to_string()),
                    effective_from,
                    card,
                },
            )
            .collect(),
        cost: cost
            .into_iter()
            .map(
                |(
                    revision_id,
                    engine,
                    upstream_provider,
                    upstream_model,
                    key,
                    effective_from,
                    card,
                )| {
                    CostPriceRow {
                        revision_id,
                        engine_version: engine_version(engine),
                        upstream_provider,
                        upstream_model,
                        provider_key_id: key.map(|k| k.to_string()),
                        effective_from,
                        card,
                    }
                },
            )
            .collect(),
        discounts: discounts
            .into_iter()
            .map(
                |(revision_id, engine, tenant_id, effective_from, discount_bps)| DiscountRow {
                    revision_id,
                    engine_version: engine_version(engine),
                    tenant_id: tenant_id.to_string(),
                    effective_from,
                    discount_bps,
                },
            )
            .collect(),
    };
    Ok(PriceBook::from_rows(rows, catalogue))
}

/// A stored `engine_version` as the book reads it. One no `u32` holds
/// (negative, or above `u32::MAX`, which only raw SQL can write) reads as
/// `u32::MAX`: its revision is isolated as written by a newer engine, and it
/// neither drops the row nor fails the whole load.
fn engine_version(stored: i64) -> u32 {
    u32::try_from(stored).unwrap_or(u32::MAX)
}

/// Who a charge is for. `team_id` (the tenant) and `user_id` (the member)
/// come from the control plane's record of the row's key, `""` when its key
/// does not resolve or has no member; `api_key_id` and `requested_model_id`
/// are the row's own text fields, `""` when it has none.
#[derive(Debug, Clone, Copy)]
pub struct ChargeIdentity<'a> {
    pub event_id: &'a str,
    pub occurred_at: DateTime<Utc>,
    pub team_id: &'a str,
    pub api_key_id: &'a str,
    pub user_id: &'a str,
    pub requested_model_id: &'a str,
}

/// Why a stored row was not priced. It is recorded as an `unpriced` charge
/// with [`ChargeError::code`] as its `error_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChargeError {
    /// The engine could not price the row.
    Pricing(PricingError),
    /// The row's fields do not decode as usage the engine reads.
    Undecodable,
    /// The row names a key the control plane never issued.
    UnknownApiKey,
    /// The row names a tenant other than the one its key belongs to.
    TenantMismatch,
}

impl ChargeError {
    /// `PricingError::code()`, `undecodable_usage`, `unknown_api_key` or
    /// `tenant_mismatch`.
    pub fn code(&self) -> &'static str {
        match self {
            ChargeError::Pricing(e) => e.code(),
            ChargeError::Undecodable => "undecodable_usage",
            ChargeError::UnknownApiKey => "unknown_api_key",
            ChargeError::TenantMismatch => "tenant_mismatch",
        }
    }
}

/// What [`record_charge`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Inserted,
    /// The row had its charge already; charges are written once.
    AlreadyRecorded,
}

/// `value` when it is at most [`MAX_ID_CHARS`] characters, otherwise `""`.
fn id_column(value: &str) -> &str {
    if value.chars().count() <= MAX_ID_CHARS {
        value
    } else {
        ""
    }
}

/// The columns a charge writes beyond the row's identity.
struct Columns<'a> {
    price_instant: Option<DateTime<Utc>>,
    status: &'static str,
    charge: Option<i64>,
    cost: Option<i64>,
    cost_source: Option<Json<&'a CostSource>>,
    sell_revision_id: Option<i64>,
    tenant_override: bool,
    discount_revision_id: Option<i64>,
    discount_bps: i64,
    service_tier: Option<&'a str>,
    requested_service_tier: Option<&'a str>,
    long_context_above: Option<i64>,
    estimated: bool,
    exemption: Option<&'static str>,
    error_code: Option<&'static str>,
    lines: Json<&'a [ChargeLine]>,
    warnings: Json<&'a [PricingWarning]>,
    engine_version: i64,
}

impl<'a> Columns<'a> {
    fn of(charge: &'a Result<PricedEvent, ChargeError>) -> Columns<'a> {
        match charge {
            Ok(p) => Columns {
                price_instant: Some(p.price_instant),
                status: p.outcome.status(),
                charge: Some(p.charge.0),
                cost: p.cost.as_ref().map(|c| c.amount.0),
                cost_source: p.cost.as_ref().map(|c| Json(&c.source)),
                sell_revision_id: p.sell.map(|s| s.revision_id),
                tenant_override: p.sell.is_some_and(|s| s.tenant_override),
                discount_revision_id: p.discount.map(|d| d.revision_id),
                discount_bps: p.discount.map_or(0, |d| i64::from(d.bps)),
                service_tier: p.service_tier.as_ref().map(|t| t.as_str()),
                requested_service_tier: p.requested_service_tier.as_deref(),
                long_context_above: p
                    .long_context_above
                    .map(|above| i64::try_from(above).unwrap_or(i64::MAX)),
                estimated: p.estimated,
                exemption: match p.outcome {
                    Outcome::Exempt(Exemption::ResponseCacheHit) => Some("response_cache_hit"),
                    Outcome::Exempt(Exemption::NotModelUsage) => Some("not_model_usage"),
                    Outcome::Charged | Outcome::NothingToBill => None,
                },
                error_code: None,
                lines: Json(&p.lines),
                warnings: Json(&p.warnings),
                engine_version: i64::from(p.engine_version),
            },
            Err(e) => Columns {
                price_instant: None,
                status: "unpriced",
                charge: None,
                cost: None,
                cost_source: None,
                sell_revision_id: None,
                tenant_override: false,
                discount_revision_id: None,
                discount_bps: 0,
                service_tier: None,
                requested_service_tier: None,
                long_context_above: None,
                estimated: false,
                exemption: None,
                error_code: Some(e.code()),
                lines: Json(&[]),
                warnings: Json(&[]),
                engine_version: i64::from(ENGINE_VERSION),
            },
        }
    }
}

/// Records the charge of the stored row `ident`, once: `AlreadyRecorded`
/// when the row has one. An `Err` is stored as an `unpriced` charge with its
/// code and no amount.
pub async fn record_charge(
    conn: &mut PgConnection,
    ident: &ChargeIdentity<'_>,
    charge: &Result<PricedEvent, ChargeError>,
) -> Result<Recorded, DbError> {
    let c = Columns::of(charge);
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO pricing_charges \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, \
          price_instant, status, charge_micro_usd, cost_micro_usd, cost_source, sell_revision_id, \
          tenant_override, discount_revision_id, discount_bps, service_tier, \
          requested_service_tier, long_context_above, estimated, exemption, error_code, lines, \
          warnings, engine_version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, \
                 $19, $20, $21, $22, $23, $24) \
         ON CONFLICT (event_id) DO NOTHING RETURNING event_id",
    )
    .bind(ident.event_id)
    .bind(id_column(ident.team_id))
    .bind(id_column(ident.api_key_id))
    .bind(id_column(ident.user_id))
    .bind(id_column(ident.requested_model_id))
    .bind(ident.occurred_at)
    .bind(c.price_instant)
    .bind(c.status)
    .bind(c.charge)
    .bind(c.cost)
    .bind(c.cost_source)
    .bind(c.sell_revision_id)
    .bind(c.tenant_override)
    .bind(c.discount_revision_id)
    .bind(c.discount_bps)
    .bind(c.service_tier)
    .bind(c.requested_service_tier)
    .bind(c.long_context_above)
    .bind(c.estimated)
    .bind(c.exemption)
    .bind(c.error_code)
    .bind(c.lines)
    .bind(c.warnings)
    .bind(c.engine_version)
    .fetch_optional(conn)
    .await?;
    Ok(match inserted {
        Some(_) => Recorded::Inserted,
        None => Recorded::AlreadyRecorded,
    })
}

/// A sell price to write: the platform's price for a model or, with a
/// `tenant_id`, that tenant's override. `card: None` only with a
/// `tenant_id`: it ends the override from `effective_from`.
#[derive(Debug, Clone)]
pub struct NewSellPrice {
    pub model_id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    pub card: Option<RateCard>,
    pub source: String,
}

/// A cost price to write: what an upstream model costs the platform or,
/// with a `provider_key_id`, what it costs through that one key.
#[derive(Debug, Clone)]
pub struct NewCostPrice {
    pub upstream_provider: String,
    pub upstream_model: String,
    pub provider_key_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    pub card: RateCard,
    pub source: String,
}

/// A tenant discount to write, in basis points (0 to 10000).
#[derive(Debug, Clone)]
pub struct NewDiscount {
    pub tenant_id: Uuid,
    pub effective_from: DateTime<Utc>,
    pub discount_bps: u16,
}

/// One `pricing_sell_revisions` row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct SellRevision {
    pub revision_id: i64,
    pub engine_version: i64,
    pub model_id: Uuid,
    pub tenant_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    pub card: Option<Value>,
    pub source: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// One `pricing_cost_revisions` row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CostRevision {
    pub revision_id: i64,
    pub engine_version: i64,
    pub upstream_provider: String,
    pub upstream_model: String,
    pub provider_key_id: Option<Uuid>,
    pub effective_from: DateTime<Utc>,
    pub card: Value,
    pub source: String,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// One `pricing_discount_revisions` row.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DiscountRevision {
    pub revision_id: i64,
    pub engine_version: i64,
    pub tenant_id: Uuid,
    pub effective_from: DateTime<Utc>,
    pub discount_bps: i64,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// A `422` at `path`.
fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// Refuses a revision that would start more than [`BACKDATE_TOLERANCE`]
/// before the database's `now()`.
async fn starts_now_or_later(
    conn: &mut PgConnection,
    effective_from: DateTime<Utc>,
) -> Result<(), ApiError> {
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
    if effective_from < now - BACKDATE_TOLERANCE {
        return Err(refused(
            "/effective_from",
            "生效时间不能早于当前时间：价格只能从现在或以后开始生效",
        ));
    }
    Ok(())
}

fn check_card(card: &RateCard) -> Result<(), ApiError> {
    card.validate()
        .map_err(|e| refused("/card", format!("价目卡不合法：{e}")))
}

async fn tenant_exists(conn: &mut PgConnection, id: Uuid) -> Result<bool, DbError> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
            .bind(id)
            .fetch_one(conn)
            .await?,
    )
}

/// Tells every cached price book to reload. It also serializes concurrent
/// price writes on this one row.
async fn bump_pricing_version(conn: &mut PgConnection) -> Result<(), DbError> {
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1), updated_at = now() \
         WHERE key = 'pricing_version'",
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// The operator a revision is written by; `None` for the control plane
/// itself (and anyone who is not an operator, who the audit row names).
fn created_by(actor: Actor<'_>) -> Option<Uuid> {
    match actor {
        Actor::Operator(op) => Some(op.id),
        _ => None,
    }
}

/// Writes a sell revision starting at `row.effective_from`, refusing with
/// `422` one that starts in the past (`/effective_from`), a card that does
/// not validate or a platform price without one (`/card`), a model that is
/// not live (`/model_id`) and an unknown tenant (`/tenant_id`).
pub async fn insert_sell_price(
    conn: &mut PgConnection,
    row: &NewSellPrice,
    actor: Actor<'_>,
) -> Result<SellRevision, ApiError> {
    starts_now_or_later(&mut *conn, row.effective_from).await?;
    match (&row.card, row.tenant_id) {
        (Some(card), _) => check_card(card)?,
        (None, Some(_)) => {}
        (None, None) => {
            return Err(refused(
                "/card",
                "平台价格必须带价目卡；只有租户专属价格可以不带价目卡来结束",
            ))
        }
    }
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM gw_resources WHERE kind = 'models' AND id = $1 AND NOT deleted)",
    )
    .bind(row.model_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if !live {
        return Err(refused("/model_id", "模型不存在"));
    }
    if let Some(tenant) = row.tenant_id {
        if !tenant_exists(&mut *conn, tenant).await? {
            return Err(refused("/tenant_id", "租户不存在"));
        }
    }
    let revision: SellRevision = sqlx::query_as(
        "INSERT INTO pricing_sell_revisions \
         (engine_version, model_id, tenant_id, effective_from, card, source, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING revision_id, engine_version, model_id, tenant_id, effective_from, card, source, \
                   created_by, created_at",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(row.model_id)
    .bind(row.tenant_id)
    .bind(row.effective_from)
    .bind(row.card.as_ref().map(Json))
    .bind(&row.source)
    .bind(created_by(actor))
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    bump_pricing_version(&mut *conn).await?;
    audit::record(
        conn,
        AuditEntry {
            actor,
            action: if row.card.is_some() {
                "pricing.sell.create"
            } else {
                "pricing.sell.end_override"
            },
            target_kind: "pricing_sell_revision",
            target_id: Some(revision.revision_id.to_string()),
            tenant_id: row.tenant_id,
            detail: json!({
                "model_id": row.model_id,
                "effective_from": row.effective_from,
                "source": row.source,
            }),
        },
    )
    .await?;
    Ok(revision)
}

/// Writes a cost revision starting at `row.effective_from`, refusing with
/// `422` one that starts in the past (`/effective_from`), a card that does
/// not validate (`/card`) and a provider key that is not live
/// (`/provider_key_id`).
pub async fn insert_cost_price(
    conn: &mut PgConnection,
    row: &NewCostPrice,
    actor: Actor<'_>,
) -> Result<CostRevision, ApiError> {
    starts_now_or_later(&mut *conn, row.effective_from).await?;
    check_card(&row.card)?;
    if let Some(key) = row.provider_key_id {
        // A revision for a key that does not exist would never apply.
        let live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gw_resources \
                            WHERE kind = 'provider_keys' AND id = $1 AND NOT deleted)",
        )
        .bind(key)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
        if !live {
            return Err(refused("/provider_key_id", "供应商密钥不存在"));
        }
    }
    let revision: CostRevision = sqlx::query_as(
        "INSERT INTO pricing_cost_revisions \
         (engine_version, upstream_provider, upstream_model, provider_key_id, effective_from, \
          card, source, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING revision_id, engine_version, upstream_provider, upstream_model, \
                   provider_key_id, effective_from, card, source, created_by, created_at",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(&row.upstream_provider)
    .bind(&row.upstream_model)
    .bind(row.provider_key_id)
    .bind(row.effective_from)
    .bind(Json(&row.card))
    .bind(&row.source)
    .bind(created_by(actor))
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    bump_pricing_version(&mut *conn).await?;
    audit::record(
        conn,
        AuditEntry {
            actor,
            action: "pricing.cost.create",
            target_kind: "pricing_cost_revision",
            target_id: Some(revision.revision_id.to_string()),
            tenant_id: None,
            detail: json!({
                "upstream_provider": row.upstream_provider,
                "upstream_model": row.upstream_model,
                "provider_key_id": row.provider_key_id,
                "effective_from": row.effective_from,
                "source": row.source,
            }),
        },
    )
    .await?;
    Ok(revision)
}

/// Writes a discount revision starting at `row.effective_from`, refusing
/// with `422` one that starts in the past (`/effective_from`), more than
/// 10000 basis points (`/discount_bps`) and an unknown tenant
/// (`/tenant_id`).
pub async fn insert_discount(
    conn: &mut PgConnection,
    row: &NewDiscount,
    actor: Actor<'_>,
) -> Result<DiscountRevision, ApiError> {
    starts_now_or_later(&mut *conn, row.effective_from).await?;
    if row.discount_bps > 10_000 {
        return Err(refused("/discount_bps", "折扣必须在 0 到 10000 个基点之间"));
    }
    if !tenant_exists(&mut *conn, row.tenant_id).await? {
        return Err(refused("/tenant_id", "租户不存在"));
    }
    let revision: DiscountRevision = sqlx::query_as(
        "INSERT INTO pricing_discount_revisions \
         (engine_version, tenant_id, effective_from, discount_bps, created_by) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING revision_id, engine_version, tenant_id, effective_from, discount_bps, \
                   created_by, created_at",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(row.tenant_id)
    .bind(row.effective_from)
    .bind(i64::from(row.discount_bps))
    .bind(created_by(actor))
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    bump_pricing_version(&mut *conn).await?;
    audit::record(
        conn,
        AuditEntry {
            actor,
            action: "pricing.discount.create",
            target_kind: "pricing_discount_revision",
            target_id: Some(revision.revision_id.to_string()),
            tenant_id: Some(row.tenant_id),
            detail: json!({
                "effective_from": row.effective_from,
                "discount_bps": row.discount_bps,
            }),
        },
    )
    .await?;
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_version_no_u32_holds_reads_as_the_newest() {
        assert_eq!(engine_version(1), 1);
        assert_eq!(engine_version(i64::from(u32::MAX)), u32::MAX);
        assert_eq!(engine_version(i64::from(u32::MAX) + 1), u32::MAX);
        assert_eq!(engine_version(i64::MAX), u32::MAX);
        assert_eq!(engine_version(-1), u32::MAX);
        assert_eq!(engine_version(i64::MIN), u32::MAX);
    }

    #[test]
    fn every_charge_error_has_its_stored_code() {
        let codes: Vec<&str> = [
            ChargeError::Pricing(PricingError::MissingDimensionPrice {
                dimension: aisix_pricing::Dimension::WebSearch,
            }),
            ChargeError::Pricing(PricingError::UnknownTenant),
            ChargeError::Undecodable,
            ChargeError::UnknownApiKey,
            ChargeError::TenantMismatch,
        ]
        .iter()
        .map(ChargeError::code)
        .collect();
        assert_eq!(
            codes,
            [
                "missing_dimension_price",
                "unknown_tenant",
                "undecodable_usage",
                "unknown_api_key",
                "tenant_mismatch"
            ]
        );
    }

    #[test]
    fn an_identity_too_long_to_index_is_not_copied() {
        let id = "k".repeat(MAX_ID_CHARS);
        assert_eq!(id_column(&id), id);
        assert_eq!(id_column(&format!("{id}k")), "");
        // Counted in characters, as intake counts its filter columns.
        let wide = "é".repeat(MAX_ID_CHARS);
        assert_eq!(id_column(&wide), wide);
    }
}
