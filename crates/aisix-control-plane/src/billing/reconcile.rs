//! Reconciliation: whether the ledger matches what moved it, and the usage
//! the gateways report they dropped (`GET /admin/billing/reconciliation`).
//!
//! The ledger is the record of a tenant's money. The account's balance is a
//! cache of it, and the charges, the re-pricing adjustments and the top-ups
//! are what it is posted from, so per tenant (every billing account, or the
//! one asked for) [`check`] compares, each as "actual against expected":
//! - `balance`: the account's balance against the total of the tenant's
//!   entries;
//! - `usage`: the total of its `usage` entries against minus the total of
//!   `charge_micro_usd` over the charges filed under it as `charged` (a row
//!   charged 0 posts no entry and adds 0);
//! - `adjustments`: the total of its `adjustment` entries against minus the
//!   total of `pricing_adjustments.delta_micro_usd` over the charges filed
//!   under it;
//! - `topups`: the total of its `topup` entries against the total of its
//!   approved top-ups, operator credits included;
//! - `running_balance`: the first entry, by `seq`, whose `balance_after` is
//!   not the previous entry's (0 before the first) plus its own amount; the
//!   mismatch names that entry's `seq`.
//!
//! Beside them, three figures:
//! - `charged_without_entry`: the charges `charged` above 0 that have no
//!   entry. Such a row also shows in its tenant's `usage` total, but totals
//!   can cancel (an entry posted for a row that is not charged, beside a
//!   charged row missing its entry), and a count cannot;
//! - `unpriced_last_24h`, counted as `/admin/pricing/status` counts it;
//! - `usage_loss`: each gateway instance heard from in the last 24 hours,
//!   with the drops it reported by reason, `dropped_total` the total of every
//!   reason but `send_unconfirmed` and `unconfirmed_total` that reason's. An
//!   unconfirmed batch may have been stored, and drops a process counted
//!   after its last heartbeat end with it, so this is what is visibly lost,
//!   not all of it.
//!
//! It only reads. The route runs it in one read-only `REPEATABLE READ`
//! transaction, so every check sees one snapshot, and takes no tenant,
//! account or advisory lock, so it never holds up intake or a money path.
//! All of its reads together are bounded by [`TIME_BOUND`], past which the
//! route answers `503`: a reconciliation holds a connection of the pool
//! intake shares, and its tables' `ACCESS SHARE` locks, for as long as it
//! runs.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::pricing::store;
use crate::usage::query_rejected;
use crate::AppState;

/// How long one reconciliation may take, all of its reads together. They
/// read whole tables.
const TIME_BOUND: Duration = Duration::from_secs(10);

/// The drop reason for a batch the gateway sent without learning whether
/// it was stored.
const UNCONFIRMED: &str = "send_unconfirmed";

/// How recently an instance must have reported for `usage_loss` to count
/// its drops, as a Postgres interval. The instance cap never evicts an
/// instance whose drops it still counts (`dp::heartbeat`).
pub const USAGE_LOSS_WINDOW: &str = "interval '24 hours'";

/// A per-tenant check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Balance,
    Usage,
    Adjustments,
    Topups,
    RunningBalance,
}

/// A per-tenant check that does not hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mismatch {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub check: Check,
    pub expected_micro_usd: i64,
    pub actual_micro_usd: i64,
    /// The entry that breaks the running balance; `None` for the other
    /// checks.
    pub seq: Option<i64>,
}

/// The charges stored unpriced in the last 24 hours, by reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unpriced {
    pub total: i64,
    pub by_code: BTreeMap<String, i64>,
}

/// One gateway instance heard from in the last 24 hours, and the usage
/// events it reported in its last heartbeat as dropped since its process
/// started, by reason.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct InstanceDrops {
    pub dp_id: Uuid,
    pub credential_name: String,
    pub instance_id: String,
    pub hostname: String,
    pub last_seen_at: DateTime<Utc>,
    pub drops: Value,
}

/// The usage the gateways heard from since `since` report they dropped.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageLoss {
    pub since: DateTime<Utc>,
    pub dropped_total: i64,
    pub unconfirmed_total: i64,
    pub instances: Vec<InstanceDrops>,
}

/// What a reconciliation found, as of `checked_at`, for tenant `tenant_id`
/// or, when it is `None`, for every tenant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reconciliation {
    pub checked_at: DateTime<Utc>,
    pub tenant_id: Option<Uuid>,
    /// By tenant name, then tenant, then check.
    pub mismatches: Vec<Mismatch>,
    /// The tenant's when one is asked for.
    pub charged_without_entry: i64,
    /// Every tenant's, whichever is asked for.
    pub unpriced_last_24h: Unpriced,
    /// Every gateway's, whichever tenant is asked for.
    pub usage_loss: UsageLoss,
}

