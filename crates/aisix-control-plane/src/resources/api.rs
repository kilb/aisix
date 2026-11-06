//! The operators' generic resource API, `/admin/resources/{kind}[/{id}]`,
//! and the strict schemas it validates with, `/admin/schemas/{resource}`.
//!
//! Reads redact every credential, whoever reads. Writes go through
//! [`write_resource`] with no `managed_by`, so a kind or a row that is
//! derived from tenants, members or keys is refused here (`409
//! managed_resource`). Replace and delete need `If-Match: "<version>"`.

use aisix_core::models::schema::{resource_root_schema, RESOURCES};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::store::{self, ResourceRow};
use super::write::{write_resource, ResourceWrite, WriteCtx, WriteOp};
use super::{managed_resource, secrets, Kind};
use crate::audit::{self, Actor, AuditEntry};
use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::AppState;

/// `ResourceView`: a stored document as the API shows it.
#[derive(Debug, Serialize)]
pub struct ResourceView {
    kind: Kind,
    id: Uuid,
    /// With every credential replaced by `[REDACTED]`.
    doc: Value,
    version: i64,
    managed_by: Option<String>,
    updated_at: DateTime<Utc>,
    rollout: Rollout,
}

/// How far the document has reached the gateways. Always `pending` until
/// rollout tracking lands (Task 9).
#[derive(Debug, Serialize)]
struct Rollout {
    state: &'static str,
    kine_revision: Option<i64>,
    applied: u64,
    online: u64,
    rejections: Vec<Value>,
}

impl ResourceView {
    fn of(row: ResourceRow) -> ResourceView {
        let mut doc = row.doc;
        secrets::redact(row.kind, &mut doc);
        ResourceView {
            kind: row.kind,
            id: row.id,
            doc,
            version: row.version,
            managed_by: row.managed_by,
            updated_at: row.updated_at,
            rollout: Rollout {
                state: "pending",
                kine_revision: row.kine_revision,
                applied: 0,
                online: 0,
                rejections: Vec::new(),
            },
        }
    }
}

/// A create or replace request: `{doc}`.
#[derive(Deserialize)]
pub struct DocBody {
    doc: Value,
}

fn kind_of(segment: &str) -> Result<Kind, ApiError> {
    Kind::from_segment(segment).ok_or_else(ApiError::unknown_kind)
}

fn writable(kind: Kind) -> Result<(), ApiError> {
    if kind.generic_writable() {
        Ok(())
    } else {
        Err(managed_resource())
    }
}

/// A malformed id names no row.
fn row_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}

/// The version `If-Match` names: `"3"`, or `3` unquoted.
fn if_match(headers: &HeaderMap) -> Result<i64, ApiError> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Err(ApiError::if_match_required());
    };
    value
        .to_str()
        .ok()
        .map(str::trim)
        .map(|v| {
            v.strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v)
        })
        .and_then(|v| v.parse::<i64>().ok())
        .ok_or_else(|| {
            ApiError::validation(vec![Detail {
                path: "If-Match".to_string(),
                message: "If-Match 必须是资源的版本号，例如 \"3\"".to_string(),
            }])
        })
}

/// The top-level field names of `doc`, which is all an audit row records of
/// a document.
fn field_names(doc: &Value) -> Vec<String> {
    let mut names: Vec<String> = doc
        .as_object()
        .map(|fields| fields.keys().cloned().collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// `GET /admin/schemas/{resource}`: the strict schema the gateway validates
/// writes of `resource` with, the published `schemas/resources` file.
pub async fn schema(
    auth: OperatorAuth,
    Path(resource): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    if !RESOURCES.contains(&resource.as_str()) {
        return Err(ApiError::unknown_kind());
    }
    Ok(Json(resource_root_schema(&resource, true)))
}

/// `GET /admin/resources/{kind}` → `{items: [ResourceView]}`, the live rows.
pub async fn list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let kind = kind_of(&kind)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let items: Vec<ResourceView> = store::list(&mut conn, kind)
        .await?
        .into_iter()
        .map(ResourceView::of)
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `GET /admin/resources/{kind}/{id}` → `ResourceView` and `ETag: "<version>"`.
pub async fn get(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let kind = kind_of(&kind)?;
    let id = row_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let row = store::get(&mut conn, kind, id)
        .await?
        .filter(|row| !row.deleted)
        .ok_or_else(ApiError::not_found)?;
    let etag = format!("\"{}\"", row.version);
    Ok(([(header::ETAG, etag)], Json(ResourceView::of(row))).into_response())
}

/// `POST /admin/resources/{kind}` with `{doc}` → `201 ResourceView`. A
/// singleton is created at the environment's id.
pub async fn create(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(kind): Path<String>,
    ApiJson(body): ApiJson<DocBody>,
) -> Result<(StatusCode, Json<ResourceView>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let kind = kind_of(&kind)?;
    writable(kind)?;
    let id = if kind.is_singleton() {
        state.env.env_id
    } else {
        Uuid::new_v4()
    };
    let ctx = WriteCtx::of(&state);
    let row = state
        .db
        .tx(move |conn| {
            async move {
                let write = ResourceWrite {
                    kind,
                    id,
                    op: WriteOp::Create(&body.doc),
                    expected_version: None,
                    managed_by: None,
                };
                let row = write_resource(conn, &ctx, write).await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "resource.create",
                        target_kind: kind.segment(),
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({ "fields": field_names(&row.doc) }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(row)
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(ResourceView::of(row))))
}

/// `PUT /admin/resources/{kind}/{id}` with `{doc}` and `If-Match` →
/// `200 ResourceView`.
pub async fn replace(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<DocBody>,
) -> Result<Json<ResourceView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let kind = kind_of(&kind)?;
    writable(kind)?;
    let id = row_id(&id)?;
    let expected = if_match(&headers)?;
    let ctx = WriteCtx::of(&state);
    let row = state
        .db
        .tx(move |conn| {
            async move {
                let write = ResourceWrite {
                    kind,
                    id,
                    op: WriteOp::Replace(&body.doc),
                    expected_version: Some(expected),
                    managed_by: None,
                };
                let row = write_resource(conn, &ctx, write).await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "resource.update",
                        target_kind: kind.segment(),
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({ "fields": field_names(&row.doc) }),
                    },
                )
                .await?;
                Ok::<_, ApiError>(row)
            }
            .boxed()
        })
        .await?;
    Ok(Json(ResourceView::of(row)))
}

/// `DELETE /admin/resources/{kind}/{id}` with `If-Match` → `200
/// ResourceView` of the tombstone; `409 in_use` while referenced.
pub async fn delete(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<ResourceView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let kind = kind_of(&kind)?;
    writable(kind)?;
    let id = row_id(&id)?;
    let expected = if_match(&headers)?;
    let ctx = WriteCtx::of(&state);
    let row = state
        .db
        .tx(move |conn| {
            async move {
                let write = ResourceWrite {
                    kind,
                    id,
                    op: WriteOp::Delete,
                    expected_version: Some(expected),
                    managed_by: None,
                };
                let row = write_resource(conn, &ctx, write).await?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: Actor::Operator(&auth),
                        action: "resource.delete",
                        target_kind: kind.segment(),
                        target_id: Some(id.to_string()),
                        tenant_id: None,
                        detail: json!({}),
                    },
                )
                .await?;
                Ok::<_, ApiError>(row)
            }
            .boxed()
        })
        .await?;
    Ok(Json(ResourceView::of(row)))
}
