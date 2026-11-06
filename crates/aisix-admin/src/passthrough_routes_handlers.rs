//! Read handlers for `/admin/v1/passthrough_routes`: list and get-by-id,
//! same shape as [`crate::mcp_servers_handlers`]. Cross-field coupling
//! (match dimensions, target shape, per-mode required companions) lives in
//! the canonical schema, enforced on every declarative write path.
//! A password in `target_url` is returned as `[REDACTED]`
//! (see [`crate::redaction`]).

use aisix_core::credentials::CredentialKind;
use aisix_core::resource::ResourceEntry;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::Value;

use crate::auth::AdminAuth;
use crate::error::AdminError;
use crate::redaction::{redacted, redacted_all};
use crate::state::AdminState;

pub async fn list_passthrough_routes(
    _auth: AdminAuth,
    State(state): State<AdminState>,
) -> Result<Json<Vec<ResourceEntry<Value>>>, AdminError> {
    let entries = state.store.list_passthrough_routes().await?;
    Ok(Json(redacted_all(
        entries,
        CredentialKind::PassthroughRoute,
    )?))
}

pub async fn get_passthrough_route(
    _auth: AdminAuth,
    Path(id): Path<String>,
    State(state): State<AdminState>,
) -> Result<Json<ResourceEntry<Value>>, AdminError> {
    let entry = state
        .store
        .get_passthrough_route(&id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(redacted(entry, CredentialKind::PassthroughRoute)?))
}
