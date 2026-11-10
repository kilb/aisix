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
    BaselineRow, Catalogue, ChargeLine, CostRateRow, DiscountRow, Exemption, GroupRateRow, Outcome,
    PriceBook, PriceRows, PricedEvent, PricingError, PricingWarning, RevisionTable, ENGINE_VERSION,
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

/// The in-memory price book, keyed on `cp_settings.pricing_version`, and
/// the built-in catalogue the operators' API and the imports read.
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
        let book = Arc::new(load_price_book(conn).await?);
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

/// The charges stored unpriced in the last 24 hours (by when they were
/// priced), counted under each reason: the control plane's own
/// (`unknown_api_key`, `tenant_mismatch`, `undecodable_usage`) as well as
/// the engine's, and `unknown` for a charge with none. Ordered by reason.
/// `/admin/pricing/status` and the reconciliation both report this.
pub async fn unpriced_by_code(conn: &mut PgConnection) -> Result<Vec<(String, i64)>, DbError> {
    Ok(sqlx::query_as(
        "SELECT COALESCE(error_code, 'unknown'), count(*)::bigint FROM pricing_charges_cny \
         WHERE status = 'unpriced' AND priced_at >= now() - interval '24 hours' \
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(conn)
    .await?)
}

/// The table an invalid revision is in, as the alert names it.
fn table_name(table: RevisionTable) -> &'static str {
    match table {
        RevisionTable::Baseline => "pricing_baselines",
        RevisionTable::Rate => "pricing_group_rates",
        RevisionTable::Cost => "pricing_cost_rates",
        RevisionTable::Discount => "pricing_discount_revisions",
    }
}

type BaselineTableRow = (i64, i64, Uuid, DateTime<Utc>, Value);
type RateTableRow = (i64, i64, Uuid, Option<Uuid>, DateTime<Utc>, Option<i64>);
type CostRateTableRow = (i64, i64, Uuid, Option<Uuid>, DateTime<Utc>, Option<i64>);
type DiscountTableRow = (i64, i64, Uuid, DateTime<Utc>, i64);

