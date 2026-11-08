//! An organization's quota, set by operators: the `team` level, which caps
//! every key of one tenant (`/admin/tenants/{id}/budgets`).
//!
//! An operator reads every level of the tenant's quotas, and creates, edits
//! and deletes the ones an operator set (`set_by 'operator'`). A quota set at
//! another level answers `403 budget_managed`, and another tenant's quota
//! answers `404`, like a random id. The writes are [`budgets`]'s, which take
//! the tenant's billing lock, settle the gateway's documents inline and
//! record the audit row, so the answer already stands in the gateway's
//! documents.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::budgets::{self, BudgetRow, BudgetScope, Kind, NewBudget, SetBy};
use super::money::parse_usd;
use crate::audit::Actor;
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::write::WriteCtx;
use crate::tenants::{self, TenantRow};
use crate::AppState;
use aisix_core::models::BudgetPeriod;

/// The label and liveness of each target of `rows`: the tenant's name, a
/// member's display name or email, a key's label. A target that is gone
/// keeps its label and is not live; one that is not found at all reads as
/// not live with an empty label.
async fn targets(
    conn: &mut PgConnection,
    tenant: &TenantRow,
    rows: &[BudgetRow],
) -> Result<HashMap<Uuid, (String, bool)>, DbError> {
    let mut found = HashMap::new();
    found.insert(tenant.id, (tenant.name.clone(), true));
    for (scope, sql) in [
        (
            BudgetScope::Member,
            "SELECT id, COALESCE(display_name, email), removed_at IS NULL FROM members \
             WHERE tenant_id = $1 AND id = ANY($2)",
        ),
        (
            BudgetScope::ApiKey,
            "SELECT id, label, revoked_at IS NULL FROM tenant_keys \
             WHERE tenant_id = $1 AND id = ANY($2)",
        ),
    ] {
        let ids: Vec<Uuid> = rows
            .iter()
            .filter(|b| b.scope == scope)
            .map(|b| b.scope_ref)
            .collect();
        if ids.is_empty() {
            continue;
        }
        let named: Vec<(Uuid, String, bool)> = sqlx::query_as(sql)
            .bind(tenant.id)
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
        found.extend(
            named
                .into_iter()
                .map(|(id, label, live)| (id, (label, live))),
        );
    }
    Ok(found)
}

/// Quota `b` as the API shows it, with its target from `found`.
fn api(
    b: &BudgetRow,
    spent: i64,
    now: DateTime<Utc>,
    found: &HashMap<Uuid, (String, bool)>,
) -> Value {
    let (label, live) = found
        .get(&b.scope_ref)
        .map_or(("", false), |(label, live)| (label.as_str(), *live));
    budgets::to_api(b, spent, now, label, live)
}

/// `GET /admin/tenants/{id}/budgets` → `{items: [Budget]}`: every live quota
/// of the tenant at every level, oldest first, each with its spend now.
/// `404` for no such tenant.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = tenants::find(&mut conn, id).await?;
    let now = budgets::db_now(&mut conn).await?;
    let rows = budgets::list(&mut conn, id, now).await?;
    let plain: Vec<BudgetRow> = rows.iter().map(|(b, _)| b.clone()).collect();
    let found = targets(&mut conn, &tenant, &plain).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|(b, spent)| api(b, *spent, now, &found))
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewQuota {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    period: Option<String>,
    limit_usd: String,
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.to_string(),
    }])
}

