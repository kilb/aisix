//! A tenant's ledger read two ways: its entries page by page, newest first,
//! and its totals per UTC day or month. Nothing here is stored: a summary is
//! added up from the entries when it is asked for, so it can never disagree
//! with them. Operators read any tenant's
//! (`/admin/tenants/{id}/billing/ledger`, `.../billing/summary`); a tenant's
//! owner, admin or billing member reads its own (`/api/billing/ledger`,
//! `/api/billing/summary`).
//!
//! Entries page on `seq`, which the ledger allocates in posting order.
//! Channel information stays out: a usage entry names the model the caller
//! asked for and carries no cost, provider or upstream detail.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::accounts;
use super::ledger::EntryKind;
use super::range::{checked_range, Group};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::resources::kinds::Kind;
use crate::AppState;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

#[derive(Deserialize)]
pub struct LedgerQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    kind: Option<EntryKind>,
    limit: Option<i64>,
    /// Only entries with a smaller `seq`: the `next_before` of the previous
    /// page.
    before: Option<i64>,
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    group: Group,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

fn refused<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query.map(|Query(q)| q).map_err(|_| {
        ApiError::validation(vec![Detail {
            path: "query".to_string(),
            message: "查询参数不合法".to_string(),
        }])
    })
}

type Row = (
    i64,
    DateTime<Utc>,
    String,
    i64,
    i64,
    Option<String>,
    Option<Uuid>,
    Option<Uuid>,
    // The usage entry's model: the id the charge recorded and whether the
    // tenant's grants still hold it, the name the event carried, and the
    // model's current display name.
    Option<String>,
    Option<bool>,
    Option<String>,
    Option<String>,
);

/// `tenant`'s entries, newest first, as `{items, next_before}`.
async fn ledger_of(
    conn: &mut PgConnection,
    tenant: Uuid,
    q: LedgerQuery,
) -> Result<Json<Value>, ApiError> {
    if let (Some(from), Some(to)) = (q.from, q.to) {
        checked_range(from, to, None)?;
    }
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // One row more than the page, to tell whether another page follows.
    let mut rows: Vec<Row> = sqlx::query_as(
        "SELECT l.seq, l.posted_at, l.kind, l.amount_micro_usd, l.balance_after_micro_usd, \
                l.event_id, l.topup_id, l.repricing_run_id, \
                c.requested_model_id, \
                EXISTS (SELECT 1 FROM tenants t WHERE t.id = l.tenant_id \
                          AND c.requested_model_id = ANY(t.allowed_model_ids::text[])), \
                u.event->>'requested_model', \
                (SELECT r.doc->>'display_name' FROM gw_resources r \
                  WHERE r.kind = $6 AND r.id::text = c.requested_model_id) \
         FROM billing_ledger l \
         LEFT JOIN pricing_charges c ON c.event_id = l.event_id \
         LEFT JOIN usage_events u ON u.event_id = l.event_id \
         WHERE l.tenant_id = $1 \
           AND ($2::timestamptz IS NULL OR l.posted_at >= $2) \
           AND ($3::timestamptz IS NULL OR l.posted_at < $3) \
           AND ($4::text IS NULL OR l.kind = $4) \
           AND ($5::bigint IS NULL OR l.seq < $5) \
         ORDER BY l.seq DESC LIMIT $7",
    )
    .bind(tenant)
    .bind(q.from)
    .bind(q.to)
    .bind(q.kind.map(EntryKind::as_str))
    .bind(q.before)
    .bind(Kind::Models.segment())
    .bind(limit + 1)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_before = if more { rows.last().map(|r| r.0) } else { None };
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                seq,
                posted_at,
                kind,
                amount,
                balance_after,
                event_id,
                topup_id,
                run_id,
                model_id,
                granted,
                event_name,
                current_name,
            )| {
                // Only a usage entry has a model. Its id is shown while the
                // tenant may still use the model.
                let model = (kind == "usage").then(|| {
                    let name = event_name
                        .filter(|n| !n.is_empty())
                        .or(current_name)
                        .unwrap_or_default();
                    let id = model_id.filter(|_| granted == Some(true));
                    json!({ "id": id, "name": name })
                });
                json!({
                    "seq": seq,
                    "posted_at": posted_at,
                    "kind": kind,
                    "amount_micro_usd": amount,
                    "balance_after_micro_usd": balance_after,
                    "event_id": event_id,
                    "model": model,
                    "topup_id": topup_id,
                    "repricing_run_id": run_id,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

/// `tenant`'s totals per UTC period of `[from, to)`, oldest first, as
/// `{items}`. A period with no entry is absent; the closing balance is the
/// balance after the period's last entry in the range.
async fn summary_of(
    conn: &mut PgConnection,
    tenant: Uuid,
    q: SummaryQuery,
) -> Result<Json<Value>, ApiError> {
    let (from, to) = checked_range(q.from, q.to, Some(q.group))?;
    let unit = match q.group {
        Group::Day => "day",
        Group::Month => "month",
    };
    let rows: Vec<(DateTime<Utc>, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT date_trunc($4, posted_at, 'UTC') AS period, \
                COALESCE(SUM(amount_micro_usd) FILTER (WHERE kind = 'usage'), 0)::bigint, \
                COALESCE(SUM(amount_micro_usd) FILTER (WHERE kind = 'topup'), 0)::bigint, \
                COALESCE(SUM(amount_micro_usd) FILTER (WHERE kind = 'adjustment'), 0)::bigint, \
                (array_agg(balance_after_micro_usd ORDER BY seq DESC))[1] \
         FROM billing_ledger \
         WHERE tenant_id = $1 AND posted_at >= $2 AND posted_at < $3 \
         GROUP BY period ORDER BY period",
    )
    .bind(tenant)
    .bind(from)
    .bind(to)
    .bind(unit)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(period, usage, topups, adjustments, closing)| {
            json!({
                "period_start": period,
                "usage_micro_usd": usage,
                "topups_micro_usd": topups,
                "adjustments_micro_usd": adjustments,
                "closing_balance_micro_usd": closing,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// The tenant `id` of an operator's path: `404` for a malformed or unknown
/// one.
async fn known_tenant(conn: &mut PgConnection, id: &str) -> Result<Uuid, ApiError> {
    let id = Uuid::parse_str(id).map_err(|_| ApiError::not_found())?;
    accounts::find(conn, id).await?;
    Ok(id)
}

/// `GET /admin/tenants/{id}/billing/ledger?from&to&kind&limit&before` →
/// `{items: [LedgerItem], next_before}`, newest first, `limit` (default 50,
/// at most 500) at a time; `next_before` is null on the last page.
pub async fn admin_ledger(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<LedgerQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let q = refused(query)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = known_tenant(&mut conn, &id).await?;
    ledger_of(&mut conn, tenant, q).await
}

/// `GET /admin/tenants/{id}/billing/summary?group&from&to` → `{items:
/// [LedgerSummaryRow]}`; `422 /to` past 92 days by day or 24 months by
/// month.
pub async fn admin_summary(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<SummaryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let q = refused(query)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = known_tenant(&mut conn, &id).await?;
    summary_of(&mut conn, tenant, q).await
}

/// `GET /api/billing/ledger`: the caller's own organization's entries, as
/// [`admin_ledger`].
pub async fn ledger(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<LedgerQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let q = refused(query)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    ledger_of(&mut conn, auth.tenant_id, q).await
}

/// `GET /api/billing/summary`: the caller's own organization's totals, as
/// [`admin_summary`].
pub async fn summary(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<SummaryQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let q = refused(query)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    summary_of(&mut conn, auth.tenant_id, q).await
}