/// Every revision of the four tables, read on `conn`, as a book. Ids are
/// written as `Uuid::to_string()`, the lowercase hyphenated form the control
/// plane projects and the gateway echoes back; cards stay raw JSON, so one
/// bad or newer card is isolated to its own revision.
///
/// The cost side is `pricing_cost_rates`. The per-dimension cost cards of
/// `pricing_cost_revisions` are no longer read: that table keeps the cards
/// written before cost became a rate, and prices nothing.
pub async fn load_price_book(conn: &mut PgConnection) -> Result<PriceBook, DbError> {
    let baselines: Vec<BaselineTableRow> = sqlx::query_as(
        "SELECT revision_id, engine_version, model_id, effective_from, card \
         FROM pricing_baselines",
    )
    .fetch_all(&mut *conn)
    .await?;
    let rates: Vec<RateTableRow> = sqlx::query_as(
        "SELECT revision_id, engine_version, group_id, model_id, effective_from, cny_per_usd_e4 \
         FROM pricing_group_rates",
    )
    .fetch_all(&mut *conn)
    .await?;
    let cost_rates: Vec<CostRateTableRow> = sqlx::query_as(
        "SELECT revision_id, engine_version, provider_key_id, model_id, effective_from, \
                cny_per_usd_e4 \
         FROM pricing_cost_rates",
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
        baselines: baselines
            .into_iter()
            .map(
                |(revision_id, engine, model_id, effective_from, card)| BaselineRow {
                    revision_id,
                    engine_version: engine_version(engine),
                    model_id: model_id.to_string(),
                    effective_from,
                    card,
                },
            )
            .collect(),
        rates: rates
            .into_iter()
            .map(
                |(revision_id, engine, group_id, model_id, effective_from, cny_per_usd_e4)| {
                    GroupRateRow {
                        revision_id,
                        engine_version: engine_version(engine),
                        group_id: group_id.to_string(),
                        model_id: model_id.map(|m| m.to_string()),
                        effective_from,
                        cny_per_usd_e4,
                    }
                },
            )
            .collect(),
        cost_rates: cost_rates
            .into_iter()
            .map(
                |(
                    revision_id,
                    engine,
                    provider_key_id,
                    model_id,
                    effective_from,
                    cny_per_usd_e4,
                )| {
                    CostRateRow {
                        revision_id,
                        engine_version: engine_version(engine),
                        provider_key_id: provider_key_id.to_string(),
                        model_id: model_id.map(|m| m.to_string()),
                        effective_from,
                        cny_per_usd_e4,
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
    Ok(PriceBook::from_rows(rows))
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
    /// The group the row's key names, as the control plane recorded it: the
    /// group the charge is filed under, whatever its outcome (a row the
    /// engine refused included), and what a re-pricing run prices a
    /// `no_group` row at (R11). `None` for a row whose key names none, or
    /// does not resolve.
    pub group_id: Option<Uuid>,
    /// The row's request id as `usage_events` stores it.
    pub request_id: Option<&'a str>,
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
    /// `cost_micro_cny`: `None` when the cost is unknown.
    cost: Option<i64>,
    /// The cost rate that costed the row, as `{kind: "account_rate", …}`.
    cost_source: Option<Value>,
    baseline_revision_id: Option<i64>,
    rate_revision_id: Option<i64>,
    cny_per_usd_e4: Option<i64>,
    rate_is_model_override: bool,
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
                cost_source: p.cost.as_ref().map(|c| {
                    json!({
                        "kind": "account_rate",
                        "revision_id": c.rate.revision_id,
                        "cny_per_usd": c.rate.cny_per_usd,
                        "model_override": c.rate.model_override,
                    })
                }),
                baseline_revision_id: p.baseline.map(|b| b.revision_id),
                rate_revision_id: p.rate.map(|r| r.revision_id),
                cny_per_usd_e4: p.rate.map(|r| i64::from(r.cny_per_usd.0)),
                rate_is_model_override: p.rate.is_some_and(|r| r.model_override),
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
                baseline_revision_id: None,
                rate_revision_id: None,
                cny_per_usd_e4: None,
                rate_is_model_override: false,
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
///
/// The cost goes to `cost_micro_cny`, NULL when it is unknown, and the rate
/// that decided it to `cost_source`. `cost_micro_usd` is not written: it
/// holds the costs of rows priced before cost became a rate.
pub async fn record_charge(
    conn: &mut PgConnection,
    ident: &ChargeIdentity<'_>,
    charge: &Result<PricedEvent, ChargeError>,
) -> Result<Recorded, DbError> {
    let c = Columns::of(charge);
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, \
          price_instant, status, charge_micro_cny, cost_micro_cny, cost_source, group_id, \
          baseline_revision_id, rate_revision_id, cny_per_usd_e4, rate_is_model_override, \
          discount_revision_id, discount_bps, service_tier, \
          requested_service_tier, long_context_above, estimated, exemption, error_code, lines, \
          warnings, engine_version, request_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, \
                 $19, $20, $21, $22, $23, $24, $25, $26, $27, $28) \
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
    // The key's group, whatever the outcome: the engine prices a row at
    // exactly this group, and a row it refused is still filed under it.
    .bind(ident.group_id)
    .bind(c.baseline_revision_id)
    .bind(c.rate_revision_id)
    .bind(c.cny_per_usd_e4)
    .bind(c.rate_is_model_override)
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
    .bind(ident.request_id)
    .fetch_optional(conn)
    .await?;
    Ok(match inserted {
        Some(_) => Recorded::Inserted,
        None => Recorded::AlreadyRecorded,
    })
}

/// A tenant discount to write, in basis points (0 to 10000).
#[derive(Debug, Clone)]
pub struct NewDiscount {
    pub tenant_id: Uuid,
    pub effective_from: DateTime<Utc>,
    pub discount_bps: u16,
}

/// One `pricing_cost_revisions` row: a per-dimension cost card written
/// before cost became a rate. The table is read only by
/// `GET /admin/pricing/cost`, for its history, and prices nothing.
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
pub(crate) fn refused(path: &str, message: impl Into<String>) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.into(),
    }])
}

/// Refuses a revision that would start more than [`BACKDATE_TOLERANCE`]
/// before the database's `now()`.
pub(crate) async fn starts_now_or_later(
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
pub(crate) async fn bump_pricing_version(conn: &mut PgConnection) -> Result<(), DbError> {
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
pub(crate) fn created_by(actor: Actor<'_>) -> Option<Uuid> {
    match actor {
        Actor::Operator(op) => Some(op.id),
        _ => None,
    }
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
