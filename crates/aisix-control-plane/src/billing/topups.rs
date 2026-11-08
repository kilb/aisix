//! Top-ups: money coming into a tenant's balance, with no payment provider.
//!
//! A tenant's owner, admin or billing member requests one
//! (`POST /api/billing/topups`), and an operator decides it: an approval
//! posts one `topup` ledger entry and settles the balance mark in the same
//! transaction, under the tenant's billing lock (`tenants::lock_billing`); a
//! rejection carries a reason and moves no money. An operator credit is a
//! top-up inserted already approved ([`insert_approved`]). Every step is
//! audited in its own transaction.
//!
//! A top-up is decided once. The decision is an `UPDATE … WHERE status =
//! 'pending'` under the tenant's row lock, so of two approvals racing for
//! one top-up (a double click, two operators, two control-plane instances)
//! the second finds no pending row and answers `409 topup_state`, and the
//! ledger's unique `topup_id` holds even without the lock.
//!
//! A tenant reads its own top-ups without anything about the operator who
//! decided them; operators read every tenant's, with the decider.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::ledger::{self, Posting, Source};
use super::marks;
use super::money::parse_usd;
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::resources::write::WriteCtx;
use crate::tenants;
use crate::usage::{page, page_limit, query_rejected};
use crate::AppState;

/// The most top-up requests one tenant may have awaiting a decision.
pub const MAX_PENDING_TOPUPS: i64 = 20;

/// The largest top-up, $1,000,000, in micro-USD.
pub const MAX_TOPUP_MICRO_USD: i64 = 1_000_000_000_000;

/// The longest note a request may carry, in characters.
const MAX_NOTE_CHARS: usize = 500;

/// The longest payment reference, in characters.
pub const MAX_REFERENCE_CHARS: usize = 128;

/// The longest reason an operator gives (a rejection's or a credit's), in
/// characters.
const MAX_REASON_CHARS: usize = 500;

const TARGET_KIND: &str = "topup";

/// Who started a top-up: a tenant's request, or an operator's credit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Tenant,
    Operator,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Tenant => "tenant",
            Origin::Operator => "operator",
        }
    }
}

impl TryFrom<String> for Origin {
    type Error = String;

    fn try_from(s: String) -> Result<Origin, String> {
        [Origin::Tenant, Origin::Operator]
            .into_iter()
            .find(|origin| origin.as_str() == s)
            .ok_or_else(|| format!("unknown top-up origin {s:?}"))
    }
}

/// Where a top-up stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Approved,
    Rejected,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Rejected => "rejected",
        }
    }
}

impl TryFrom<String> for Status {
    type Error = String;

    fn try_from(s: String) -> Result<Status, String> {
        [Status::Pending, Status::Approved, Status::Rejected]
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| format!("unknown top-up status {s:?}"))
    }
}

/// One `billing_topups` row, with the names its views show: its tenant's,
/// its requesting member's display name and its deciding operator's email.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TopupRow {
    pub id: Uuid,
    /// Request order, for paging only.
    pub seq: i64,
    pub tenant_id: Uuid,
    pub tenant_name: String,
    pub amount_micro_usd: i64,
    #[sqlx(try_from = "String")]
    pub origin: Origin,
    pub note: String,
    pub reference: String,
    #[sqlx(try_from = "String")]
    pub status: Status,
    pub requested_by: Option<Uuid>,
    pub requester_name: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub decided_by: Option<Uuid>,
    pub decider_email: Option<String>,
    pub decided_at: Option<DateTime<Utc>>,
    pub reject_reason: Option<String>,
    pub credit_reason: Option<String>,
}

const SELECT: &str = "SELECT p.id, p.seq, p.tenant_id, t.name AS tenant_name, \
     p.amount_micro_usd, p.origin, p.note, p.reference, p.status, p.requested_by, \
     m.display_name AS requester_name, p.requested_at, p.decided_by, \
     o.email AS decider_email, p.decided_at, p.reject_reason, p.credit_reason \
     FROM billing_topups p JOIN tenants t ON t.id = p.tenant_id \
     LEFT JOIN members m ON m.id = p.requested_by \
     LEFT JOIN operators o ON o.id = p.decided_by";

