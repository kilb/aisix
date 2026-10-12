//! Read handlers for `/admin/v1/a2a_agents`: list and get-by-id, same
//! shape as [`crate::models_handlers`]. The name is the path segment
//! under which the agent is exposed (`/a2a/<name>`); its constraints
//! and the per-auth_type credential coupling live in the canonical
//! schema, enforced on every declarative write path.
//! Credentials the resource stores are returned as `[REDACTED]`
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

pub async fn list_a2a_agents(
    _auth: AdminAuth,
    State(state): State<AdminState>,
) -> Result<Json<Vec<ResourceEntry<Value>>>, AdminError> {
    let entries = state.store.list_a2a_agents().await?;
    Ok(Json(redacted_all(entries, CredentialKind::A2aAgent)?))
}

pub async fn get_a2a_agent(
    _auth: AdminAuth,
    Path(id): Path<String>,
    State(state): State<AdminState>,
) -> Result<Json<ResourceEntry<Value>>, AdminError> {
    let entry = state
        .store
        .get_a2a_agent(&id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(redacted(entry, CredentialKind::A2aAgent)?))
}
