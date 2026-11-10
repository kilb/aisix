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
//!
//! The tenant's own levels are `/api/budgets` (the `member_*` handlers): an
//! owner or admin sets `member` quotas and quotas on any key, and a member
//! caps its own keys. Each level edits only its own rows.

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
use super::money::parse_cny;
use crate::audit::Actor;
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::members;
use crate::resources::write::WriteCtx;
use crate::tenants::{self, TenantRow};
use crate::AppState;
use aisix_core::models::BudgetPeriod;

/// Which members' emails a reader may see in a target's label.
#[derive(Debug, Clone, Copy)]
enum Emails {
    /// Every member's: an operator, or an owner or admin, who may list the
    /// members.
    All,
    /// Only the reader's own: every other member role.
    OwnOnly(Uuid),
}

impl Emails {
    fn of(auth: &MemberAuth) -> Emails {
        match auth.role {
            MemberRole::Owner | MemberRole::Admin => Emails::All,
            _ => Emails::OwnOnly(auth.id),
        }
    }
}

/// A member target's label: its display name; else its email, for a reader
/// `emails` allows; else 成员 and the start of its id.
fn member_label(id: Uuid, display_name: Option<String>, email: String, emails: Emails) -> String {
    if let Some(name) = display_name.filter(|n| !n.is_empty()) {
        return name;
    }
    match emails {
        Emails::OwnOnly(me) if me != id => format!("成员 {}", &id.simple().to_string()[..8]),
        _ => email,
    }
}

/// The label and liveness of each target of `rows`: the tenant's name, a
/// member's label ([`member_label`]), a key's label. A target that is gone
/// keeps its label and is not live; one that is not found at all reads as
/// not live with an empty label.
async fn targets(
    conn: &mut PgConnection,
    tenant: &TenantRow,
    rows: &[BudgetRow],
    emails: Emails,
) -> Result<HashMap<Uuid, (String, bool)>, DbError> {
    let mut found = HashMap::new();
    found.insert(tenant.id, (tenant.name.clone(), true));
    let ids = |scope: BudgetScope| -> Vec<Uuid> {
        rows.iter()
            .filter(|b| b.scope == scope)
            .map(|b| b.scope_ref)
            .collect()
    };
    let members = ids(BudgetScope::Member);
    if !members.is_empty() {
        let named: Vec<(Uuid, Option<String>, String, bool)> = sqlx::query_as(
            "SELECT id, display_name, email, removed_at IS NULL FROM members \
             WHERE tenant_id = $1 AND id = ANY($2)",
        )
        .bind(tenant.id)
        .bind(&members)
        .fetch_all(&mut *conn)
        .await?;
        found.extend(named.into_iter().map(|(id, display_name, email, live)| {
            (id, (member_label(id, display_name, email, emails), live))
        }));
    }
    let keys = ids(BudgetScope::ApiKey);
    if !keys.is_empty() {
        let named: Vec<(Uuid, String, bool)> = sqlx::query_as(
            "SELECT id, label, revoked_at IS NULL FROM tenant_keys \
             WHERE tenant_id = $1 AND id = ANY($2)",
        )
        .bind(tenant.id)
        .bind(&keys)
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
    let found = targets(&mut conn, &tenant, &plain, Emails::All).await?;
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
    limit_cny: String,
}

fn invalid(path: &str, message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: message.to_string(),
    }])
}

/// A quota's `kind`, `period` and limit as typed: `periodic` (the default)
/// or `cumulative`; a periodic quota's period is `day`, `week` or `month`
/// (the default), a cumulative one has none. What is wrong is added to
/// `problems` at `/kind`, `/period` and `/limit_cny`.
fn terms(
    kind: Option<&str>,
    period: Option<&str>,
    limit_cny: &str,
    problems: &mut Vec<Detail>,
) -> (Kind, Option<BudgetPeriod>, i64) {
    let kind = match kind {
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
    let period = match (kind, period) {
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
    let limit = parse_cny(limit_cny, "/limit_cny").unwrap_or_else(|e| {
        problems.extend(e.details);
        0
    });
    (kind, period, limit)
}

/// `POST /admin/tenants/{id}/budgets` with `{kind?, period?, limit_cny}` →
/// `201 Budget`: the organization's quota (scope `team`, `set_by
/// "operator"`), settled before the answer. `kind` is `periodic` (the
/// default) or `cumulative`; a periodic quota's `period` is `day`, `week` or
/// `month` (the default), and a cumulative one has none. `422` at `/kind`,
/// `/period`, `/limit_cny` (yuan and fen above 0) and, for the internal
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
    let (kind, period, limit) = terms(
        req.kind.as_deref(),
        req.period.as_deref(),
        &req.limit_cny,
        &mut problems,
    );
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
                    limit_micro_cny: limit,
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
                let found =
                    targets(&mut *conn, &tenant, std::slice::from_ref(&row), Emails::All).await?;
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
    limit_cny: String,
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

/// `PATCH /admin/tenants/{id}/budgets/{bid}` with `{limit_cny}` → `200
/// Budget`, settled. Only an operator's quota: another level's is `403
/// budget_managed`, another tenant's or a deleted one `404`; `422
/// /limit_cny` unless the limit is yuan and fen above 0.
pub async fn update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((id, bid)): Path<(String, String)>,
    ApiJson(req): ApiJson<SetLimit>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let (id, bid) = ids(&id, &bid)?;
    let limit = parse_cny(&req.limit_cny, "/limit_cny")?;
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
                let found =
                    targets(&mut *conn, &tenant, std::slice::from_ref(&row), Emails::All).await?;
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

/// The ids of the live and revoked keys of `tenant` bound to member `member`.
async fn keys_of(
    conn: &mut PgConnection,
    tenant: Uuid,
    member: Uuid,
) -> Result<Vec<Uuid>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT id FROM tenant_keys WHERE tenant_id = $1 AND member_id = $2")
            .bind(tenant)
            .bind(member)
            .fetch_all(conn)
            .await?,
    )
}