/// `POST /admin/tenants/{id}/budgets` with `{kind?, period?, limit_usd}` →
/// `201 Budget`: the organization's quota (scope `team`, `set_by
/// "operator"`), settled before the answer. `kind` is `periodic` (the
/// default) or `cumulative`; a periodic quota's `period` is `day`, `week` or
/// `month` (the default), and a cumulative one has none. `422` at `/kind`,
/// `/period`, `/limit_usd` (dollars and cents above 0) and, for the internal
/// tenant, which takes no quota, at `/tenant_id`; `409 budget_exists` when
/// the tenant already has an operator quota of that kind and period; `404`
/// for no such tenant.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<NewQuota>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    let mut problems = Vec::new();
    let kind = match req.kind.as_deref() {
        None | Some("periodic") => Kind::Periodic,
        Some("cumulative") => Kind::Cumulative,
        Some(_) => {
            problems.push(Detail {
                path: "/kind".into(),
                message: "额度类型应为 periodic 或 cumulative".into(),
            });
            Kind::Periodic
        }
    };
    let period = match (kind, req.period.as_deref()) {
        (Kind::Periodic, None) => Some(BudgetPeriod::Month),
        (_, None) => None,
        (_, Some(p)) => match [BudgetPeriod::Day, BudgetPeriod::Week, BudgetPeriod::Month]
            .into_iter()
            .find(|v| v.as_str() == p)
        {
            Some(v) => Some(v),
            None => {
                problems.push(Detail {
                    path: "/period".into(),
                    message: "周期应为 day、week 或 month".into(),
                });
                None
            }
        },
    };
    let limit = parse_usd(&req.limit_usd, "/limit_usd").unwrap_or_else(|e| {
        problems.extend(e.details);
        0
    });
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let ctx = WriteCtx::of(&state);
    let (row, spent, now, found) = state
        .db
        .tx(move |conn| {
            async move {
                // The billing lock, as the domain write takes it too.
                let (tenant, _) = tenants::lock_billing(&mut *conn, id).await?;
                let new = NewBudget {
                    scope: BudgetScope::Team,
                    scope_ref: id,
                    set_by: SetBy::Operator,
                    kind,
                    period,
                    limit_micro_usd: limit,
                    created_by_kind: "operator",
                    created_by: Some(auth.id),
                };
                let row = budgets::create(&mut *conn, &ctx, &tenant, new, Actor::Operator(&auth))
                    .await
                    .map_err(|e| {
                        // The only target an operator names is the tenant,
                        // which only the internal one fails.
                        if e.details.iter().any(|d| d.path == "/scope_ref") {
                            invalid("/tenant_id", "内部租户不设额度")
                        } else {
                            e
                        }
                    })?;
                let now = budgets::db_now(&mut *conn).await?;
                let spent = budgets::spent(&mut *conn, &row, now).await?;
                let found = targets(&mut *conn, &tenant, std::slice::from_ref(&row)).await?;
                Ok::<_, ApiError>((row, spent, now, found))
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(api(&row, spent, now, &found))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetLimit {
    limit_usd: String,
}

/// An operator's own quota `bid` of tenant `tenant`, locked behind the
/// billing lock: `404` for another tenant's or a deleted one, `403
/// budget_managed` for one another level set.
async fn operator_row(
    conn: &mut PgConnection,
    tenant: Uuid,
    bid: Uuid,
) -> Result<BudgetRow, ApiError> {
    let row = budgets::get(conn, tenant, bid)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if row.set_by != SetBy::Operator {
        return Err(ApiError::budget_managed());
    }
    Ok(row)
}

/// `PATCH /admin/tenants/{id}/budgets/{bid}` with `{limit_usd}` → `200
/// Budget`, settled. Only an operator's quota: another level's is `403
/// budget_managed`, another tenant's or a deleted one `404`; `422
/// /limit_usd` unless the limit is dollars and cents above 0.
pub async fn update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, bid)): Path<(String, String)>,
    ApiJson(req): ApiJson<SetLimit>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let (id, bid) = ids(&id, &bid)?;
    let limit = parse_usd(&req.limit_usd, "/limit_usd")?;
    let ctx = WriteCtx::of(&state);
    let (row, spent, now, found) = state
        .db
        .tx(move |conn| {
            async move {
                let (tenant, _) = tenants::lock_billing(&mut *conn, id).await?;
                operator_row(&mut *conn, id, bid).await?;
                let row = budgets::set_limit(
                    &mut *conn,
                    &ctx,
                    &tenant,
                    bid,
                    limit,
                    Actor::Operator(&auth),
                )
                .await?;
                let now = budgets::db_now(&mut *conn).await?;
                let spent = budgets::spent(&mut *conn, &row, now).await?;
                let found = targets(&mut *conn, &tenant, std::slice::from_ref(&row)).await?;
                Ok::<_, ApiError>((row, spent, now, found))
            }
            .boxed()
        })
        .await?;
    Ok(Json(api(&row, spent, now, &found)))
}

/// `DELETE /admin/tenants/{id}/budgets/{bid}` → `204`, settled. Only an
/// operator's quota, as for [`update`].
pub async fn delete(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, bid)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let (id, bid) = ids(&id, &bid)?;
    let ctx = WriteCtx::of(&state);
    state
        .db
        .tx(move |conn| {
            async move {
                let (tenant, _) = tenants::lock_billing(&mut *conn, id).await?;
                operator_row(&mut *conn, id, bid).await?;
                budgets::delete(&mut *conn, &ctx, &tenant, bid, Actor::Operator(&auth)).await
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn ids(tenant: &str, budget: &str) -> Result<(Uuid, Uuid), ApiError> {
    let parse = |s: &str| Uuid::parse_str(s).map_err(|_| ApiError::not_found());
    Ok((parse(tenant)?, parse(budget)?))
}