/// `condition` (on `$1`, the tenant) when one tenant is checked; `true`
/// when every tenant is.
fn only(tenant_id: Option<Uuid>, condition: &str) -> &str {
    if tenant_id.is_some() {
        condition
    } else {
        "true"
    }
}

/// The time left of a reconciliation's [`TIME_BOUND`], set as the
/// statement timeout of its next read: at least 1 ms, so a spent budget
/// cancels the next read (SQLSTATE `57014`), and never more than the
/// connection's own `statement_timeout`, `session_ms` (0 for none).
/// `set_config(…, true)` is local to the transaction, as `SET LOCAL` is.
async fn bound(conn: &mut PgConnection, deadline: Instant, session_ms: i64) -> Result<(), DbError> {
    let mut left = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .max(1);
    if let Ok(session_ms @ 1..) = u128::try_from(session_ms) {
        left = left.min(session_ms);
    }
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(format!("{left}ms"))
        .execute(conn)
        .await?;
    Ok(())
}

/// What [`check`] finds, for tenant `tenant_id` or every tenant, within
/// [`TIME_BOUND`] (`57014` past it). Run it in one `REPEATABLE READ`
/// transaction, so every check reads one snapshot.
pub async fn check(
    conn: &mut PgConnection,
    tenant_id: Option<Uuid>,
) -> Result<Reconciliation, DbError> {
    let deadline = Instant::now() + TIME_BOUND;
    let (checked_at, since, session_ms): (DateTime<Utc>, DateTime<Utc>, i64) =
        sqlx::query_as(&format!(
            "SELECT now(), now() - {USAGE_LOSS_WINDOW}, \
               (SELECT reset_val::bigint FROM pg_settings WHERE name = 'statement_timeout')"
        ))
        .fetch_one(&mut *conn)
        .await?;
    bound(&mut *conn, deadline, session_ms).await?;
    let mut mismatches = totals(&mut *conn, tenant_id).await?;
    bound(&mut *conn, deadline, session_ms).await?;
    mismatches.extend(running_balances(&mut *conn, tenant_id).await?);
    mismatches.sort_by(|a, b| {
        (&a.tenant_name, a.tenant_id, a.check).cmp(&(&b.tenant_name, b.tenant_id, b.check))
    });
    bound(&mut *conn, deadline, session_ms).await?;
    let charged_without_entry = charged_without_entry(&mut *conn, tenant_id).await?;
    bound(&mut *conn, deadline, session_ms).await?;
    let unpriced_last_24h = unpriced(&mut *conn).await?;
    let usage_loss = usage_loss(conn, since, deadline, session_ms).await?;
    Ok(Reconciliation {
        checked_at,
        tenant_id,
        mismatches,
        charged_without_entry,
        unpriced_last_24h,
        usage_loss,
    })
}

type Totals = (Uuid, String, i64, i64, i64, i64, i64, i64, i64, i64);

