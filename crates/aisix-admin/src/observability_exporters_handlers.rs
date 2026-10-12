//! Read handlers for `/admin/v1/observability_exporters`: list and
//! get-by-id, same shape as [`crate::models_handlers`].
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

pub async fn list_observability_exporters(
    _auth: AdminAuth,
    State(state): State<AdminState>,
) -> Result<Json<Vec<ResourceEntry<Value>>>, AdminError> {
    let entries = state.store.list_observability_exporters().await?;
    Ok(Json(redacted_all(
        entries,
        CredentialKind::ObservabilityExporter,
    )?))
}

pub async fn get_observability_exporter(
    _auth: AdminAuth,
    Path(id): Path<String>,
    State(state): State<AdminState>,
) -> Result<Json<ResourceEntry<Value>>, AdminError> {
    let entry = state
        .store
        .get_observability_exporter(&id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(redacted(
        entry,
        CredentialKind::ObservabilityExporter,
    )?))
}