/// Whether quota `b` is at a level member `me` sees: its own member quota or
/// a quota on a key bound to it (`keys`).
fn is_mine(b: &BudgetRow, me: Uuid, keys: &[Uuid]) -> bool {
    match b.scope {
        BudgetScope::Team => false,
        BudgetScope::Member => b.scope_ref == me,
        BudgetScope::ApiKey => keys.contains(&b.scope_ref),
    }
}

/// `GET /api/budgets` → `{items: [Budget]}`. An owner, admin or billing
/// member sees every level of the tenant's quotas; a member sees the quotas
/// on itself and on the keys bound to it, and no organization quota.
pub async fn member_list(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Billing,
        MemberRole::Member,
    ])?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = tenants::find(&mut conn, auth.tenant_id).await?;
    let now = budgets::db_now(&mut conn).await?;
    let mut rows = budgets::list(&mut conn, tenant.id, now).await?;
    if auth.role == MemberRole::Member {
        let keys = keys_of(&mut conn, tenant.id, auth.id).await?;
        rows.retain(|(b, _)| is_mine(b, auth.id, &keys));
    }
    let plain: Vec<BudgetRow> = rows.iter().map(|(b, _)| b.clone()).collect();
    let found = targets(&mut conn, &tenant, &plain, Emails::of(&auth)).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|(b, spent)| api(b, *spent, now, &found))
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewMemberQuota {
    scope: String,
    scope_ref: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    period: Option<String>,
    limit_cny: String,
}

/// `POST /api/budgets` with `{scope, scope_ref, kind?, period?, limit_cny}`
/// → `201 Budget`, settled. The scope is `member` or `api_key`; `team` is
/// the operators' (`403 budget_managed`). An owner or admin sets a `member`
/// quota or one on any live key of the tenant (`set_by "org_admin"`); a
/// member sets one on a live key bound to it (`set_by "member"`) and gets
/// `403 budget_managed` for a `member` quota and `403 forbidden` for another
/// member's key. A target that is not the tenant's, another tenant's
/// included, is `422` at `/scope_ref`, as for one that does not exist.
/// `kind` and `period` default to `periodic` and `month`; `422` at `/scope`,
/// `/kind`, `/period` and `/limit_cny` as for the operators' route.
pub async fn member_create(
    auth: MemberAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<NewMemberQuota>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Member])?;
    let mut problems = Vec::new();
    let scope = match req.scope.as_str() {
        "team" => return Err(ApiError::budget_managed()),
        "member" => Some(BudgetScope::Member),
        "api_key" => Some(BudgetScope::ApiKey),
        _ => {
            problems.push(Detail {
                path: "/scope".into(),
                message: "额度层级应为 member 或 api_key".into(),
            });
            None
        }
    };
    let (kind, period, limit) = terms(
        req.kind.as_deref(),
        req.period.as_deref(),
        &req.limit_cny,
        &mut problems,
    );
    // An id that is no uuid names nothing, like one that is not found.
    let scope_ref = Uuid::parse_str(&req.scope_ref).ok();
    if scope_ref.is_none() {
        problems.push(Detail {
            path: "/scope_ref".into(),
            message: "额度对象不存在".into(),
        });
    }
    let (Some(scope), Some(scope_ref), true) = (scope, scope_ref, problems.is_empty()) else {
        return Err(ApiError::validation(problems));
    };
    if auth.role == MemberRole::Member && scope == BudgetScope::Member {
        return Err(ApiError::budget_managed());
    }
    let ctx = WriteCtx::of(&state);
    let (row, spent, now, found) = state
        .db
        .tx(move |conn| {
            async move {
                let (tenant, _) = tenants::lock_billing(&mut *conn, auth.tenant_id).await?;
                let auth = members::current(&mut *conn, &auth, QUOTA_SETTERS).await?;
                let by_member = auth.role == MemberRole::Member;
                if by_member && scope == BudgetScope::Member {
                    return Err(ApiError::budget_managed());
                }
                if by_member {
                    // A key that is not live and the tenant's reads as no
                    // key at all: the domain answers it at `/scope_ref`.
                    let bound: Option<Option<Uuid>> = sqlx::query_scalar(
                        "SELECT member_id FROM tenant_keys \
                         WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
                    )
                    .bind(scope_ref)
                    .bind(tenant.id)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    if bound.is_some_and(|m| m != Some(auth.id)) {
                        return Err(ApiError::forbidden());
                    }
                }
                let new = NewBudget {
                    scope,
                    scope_ref,
                    set_by: if by_member {
                        SetBy::Member
                    } else {
                        SetBy::OrgAdmin
                    },
                    kind,
                    period,
                    limit_micro_cny: limit,
                    created_by_kind: "member",
                    created_by: Some(auth.id),
                };
                let actor = Actor::Member {
                    id: auth.id,
                    email: &auth.email,
                };
                let row = budgets::create(&mut *conn, &ctx, &tenant, new, actor).await?;
                let now = budgets::db_now(&mut *conn).await?;
                let spent = budgets::spent(&mut *conn, &row, now).await?;
                let found = targets(
                    &mut *conn,
                    &tenant,
                    std::slice::from_ref(&row),
                    Emails::of(&auth),
                )
                .await?;
                Ok::<_, ApiError>((row, spent, now, found))
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(api(&row, spent, now, &found))))
}