/// The `balance`, `usage`, `adjustments` and `topups` mismatches. Charges
/// are filed under a tenant id as text, so a charge is the tenant's when
/// that text is the tenant's id.
async fn totals(
    conn: &mut PgConnection,
    tenant_id: Option<Uuid>,
) -> Result<Vec<Mismatch>, DbError> {
    let sql = format!(
        "WITH accounts AS ( \
           SELECT a.tenant_id, t.name, a.balance_micro_usd \
           FROM billing_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE {accounts}), \
         entries AS ( \
           SELECT l.tenant_id, SUM(l.amount_micro_usd)::bigint AS total, \
             COALESCE(SUM(l.amount_micro_usd) FILTER (WHERE l.kind = 'usage'), 0)::bigint \
               AS usage, \
             COALESCE(SUM(l.amount_micro_usd) FILTER (WHERE l.kind = 'adjustment'), 0)::bigint \
               AS adjustment, \
             COALESCE(SUM(l.amount_micro_usd) FILTER (WHERE l.kind = 'topup'), 0)::bigint \
               AS topup \
           FROM billing_ledger l WHERE {ledger} GROUP BY l.tenant_id), \
         charged AS ( \
           SELECT pc.tenant_id, SUM(pc.charge_micro_usd)::bigint AS total \
           FROM pricing_charges pc WHERE pc.status = 'charged' AND {charges} \
           GROUP BY pc.tenant_id), \
         adjusted AS ( \
           SELECT pc.tenant_id, SUM(pa.delta_micro_usd)::bigint AS total \
           FROM pricing_adjustments pa JOIN pricing_charges pc ON pc.event_id = pa.event_id \
           WHERE {charges} GROUP BY pc.tenant_id), \
         approved AS ( \
           SELECT p.tenant_id, SUM(p.amount_micro_usd)::bigint AS total \
           FROM billing_topups p WHERE p.status = 'approved' AND {topups} \
           GROUP BY p.tenant_id) \
         SELECT a.tenant_id, a.name, a.balance_micro_usd, COALESCE(e.total, 0), \
           COALESCE(e.usage, 0), COALESCE(e.adjustment, 0), COALESCE(e.topup, 0), \
           COALESCE(c.total, 0), COALESCE(j.total, 0), COALESCE(p.total, 0) \
         FROM accounts a \
         LEFT JOIN entries e ON e.tenant_id = a.tenant_id \
         LEFT JOIN charged c ON c.tenant_id = a.tenant_id::text \
         LEFT JOIN adjusted j ON j.tenant_id = a.tenant_id::text \
         LEFT JOIN approved p ON p.tenant_id = a.tenant_id",
        accounts = only(tenant_id, "a.tenant_id = $1"),
        ledger = only(tenant_id, "l.tenant_id = $1"),
        charges = only(tenant_id, "pc.tenant_id = $1::text"),
        topups = only(tenant_id, "p.tenant_id = $1"),
    );
    let mut query = sqlx::query_as::<_, Totals>(&sql);
    if let Some(id) = tenant_id {
        query = query.bind(id);
    }
    let rows = query.fetch_all(conn).await?;
    let mut mismatches = Vec::new();
    for (
        tenant_id,
        name,
        balance,
        entries,
        usage,
        adjustment,
        topup,
        charged,
        adjusted,
        approved,
    ) in rows
    {
        // A charge or an adjustment raises what the tenant owes, which a
        // debit enters below 0.
        for (check, expected, actual) in [
            (Check::Balance, entries, balance),
            (Check::Usage, charged.saturating_neg(), usage),
            (Check::Adjustments, adjusted.saturating_neg(), adjustment),
            (Check::Topups, approved, topup),
        ] {
            if expected != actual {
                mismatches.push(Mismatch {
                    tenant_id,
                    tenant_name: name.clone(),
                    check,
                    expected_micro_usd: expected,
                    actual_micro_usd: actual,
                    seq: None,
                });
            }
        }
    }
    Ok(mismatches)
}

/// Each tenant's first entry whose `balance_after` is not the previous
/// entry's, as stored, plus its own amount.
async fn running_balances(
    conn: &mut PgConnection,
    tenant_id: Option<Uuid>,
) -> Result<Vec<Mismatch>, DbError> {
    let sql = format!(
        "SELECT DISTINCT ON (r.tenant_id) r.tenant_id, t.name, r.seq, r.expected, \
           r.balance_after_micro_usd \
         FROM (SELECT l.tenant_id, l.seq, l.balance_after_micro_usd, \
                 COALESCE(LAG(l.balance_after_micro_usd) \
                   OVER (PARTITION BY l.tenant_id ORDER BY l.seq), 0) + l.amount_micro_usd \
                   AS expected \
               FROM billing_ledger l WHERE {ledger}) r \
         JOIN tenants t ON t.id = r.tenant_id \
         WHERE r.expected <> r.balance_after_micro_usd \
         ORDER BY r.tenant_id, r.seq",
        ledger = only(tenant_id, "l.tenant_id = $1"),
    );
    let mut query = sqlx::query_as::<_, (Uuid, String, i64, i64, i64)>(&sql);
    if let Some(id) = tenant_id {
        query = query.bind(id);
    }
    Ok(query
        .fetch_all(conn)
        .await?
        .into_iter()
        .map(|(tenant_id, tenant_name, seq, expected, actual)| Mismatch {
            tenant_id,
            tenant_name,
            check: Check::RunningBalance,
            expected_micro_usd: expected,
            actual_micro_usd: actual,
            seq: Some(seq),
        })
        .collect())
}

async fn charged_without_entry(
    conn: &mut PgConnection,
    tenant_id: Option<Uuid>,
) -> Result<i64, DbError> {
    let sql = format!(
        "SELECT count(*) FROM pricing_charges pc \
         WHERE pc.status = 'charged' AND pc.charge_micro_usd > 0 AND {charges} \
           AND NOT EXISTS (SELECT 1 FROM billing_ledger l WHERE l.event_id = pc.event_id)",
        charges = only(tenant_id, "pc.tenant_id = $1::text"),
    );
    let mut query = sqlx::query_scalar::<_, i64>(&sql);
    if let Some(id) = tenant_id {
        query = query.bind(id);
    }
    Ok(query.fetch_one(conn).await?)
}

