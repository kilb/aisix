//! The audit log: who changed what, written in the same transaction as the
//! change, so a change is never without its row and a row never without its
//! change.
//!
//! `detail` holds field names and non-secret values only: never a password,
//! a hash, a token, a credential or a resource document.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::AppState;

/// Who made a change.
#[derive(Debug, Clone, Copy)]
pub enum Actor<'a> {
    Operator(&'a OperatorAuth),
    Member {
        id: Uuid,
        email: &'a str,
    },
    /// The control plane itself, or a command run on its host.
    System,
    /// Someone not logged in, by the email they gave.
    Anonymous {
        email: &'a str,
    },
}

/// One audit row.
#[derive(Debug, Clone)]
pub struct AuditEntry<'a> {
    pub actor: Actor<'a>,
    /// Such as `operator.create`.
    pub action: &'static str,
    pub target_kind: &'static str,
    pub target_id: Option<String>,
    pub tenant_id: Option<Uuid>,
    pub detail: Value,
}

/// Writes `e`, on the connection of the change it records.
pub async fn record(conn: &mut PgConnection, e: AuditEntry<'_>) -> Result<(), DbError> {
    let (kind, id, email) = match e.actor {
        Actor::Operator(op) => ("operator", Some(op.id), Some(op.email.as_str())),
        Actor::Member { id, email } => ("member", Some(id), Some(email)),
        Actor::System => ("system", None, None),
        Actor::Anonymous { email } => ("anonymous", None, Some(email)),
    };
    sqlx::query(
        "INSERT INTO audit_log \
         (actor_kind, actor_id, actor_email, action, target_kind, target_id, tenant_id, detail) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(kind)
    .bind(id)
    .bind(email)
    .bind(e.action)
    .bind(e.target_kind)
    .bind(e.target_id)
    .bind(e.tenant_id)
    .bind(e.detail)
    .execute(conn)
    .await?;
    Ok(())
}

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

#[derive(Deserialize)]
pub struct AuditQuery {
    limit: Option<i64>,
    /// Only rows with a smaller id: the `next_before` of the previous page.
    before: Option<i64>,
    target_kind: Option<String>,
    target_id: Option<String>,
    tenant_id: Option<Uuid>,
}

type AuditRow = (
    i64,
    DateTime<Utc>,
    String,
    Option<Uuid>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<Uuid>,
    Value,
);

/// `GET /admin/audit`: newest first, `limit` (default 50, at most 500) at a
/// time. `next_before` is the `before` of the next page, null on the last.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(|_| {
        ApiError::validation(vec![Detail {
            path: "query".to_string(),
            message: "查询参数不合法".to_string(),
        }])
    })?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // One row more than the page, to tell whether another page follows.
    let mut rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, at, actor_kind, actor_id, actor_email, action, target_kind, target_id, \
                tenant_id, detail \
         FROM audit_log \
         WHERE ($1::bigint IS NULL OR id < $1) \
           AND ($2::text IS NULL OR target_kind = $2) \
           AND ($3::text IS NULL OR target_id = $3) \
           AND ($4::uuid IS NULL OR tenant_id = $4) \
         ORDER BY id DESC LIMIT $5",
    )
    .bind(q.before)
    .bind(q.target_kind)
    .bind(q.target_id)
    .bind(q.tenant_id)
    .bind(limit + 1)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_before = if more { rows.last().map(|r| r.0) } else { None };
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                id,
                at,
                actor_kind,
                actor_id,
                actor_email,
                action,
                target_kind,
                target_id,
                tenant_id,
                detail,
            )| {
                json!({
                    "id": id,
                    "at": at,
                    "actor": {"kind": actor_kind, "id": actor_id, "email": actor_email},
                    "action": action,
                    "target_kind": target_kind,
                    "target_id": target_id,
                    "tenant_id": tenant_id,
                    "detail": detail,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}
