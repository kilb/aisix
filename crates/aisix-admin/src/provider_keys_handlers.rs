//! Read handlers for `/admin/v1/provider_keys`: list and get-by-id,
//! same shape as [`crate::models_handlers`].
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

pub async fn list_provider_keys(
    _auth: AdminAuth,
    State(state): State<AdminState>,
) -> Result<Json<Vec<ResourceEntry<Value>>>, AdminError> {
    let entries = state.store.list_provider_keys().await?;
    Ok(Json(redacted_all(entries, CredentialKind::ProviderKey)?))
}

pub async fn get_provider_key(
    _auth: AdminAuth,
    Path(id): Path<String>,
    State(state): State<AdminState>,
) -> Result<Json<ResourceEntry<Value>>, AdminError> {
    let entry = state
        .store
        .get_provider_key(&id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(redacted(entry, CredentialKind::ProviderKey)?))
}
