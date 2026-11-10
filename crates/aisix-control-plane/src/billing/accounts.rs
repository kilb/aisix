//! A tenant's billing account as people read it: its balance, its credit
//! limit, and whether the balance rule blocks it now. Operators list every
//! customer account and read or change one tenant's credit limit
//! (`/admin/billing/accounts`, `/admin/tenants/{id}/billing`); a tenant's
//! owner, admin or billing member reads its own balance
//! (`/api/billing/balance`).
//!
//! A credit limit is a money limit that moves the balance mark, so changing
//! it takes the tenant's billing lock (`tenants::lock_billing`), writes the
//! limit, settles the mark and answers the settled account, all in one
//! transaction: when the answer arrives the gateway's document already says
//! what the new limit means.

use axum::extract::{Path, State};
use axum::Json;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::marks;
use super::money::parse_cny;
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::ApiError;
use crate::http::ApiJson;
use crate::resources::write::WriteCtx;
use crate::tenants::{self, BillingMode};
use crate::AppState;

/// One tenant's billing account.
#[derive(Debug, Clone, Serialize)]
pub struct BillingAccount {
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub status: String,
    pub is_internal: bool,
    pub billing_mode: String,
    pub balance_micro_cny: i64,
    pub credit_limit_micro_cny: i64,
    /// The balance plus the credit limit, which is an overdraft in either
    /// billing mode.
    pub available_micro_cny: i64,
    /// Whether the balance rule blocks the tenant now; never for the
    /// internal tenant.
    pub exhausted: bool,
    /// The tenant's top-up requests awaiting a decision.
    pub pending_topups: i64,
}

/// The columns of a [`BillingAccount`] read, with the account's own columns
/// as zeros for a tenant that has no account yet.
const SELECT: &str = "SELECT t.id, t.name, t.status, t.is_internal, t.billing_mode, \
     COALESCE(a.balance_micro_cny, 0), COALESCE(a.credit_limit_micro_cny, 0), \
     (SELECT count(*) FROM billing_topups_cny p WHERE p.tenant_id = t.id AND p.status = 'pending') \
     FROM tenants t LEFT JOIN billing_accounts a ON a.tenant_id = t.id";

type Raw = (Uuid, String, String, bool, String, i64, i64, i64);

fn account_of(raw: Raw) -> Result<BillingAccount, ApiError> {
    let (tenant_id, tenant_name, status, is_internal, mode, balance, credit_limit, pending_topups) =
        raw;
    // A stored mode this build cannot read is a fault, even though no figure
    // here depends on it any more: the tenant view shows the same string.
    BillingMode::parse(&mode)
        .ok_or_else(|| ApiError::internal(format!("tenant {tenant_id}: billing mode {mode}")))?;
    Ok(BillingAccount {
        tenant_id,
        tenant_name,
        status,
        is_internal,
        billing_mode: mode,
        balance_micro_cny: balance,
        credit_limit_micro_cny: credit_limit,
        available_micro_cny: marks::available(balance, credit_limit),
        exhausted: !is_internal && marks::exhausted(balance, credit_limit),
        pending_topups,
    })
}

/// Tenant `id`'s account; `404 not_found` when there is no such tenant.
pub async fn find(conn: &mut PgConnection, id: Uuid) -> Result<BillingAccount, ApiError> {
    let raw: Option<Raw> = sqlx::query_as(&format!("{SELECT} WHERE t.id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?;
    account_of(raw.ok_or_else(ApiError::not_found)?)
}

/// `GET /admin/billing/accounts` → `{items: [BillingAccount]}`: every tenant
/// except pending ones, customers by name and the internal tenant last.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let raws: Vec<Raw> = sqlx::query_as(&format!(
        "{SELECT} WHERE t.status <> 'pending' ORDER BY t.is_internal, t.name, t.id"
    ))
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let items = raws
        .into_iter()
        .map(account_of)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "items": items })))
}

/// `GET /admin/tenants/{id}/billing` → `BillingAccount`.
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<BillingAccount>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    Ok(Json(find(&mut conn, id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCreditLimit {
    credit_limit_cny: String,
}

/// `PATCH /admin/tenants/{id}/billing` with `{credit_limit_cny}` → `200
/// BillingAccount`, settled: the new limit and the balance mark it decides
/// are committed together. `422 /credit_limit_cny` unless the limit is
/// yuan and fen (0 is allowed).
pub async fn set_credit_limit(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<SetCreditLimit>,
) -> Result<Json<BillingAccount>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let limit = parse_cny(&req.credit_limit_cny, "/credit_limit_cny")?;
    let ctx = WriteCtx::of(&state);
    let account = state
        .db
        .tx(move |conn| {
            async move {
                // The billing lock: the limit moves the balance mark.
                tenants::lock_billing(&mut *conn, id).await?;
                sqlx::query(
                    "UPDATE billing_accounts SET credit_limit_micro_cny = $2 WHERE tenant_id = $1",
                )
                .bind(id)
                .bind(limit)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                marks::settle_locked(&mut *conn, &ctx, id).await?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "billing.credit_limit",
                        target_kind: "tenant",
                        target_id: Some(id.to_string()),
                        tenant_id: Some(id),
                        detail: json!({ "credit_limit_micro_cny": limit }),
                    },
                )
                .await?;
                find(conn, id).await
            }
            .boxed()
        })
        .await?;
    Ok(Json(account))
}

/// `GET /api/billing/balance` → the caller's own organization's account
/// without its identity: `{billing_mode, balance_micro_cny,
/// credit_limit_micro_cny, available_micro_cny, exhausted, pending_topups}`.
pub async fn balance(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let account = find(&mut conn, auth.tenant_id).await?;
    Ok(Json(json!({
        "billing_mode": account.billing_mode,
        "balance_micro_cny": account.balance_micro_cny,
        "credit_limit_micro_cny": account.credit_limit_micro_cny,
        "available_micro_cny": account.available_micro_cny,
        "exhausted": account.exhausted,
        "pending_topups": account.pending_topups,
    })))
}
