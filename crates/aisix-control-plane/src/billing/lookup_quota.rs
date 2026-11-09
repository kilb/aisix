//! A key's quota at every level: `GET /dp/v1/keys/{key_id}/quota` for
//! gateways, which answer a caller's own-key lookup with it, and
//! `GET /api/keys/{id}/quota` for the portal.
//!
//! The quotas that apply to a key are the organization's (`team`), its
//! member's and its own, and the organization's balance or credit. A request
//! is refused when any of them is used up, so the smallest remaining amount
//! binds ([`KeyQuota::effective_remaining_micro_cny`]), and the level that
//! holds it is [`KeyQuota::binding_level`]. The effective remaining is
//! computed from every applicable level whatever the visibility; what the
//! holder may see of the organization is [`Visibility`]'s. The answer
//! carries no channel information.
//!
//! These are lookups on routes of their own: they never touch model traffic.

use axum::extract::{Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::access::{key_access, session_visibility, KeyAccess, Visibility};
use super::budgets::{self, BudgetRow, BudgetScope, Kind, SetBy};
use super::money::{cny_string, CURRENCY};
use crate::auth::{MemberAuth, MemberRole};
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::ApiError;
use crate::AppState;
use aisix_core::models::BudgetPeriod;

/// The level a figure belongs to, in the order they are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Organization,
    Member,
    Key,
}

/// The organization's money, shown to owners, admins and billing members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrgFigures {
    pub billing_mode: String,
    pub balance_micro_cny: i64,
    pub credit_limit_micro_cny: i64,
    /// The balance for a prepaid tenant, the balance plus the credit limit
    /// for a postpaid one.
    pub available_micro_cny: i64,
}

/// One quota that applies to the key, with its spend now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaFigure {
    pub id: Uuid,
    pub scope: BudgetScope,
    pub set_by: SetBy,
    pub kind: Kind,
    pub period: Option<&'static str>,
    pub limit_micro_cny: i64,
    pub spent_micro_cny: i64,
    pub remaining_micro_cny: i64,
    pub period_start: Option<DateTime<Utc>>,
    pub counted_from: Option<DateTime<Utc>>,
    pub exhausted: bool,
    pub created_at: DateTime<Utc>,
}

/// What applies to one key, now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyQuota {
    #[serde(skip)]
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub as_of: DateTime<Utc>,
    pub org: Option<OrgFigures>,
    pub quotas: Vec<QuotaFigure>,
    /// The smallest remaining of every level that applies, in micro-CNY.
    /// The organization's balance and every quota's remaining are the same
    /// unit, so the comparison is exact at any conversion rate.
    pub effective_remaining_micro_cny: Option<i64>,
    pub binding_level: Option<Level>,
}

fn level_of(scope: BudgetScope) -> Level {
    match scope {
        BudgetScope::Team => Level::Organization,
        BudgetScope::Member => Level::Member,
        BudgetScope::ApiKey => Level::Key,
    }
}

fn figure(b: &BudgetRow, spent: i64, now: DateTime<Utc>) -> QuotaFigure {
    QuotaFigure {
        id: b.id,
        scope: b.scope,
        set_by: b.set_by,
        kind: b.kind,
        period: b.period.map(BudgetPeriod::as_str),
        limit_micro_cny: b.limit_micro_cny,
        spent_micro_cny: spent,
        remaining_micro_cny: b.limit_micro_cny.saturating_sub(spent),
        period_start: b.period.map(|p| p.start_of(now)),
        counted_from: b.counted_from,
        exhausted: spent >= b.limit_micro_cny,
        created_at: b.created_at,
    }
}