/// `Topup`, as the tenant reads it: who of the tenant requested it (`null`
/// for an operator credit, which carries its `credit_reason` instead), and
/// nothing about the operator who decided it.
pub fn to_tenant(row: &TopupRow) -> Value {
    json!({
        "id": row.id,
        "origin": row.origin,
        "amount_micro_usd": row.amount_micro_usd,
        "note": row.note,
        "reference": row.reference,
        "status": row.status,
        "requested_by": row.requested_by.map(|id| json!({
            "id": id,
            "display_name": row.requester_name,
        })),
        "requested_at": row.requested_at,
        "decided_at": row.decided_at,
        "reject_reason": row.reject_reason,
        "credit_reason": row.credit_reason,
    })
}

/// `AdminTopup`: the tenant's view, with the tenant and the deciding
/// operator.
pub fn to_admin(row: &TopupRow) -> Value {
    let mut view = to_tenant(row);
    view["tenant_id"] = json!(row.tenant_id);
    view["tenant_name"] = json!(row.tenant_name);
    view["decided_by"] = json!(row.decided_by.map(|id| json!({
        "id": id,
        "email": row.decider_email,
    })));
    view
}

/// Top-up `id`, whoever's it is.
async fn read(conn: &mut PgConnection, id: Uuid) -> Result<TopupRow, DbError> {
    Ok(sqlx::query_as(&format!("{SELECT} WHERE p.id = $1"))
        .bind(id)
        .fetch_one(conn)
        .await?)
}

/// Inserts a top-up decided at once, `approved` by operator `decided_by`:
/// an operator credit (`Origin::Operator`, no `requested_by`, a
/// `credit_reason`). Its ledger entry is the caller's to post, in the same
/// transaction and under the tenant's billing lock. It does not count
/// toward [`MAX_PENDING_TOPUPS`].
#[allow(clippy::too_many_arguments)] // the shape the credit route consumes
pub async fn insert_approved(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    amount_micro_usd: i64,
    origin: Origin,
    requested_by: Option<Uuid>,
    credit_reason: Option<&str>,
    reference: &str,
    decided_by: Uuid,
) -> Result<TopupRow, DbError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_topups \
         (id, tenant_id, amount_micro_usd, origin, reference, status, requested_by, \
          decided_by, decided_at, credit_reason) \
         VALUES ($1, $2, $3, $4, $5, 'approved', $6, $7, now(), $8)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(amount_micro_usd)
    .bind(origin.as_str())
    .bind(reference)
    .bind(requested_by)
    .bind(decided_by)
    .bind(credit_reason)
    .execute(&mut *conn)
    .await?;
    read(conn, id).await
}

fn detail(path: &str, message: impl Into<String>) -> Detail {
    Detail {
        path: path.to_string(),
        message: message.into(),
    }
}

/// `raw` as a top-up amount in micro-USD: dollars and cents, above 0 and at
/// most [`MAX_TOPUP_MICRO_USD`]; the problem at `/amount_usd` otherwise.
pub fn topup_amount(raw: &str) -> Result<i64, Vec<Detail>> {
    let micro = parse_usd(raw, "/amount_usd").map_err(|e| e.details)?;
    if micro <= 0 || micro > MAX_TOPUP_MICRO_USD {
        return Err(vec![detail(
            "/amount_usd",
            "金额须大于 0 且不超过 1,000,000 美元",
        )]);
    }
    Ok(micro)
}

/// The problem with free text at `path` that may hold `max` characters and
/// no control character, if any.
fn text_problem(raw: &str, path: &str, max: usize) -> Option<Detail> {
    if raw.chars().count() > max {
        Some(detail(path, format!("最多 {max} 个字符")))
    } else if raw.chars().any(char::is_control) {
        Some(detail(path, "不能包含控制字符"))
    } else {
        None
    }
}

/// The problem with `reference` at `/reference`, if any: at most
/// [`MAX_REFERENCE_CHARS`] characters, none a control character.
pub fn reference_problem(reference: &str) -> Option<Detail> {
    text_problem(reference, "/reference", MAX_REFERENCE_CHARS)
}