/// The member roles that set quotas on `/api/budgets`.
const QUOTA_SETTERS: &[MemberRole] = &[MemberRole::Owner, MemberRole::Admin, MemberRole::Member];

/// The quota `bid` of the caller's tenant that `auth` may edit, behind the
/// billing lock, `auth` being the caller as read under that lock
/// ([`members::current`]: a caller demoted while the request waited is
/// judged by the role it has now): `404` for another tenant's, a deleted one, or (for a
/// member) one at a level it does not see; `403 budget_managed` for one set
/// at a level above the caller's.
async fn editable_row(
    conn: &mut PgConnection,
    auth: &MemberAuth,
    bid: Uuid,
) -> Result<BudgetRow, ApiError> {
    let row = budgets::get(conn, auth.tenant_id, bid)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if row.set_by == SetBy::Operator {
        return Err(ApiError::budget_managed());
    }
    if auth.role == MemberRole::Member {
        let keys = keys_of(conn, auth.tenant_id, auth.id).await?;
        if !is_mine(&row, auth.id, &keys) {
            return Err(ApiError::not_found());
        }
        if row.set_by != SetBy::Member {
            return Err(ApiError::budget_managed());
        }
    }
    Ok(row)
}

fn bid_of(s: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(s).map_err(|_| ApiError::not_found())
}

/// `PATCH /api/budgets/{id}` with `{limit_cny}` → `200 Budget`, settled. An
/// owner or admin edits `org_admin` and `member` quotas, a member only its
/// own (`set_by "member"`); an operator's quota is `403 budget_managed`, as
/// is the owner's cap on a member's key for that member. Another tenant's
/// quota is `404`; `422 /limit_cny` unless yuan and fen above 0.
pub async fn member_update(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(bid): Path<String>,
    ApiJson(req): ApiJson<SetLimit>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Member])?;
    let bid = bid_of(&bid)?;
    let limit = parse_cny(&req.limit_cny, "/limit_cny")?;
    let ctx = WriteCtx::of(&state);
    let (row, spent, now, found) = state
        .db
        .tx(move |conn| {
            async move {
                let (tenant, _) = tenants::lock_billing(&mut *conn, auth.tenant_id).await?;
                let auth = members::current(&mut *conn, &auth, QUOTA_SETTERS).await?;
                editable_row(&mut *conn, &auth, bid).await?;
                let actor = Actor::Member {
                    id: auth.id,
                    email: &auth.email,
                };
                let row = budgets::set_limit(&mut *conn, &ctx, &tenant, bid, limit, actor).await?;
                let now = budgets::db_now(&mut *conn).await?;
                let spent = budgets::spent(&mut *conn, &row, now).await?;
                let found = targets(
                    &mut *conn,
                    &tenant,
                    std::slice::from_ref(&row),
                    Emails::of(&auth),
                )
                .await?;
                Ok::<_, ApiError>((row, spent, now, found))
            }
            .boxed()
        })
        .await?;
    Ok(Json(api(&row, spent, now, &found)))
}

/// `DELETE /api/budgets/{id}` → `204`, settled. Who may delete which quota
/// is [`member_update`]'s.
pub async fn member_delete(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(bid): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Member])?;
    let bid = bid_of(&bid)?;
    let ctx = WriteCtx::of(&state);
    state
        .db
        .tx(move |conn| {
            async move {
                let (tenant, _) = tenants::lock_billing(&mut *conn, auth.tenant_id).await?;
                let auth = members::current(&mut *conn, &auth, QUOTA_SETTERS).await?;
                editable_row(&mut *conn, &auth, bid).await?;
                let actor = Actor::Member {
                    id: auth.id,
                    email: &auth.email,
                };
                budgets::delete(&mut *conn, &ctx, &tenant, bid, actor).await
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