/// Every quota level that applies to the key of `access`, as `vis` shows it.
pub async fn key_quota(
    conn: &mut PgConnection,
    access: &KeyAccess,
    vis: Visibility,
) -> Result<KeyQuota, DbError> {
    let now = budgets::db_now(&mut *conn).await?;
    let (billing_mode, balance, credit_limit): (String, i64, i64) = sqlx::query_as(
        "SELECT t.billing_mode, COALESCE(a.balance_micro_cny, 0), \
                COALESCE(a.credit_limit_micro_cny, 0) \
         FROM tenants t LEFT JOIN billing_accounts a ON a.tenant_id = t.id WHERE t.id = $1",
    )
    .bind(access.tenant_id)
    .fetch_one(&mut *conn)
    .await?;
    let available = if billing_mode == "postpaid" {
        balance.saturating_add(credit_limit)
    } else {
        balance
    };

    let all = budgets::list(&mut *conn, access.tenant_id, now).await?;
    let applicable: Vec<(BudgetRow, i64)> = all
        .into_iter()
        .filter(|(b, _)| match b.scope {
            BudgetScope::Team => b.scope_ref == access.tenant_id,
            BudgetScope::Member => Some(b.scope_ref) == access.member_id,
            BudgetScope::ApiKey => b.scope_ref == access.key_id,
        })
        .collect();

    // Each level's remaining, from every applicable figure whatever `vis`.
    let mut remaining: [Option<i64>; 3] = [None; 3];
    let mut lower = |level: Level, amount: i64| {
        let slot = &mut remaining[level as usize];
        let amount = amount.max(0);
        *slot = Some(slot.map_or(amount, |s| s.min(amount)));
    };
    if !access.is_internal {
        lower(Level::Organization, available);
    }
    for (b, spent) in &applicable {
        lower(level_of(b.scope), b.limit_micro_cny.saturating_sub(*spent));
    }
    let effective = remaining.iter().flatten().copied().min();
    let binding_level = effective.and_then(|min| {
        [Level::Organization, Level::Member, Level::Key]
            .into_iter()
            .find(|l| remaining[*l as usize] == Some(min))
    });

    let org = (vis == Visibility::Full && !access.is_internal).then_some(OrgFigures {
        billing_mode,
        balance_micro_cny: balance,
        credit_limit_micro_cny: credit_limit,
        available_micro_cny: available,
    });
    let quotas = applicable
        .iter()
        .filter(|(b, _)| vis == Visibility::Full || b.scope != BudgetScope::Team)
        .map(|(b, spent)| figure(b, *spent, now))
        .collect();
    Ok(KeyQuota {
        tenant_id: access.tenant_id,
        key_id: access.key_id,
        as_of: now,
        org,
        quotas,
        effective_remaining_micro_cny: effective,
        binding_level,
    })
}

/// `q` as a gateway relays it: every amount an exact decimal string, under a
/// neutral name, with the currency they are in.
///
/// The amounts are micro-CNY. `currency` is required by the gateway: without
/// it no amount is shown at all, so a caller is never handed a figure whose
/// unit it would have to guess.
fn dp_json(q: &KeyQuota) -> Value {
    let cny = |micro: i64| cny_string(micro);
    json!({
        "key_id": q.key_id,
        "as_of": q.as_of,
        "currency": CURRENCY,
        "org": q.org.as_ref().map(|o| json!({
            "billing_mode": o.billing_mode,
            "balance": cny(o.balance_micro_cny),
            "credit_limit": cny(o.credit_limit_micro_cny),
            "available": cny(o.available_micro_cny),
        })),
        "quotas": q.quotas.iter().map(|f| json!({
            "scope": f.scope,
            "set_by": f.set_by,
            "kind": f.kind,
            "period": f.period,
            "limit": cny(f.limit_micro_cny),
            "spend": cny(f.spent_micro_cny),
            "remaining": cny(f.remaining_micro_cny),
            "period_start": f.period_start,
            "counted_from": f.counted_from,
            "exhausted": f.exhausted,
        })).collect::<Vec<_>>(),
        "effective_remaining": q.effective_remaining_micro_cny.map(cny),
        "binding_level": q.binding_level,
    })
}

/// `GET /dp/v1/keys/{key_id}/quota`, with the key's own visibility.
pub async fn dp_quota(
    _dp: DpIdentity,
    State(state): State<AppState>,
    Path(key_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let key_id = Uuid::parse_str(&key_id).map_err(|_| ApiError::not_found())?;
    let found = state
        .lookups
        .tx(move |conn| {
            async move {
                match key_access(&mut *conn, key_id).await? {
                    Some(access) => Ok(Some(key_quota(conn, &access, access.visibility).await?)),
                    None => Ok::<_, DbError>(None),
                }
            }
            .boxed()
        })
        .await?;
    found
        .map(|q| Json(dp_json(&q)))
        .ok_or_else(ApiError::not_found)
}

/// `GET /api/keys/{id}/quota` → `KeyQuota` in micro-CNY, with the session's
/// visibility. An owner, admin or billing member reads any key of the
/// organization; a member reads the keys bound to it. Any other key, another
/// tenant's included, is `404`.
pub async fn portal_quota(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<KeyQuota>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Billing,
        MemberRole::Member,
    ])?;
    let key_id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let found = state
        .db
        .tx(move |conn| {
            async move {
                let Some(access) = key_access(&mut *conn, key_id).await? else {
                    return Ok::<_, DbError>(None);
                };
                let mine = access.tenant_id == auth.tenant_id
                    && (auth.role != MemberRole::Member || access.member_id == Some(auth.id));
                if !mine {
                    return Ok(None);
                }
                Ok(Some(
                    key_quota(conn, &access, session_visibility(auth.role)).await?,
                ))
            }
            .boxed()
        })
        .await?;
    found.map(Json).ok_or_else(ApiError::not_found)
}