/// `raw` as the reason an operator gives at `/reason` (a rejection's or a
/// credit's): 1 to [`MAX_REASON_CHARS`] characters, not blank, and no control
/// character. `missing` is the problem's message for an absent, blank or too
/// long one.
pub(crate) fn operator_reason(raw: Option<String>, missing: &str) -> Result<String, Detail> {
    match raw {
        Some(r) if r.chars().any(char::is_control) => Err(detail("/reason", "不能包含控制字符")),
        Some(r) if !r.trim().is_empty() && r.chars().count() <= MAX_REASON_CHARS => Ok(r),
        _ => Err(detail("/reason", missing)),
    }
}

/// A top-up id in a path; a malformed one names no top-up.
fn topup_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}

/// The answer to a decision on a top-up that is no longer pending.
fn decided_already() -> ApiError {
    ApiError::conflict("topup_state", "该充值申请已处理，不能再次审核")
}

/// The tenant of top-up `id`, read without a lock (it never changes); `404
/// not_found` when there is none.
async fn tenant_of(conn: &mut PgConnection, id: Uuid) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("SELECT tenant_id FROM billing_topups WHERE id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(DbError::from)?
        .ok_or_else(ApiError::not_found)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTopup {
    amount_usd: String,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    reference: Option<String>,
}

/// `POST /api/billing/topups` with `{amount_usd, note?, reference?}` →
/// `201 Topup`, pending. `422` at `/amount_usd` unless it is dollars and
/// cents above 0 and at most $1,000,000, and at `/note` (500 characters) or
/// `/reference` (128) when too long or holding a control character; `409
/// topup_limit` while the tenant has [`MAX_PENDING_TOPUPS`] pending already.
pub async fn request(
    auth: MemberAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<NewTopup>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let note = req.note.unwrap_or_default();
    let reference = req.reference.unwrap_or_default();
    let mut problems = Vec::new();
    let amount = topup_amount(&req.amount_usd).unwrap_or_else(|mut d| {
        problems.append(&mut d);
        0
    });
    problems.extend(text_problem(&note, "/note", MAX_NOTE_CHARS));
    problems.extend(reference_problem(&reference));
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    let row = state
        .db
        .tx(move |conn| {
            async move {
                // The tenant's row serialises its requests, so the count
                // below holds until this one is in.
                tenants::lock(&mut *conn, auth.tenant_id).await?;
                let pending: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM billing_topups \
                     WHERE tenant_id = $1 AND status = 'pending'",
                )
                .bind(auth.tenant_id)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if pending >= MAX_PENDING_TOPUPS {
                    return Err(ApiError::conflict(
                        "topup_limit",
                        "待处理的充值申请已达 20 笔，请等待审核后再提交",
                    ));
                }
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO billing_topups \
                     (id, tenant_id, amount_micro_usd, origin, note, reference, status, \
                      requested_by) \
                     VALUES ($1, $2, $3, 'tenant', $4, $5, 'pending', $6)",
                )
                .bind(id)
                .bind(auth.tenant_id)
                .bind(amount)
                .bind(&note)
                .bind(&reference)
                .bind(auth.id)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Member {
                            id: auth.id,
                            email: &auth.email,
                        },
                        action: "billing.topup.request",
                        target_kind: TARGET_KIND,
                        target_id: Some(id.to_string()),
                        tenant_id: Some(auth.tenant_id),
                        detail: json!({ "amount_micro_usd": amount }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(read(conn, id).await?)
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(to_tenant(&row))))
}

#[derive(Deserialize)]
pub struct TenantListQuery {
    status: Option<Status>,
    limit: Option<i64>,
    /// Only top-ups with a smaller `seq`: the `next_before` of the previous
    /// page.
    before: Option<i64>,
}

/// `GET /api/billing/topups[?status&limit&before]` → `{items: [Topup],
/// next_before}`: the caller's organization's top-ups, operator credits
/// included, newest first, paged as `/admin/usage/events` is.
pub async fn list(
    auth: MemberAuth,
    State(state): State<AppState>,
    query: Result<Query<TenantListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(format!("{SELECT} WHERE p.tenant_id = "));
    sql.push_bind(auth.tenant_id);
    filtered_page(&mut sql, q.status, None, q.before, limit);
    let rows: Vec<TopupRow> = sql
        .build_query_as()
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    let (rows, next_before) = page(rows, limit, |r| r.seq);
    let items: Vec<Value> = rows.iter().map(to_tenant).collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

/// Ends a top-up query with its filters and one page, newest first by
/// `seq`, one row past `limit`. The status and origin are written as
/// literals from their enums, so the pending queue is answered from its
/// partial index.
fn filtered_page(
    sql: &mut QueryBuilder<'_, Postgres>,
    status: Option<Status>,
    origin: Option<Origin>,
    before: Option<i64>,
    limit: i64,
) {
    if let Some(status) = status {
        sql.push(format!(" AND p.status = '{}'", status.as_str()));
    }
    if let Some(origin) = origin {
        sql.push(format!(" AND p.origin = '{}'", origin.as_str()));
    }
    if let Some(before) = before {
        sql.push(" AND p.seq < ").push_bind(before);
    }
    sql.push(" ORDER BY p.seq DESC LIMIT ").push_bind(limit + 1);
}

/// `GET /api/billing/topups/{id}` → `Topup`; `404 not_found` for an id that
/// is not one of the caller's organization's top-ups, whoever's it is.
pub async fn get(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin, MemberRole::Billing])?;
    let id = topup_id(&id)?;
    let row: Option<TopupRow> =
        sqlx::query_as(&format!("{SELECT} WHERE p.id = $1 AND p.tenant_id = $2"))
            .bind(id)
            .bind(auth.tenant_id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(DbError::from)?;
    Ok(Json(to_tenant(&row.ok_or_else(ApiError::not_found)?)))
}

#[derive(Deserialize)]
pub struct AdminListQuery {
    status: Option<Status>,
    tenant_id: Option<Uuid>,
    origin: Option<Origin>,
    limit: Option<i64>,
    /// Only top-ups with a smaller `seq`: the `next_before` of the previous
    /// page.
    before: Option<i64>,
}

/// `GET /admin/billing/topups[?status&tenant_id&origin&limit&before]` →
/// `{items: [AdminTopup], next_before}`: every tenant's top-ups, newest
/// first, paged as `/admin/usage/events` is.
pub async fn admin_list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<AdminListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = page_limit(q.limit);
    let mut sql = QueryBuilder::<Postgres>::new(format!("{SELECT} WHERE true"));
    if let Some(tenant_id) = q.tenant_id {
        sql.push(" AND p.tenant_id = ").push_bind(tenant_id);
    }
    filtered_page(&mut sql, q.status, q.origin, q.before, limit);
    let rows: Vec<TopupRow> = sql
        .build_query_as()
        .fetch_all(state.db.pool())
        .await
        .map_err(DbError::from)?;
    let (rows, next_before) = page(rows, limit, |r| r.seq);
    let items: Vec<Value> = rows.iter().map(to_admin).collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

/// `POST /admin/billing/topups/{id}/approve` → `200 AdminTopup`, in one
/// transaction under the tenant's billing lock: the top-up is approved,
/// its amount posted to the ledger, the balance mark settled and the
/// approval audited, so when the answer arrives the gateway's document
/// already says what the new balance means. `404 not_found` for no such
/// top-up; `409 topup_state` when it is no longer pending.
pub async fn approve(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = topup_id(&id)?;
    let ctx = WriteCtx::of(&state);
    let row = state
        .db
        .tx(move |conn| {
            async move {
                let tenant_id = tenant_of(&mut *conn, id).await?;
                // The billing lock: the approval posts to the ledger and
                // moves the balance mark.
                tenants::lock_billing(&mut *conn, tenant_id).await?;
                let amount: Option<i64> = sqlx::query_scalar(
                    "UPDATE billing_topups \
                     SET status = 'approved', decided_by = $2, decided_at = now() \
                     WHERE id = $1 AND status = 'pending' RETURNING amount_micro_usd",
                )
                .bind(id)
                .bind(auth.id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let amount = amount.ok_or_else(decided_already)?;
                ledger::post(
                    &mut *conn,
                    &[Posting {
                        tenant_id,
                        amount_micro_usd: amount,
                        source: Source::Topup { topup_id: id },
                    }],
                )
                .await?;
                marks::settle_locked(&mut *conn, &ctx, tenant_id).await?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "billing.topup.approve",
                        target_kind: TARGET_KIND,
                        target_id: Some(id.to_string()),
                        tenant_id: Some(tenant_id),
                        detail: json!({ "amount_micro_usd": amount }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(read(conn, id).await?)
            }
            .boxed()
        })
        .await?;
    Ok(Json(to_admin(&row)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rejection {
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /admin/billing/topups/{id}/reject` with `{reason}` → `200
/// AdminTopup`: the top-up is rejected with the reason, which the tenant
/// reads, and no money moves. `422 /reason` unless it is 1 to 500
/// characters, not blank and free of control characters; `404 not_found`
/// for no such top-up; `409 topup_state` when it is no longer pending.
pub async fn reject(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<Rejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = topup_id(&id)?;
    let reason = operator_reason(req.reason, "请填写驳回原因（1–500 个字符）")
        .map_err(|d| ApiError::validation(vec![d]))?;
    let row = state
        .db
        .tx(move |conn| {
            async move {
                let tenant_id = tenant_of(&mut *conn, id).await?;
                // No money moves, so the tenant's row will do: the account
                // stays with its usage intake.
                tenants::lock(&mut *conn, tenant_id).await?;
                let rejected = sqlx::query(
                    "UPDATE billing_topups \
                     SET status = 'rejected', decided_by = $2, decided_at = now(), \
                         reject_reason = $3 \
                     WHERE id = $1 AND status = 'pending'",
                )
                .bind(id)
                .bind(auth.id)
                .bind(&reason)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?
                .rows_affected();
                if rejected == 0 {
                    return Err(decided_already());
                }
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "billing.topup.reject",
                        target_kind: TARGET_KIND,
                        target_id: Some(id.to_string()),
                        tenant_id: Some(tenant_id),
                        detail: json!({ "reason": reason }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(read(conn, id).await?)
            }
            .boxed()
        })
        .await?;
    Ok(Json(to_admin(&row)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_up_is_dollars_and_cents_within_the_maximum() {
        assert_eq!(topup_amount("0.01"), Ok(10_000));
        assert_eq!(topup_amount("1000000"), Ok(MAX_TOPUP_MICRO_USD));
        for bad in ["0", "0.00", "1000000.01", "-1", "0.001"] {
            let problems = topup_amount(bad).expect_err(bad);
            assert_eq!(problems.len(), 1, "{bad}");
            assert_eq!(problems[0].path, "/amount_usd", "{bad}");
        }
    }

    #[test]
    fn free_text_is_bounded_in_characters_and_printable() {
        assert_eq!(text_problem(&"备".repeat(500), "/note", 500), None);
        assert!(text_problem(&"备".repeat(501), "/note", 500).is_some());
        for bad in [
            "a\u{0}b",
            "tab\there",
            "line\nbreak",
            "del\u{7f}",
            "c1\u{85}",
        ] {
            assert!(reference_problem(bad).is_some(), "{bad:?}");
        }
        assert_eq!(reference_problem("银行转账 #4b7-2026/11"), None);
    }

    #[test]
    fn origins_and_statuses_read_back_as_written() {
        for origin in [Origin::Tenant, Origin::Operator] {
            assert_eq!(Origin::try_from(origin.as_str().to_string()), Ok(origin));
            assert_eq!(
                serde_json::to_value(origin).expect("serializes"),
                json!(origin.as_str())
            );
        }
        for status in [Status::Pending, Status::Approved, Status::Rejected] {
            assert_eq!(Status::try_from(status.as_str().to_string()), Ok(status));
            assert_eq!(
                serde_json::to_value(status).expect("serializes"),
                json!(status.as_str())
            );
        }
        assert!(Origin::try_from("bank".to_string()).is_err());
        assert!(Status::try_from("done".to_string()).is_err());
    }
}