/// The charges stored unpriced in the last 24 hours, by reason, counted as
/// `/admin/pricing/status` counts them ([`store::unpriced_by_code`]).
async fn unpriced(conn: &mut PgConnection) -> Result<Unpriced, DbError> {
    let by_code: BTreeMap<String, i64> = store::unpriced_by_code(conn).await?.into_iter().collect();
    Ok(Unpriced {
        total: by_code.values().sum(),
        by_code,
    })
}

/// The instances heard from since `since`, and their drops added up.
///
/// A gateway counts drops in a u64 and reports each count as it is, and the
/// heartbeat stores any count up to `u64::MAX`, so a `bigint` total could
/// overflow and fail the route. The totals are added up as `numeric` and
/// saturate at `i64::MAX` as they are read. Each read is [`bound`].
async fn usage_loss(
    conn: &mut PgConnection,
    since: DateTime<Utc>,
    deadline: Instant,
    session_ms: i64,
) -> Result<UsageLoss, DbError> {
    bound(&mut *conn, deadline, session_ms).await?;
    let instances: Vec<InstanceDrops> = sqlx::query_as(
        "SELECT i.dp_id, c.name AS credential_name, i.instance_id, i.hostname, i.last_seen_at, \
           i.usage_event_drops AS drops \
         FROM dp_instances i JOIN dp_credentials c ON c.dp_id = i.dp_id \
         WHERE i.last_seen_at >= $1 ORDER BY c.name, i.instance_id",
    )
    .bind(since)
    .fetch_all(&mut *conn)
    .await?;
    bound(&mut *conn, deadline, session_ms).await?;
    let (dropped_total, unconfirmed_total): (i64, i64) = sqlx::query_as(
        "SELECT \
           LEAST(COALESCE(SUM(CASE WHEN d.key = $2 THEN 0 ELSE d.value::numeric END)::numeric, \
                          0), $3)::bigint, \
           LEAST(COALESCE(SUM(CASE WHEN d.key = $2 THEN d.value::numeric ELSE 0 END)::numeric, \
                          0), $3)::bigint \
         FROM dp_instances i CROSS JOIN LATERAL jsonb_each(i.usage_event_drops) AS d(key, value) \
         WHERE i.last_seen_at >= $1",
    )
    .bind(since)
    .bind(UNCONFIRMED)
    .bind(i64::MAX)
    .fetch_one(conn)
    .await?;
    Ok(UsageLoss {
        since,
        dropped_total,
        unconfirmed_total,
        instances,
    })
}

#[derive(Deserialize)]
pub struct ReconcileQuery {
    tenant_id: Option<Uuid>,
}

/// `GET /admin/billing/reconciliation[?tenant_id]` → `Reconciliation`
/// ([`check`]); `404 not_found` for a tenant id that names no tenant, rather
/// than an answer with nothing to report; `503 unavailable` when the reads
/// run past [`TIME_BOUND`]. The tenant lookup before them is one row by its
/// key, bounded by the connection's own timeouts as any route's read is.
pub async fn reconciliation(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<ReconcileQuery>, QueryRejection>,
) -> Result<Json<Reconciliation>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let tenant_id = q.tenant_id;
    let found = state
        .db
        .tx(move |conn| {
            async move {
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                    .execute(&mut *conn)
                    .await?;
                if let Some(id) = tenant_id {
                    let known: bool =
                        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
                            .bind(id)
                            .fetch_one(&mut *conn)
                            .await?;
                    if !known {
                        return Ok(None);
                    }
                }
                check(conn, tenant_id).await.map(Some)
            }
            .boxed()
        })
        .await
        .map_err(api_error)?;
    found.map(Json).ok_or_else(ApiError::not_found)
}

/// A read cancelled by its statement timeout (SQLSTATE `57014`), which
/// [`bound`] sets from [`TIME_BOUND`], is `503`, as a lock wait past
/// `lock_timeout` is: the same request can succeed once the database is less
/// busy.
fn api_error(e: DbError) -> ApiError {
    match &e {
        DbError::Db(sqlx::Error::Database(db)) if db.code().as_deref() == Some("57014") => {
            tracing::warn!("a reconciliation ran past its time bound; answered 503");
            ApiError::unavailable()
        }
        _ => ApiError::from(e),
    }
}
