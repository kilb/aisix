use aisix_core::{ResourceEntry, TokenCounting};
use axum::extract::{Path, State};
use axum::Json;

use crate::{AdminAuth, AdminError, AdminState};

pub async fn list_token_counting(
    _auth: AdminAuth,
    State(state): State<AdminState>,
) -> Result<Json<Vec<ResourceEntry<TokenCounting>>>, AdminError> {
    Ok(Json(state.store.list_token_counting().await?))
}

pub async fn get_token_counting(
    _auth: AdminAuth,
    Path(id): Path<String>,
    State(state): State<AdminState>,
) -> Result<Json<ResourceEntry<TokenCounting>>, AdminError> {
    let entry = state
        .store
        .get_token_counting(&id)
        .await?
        .ok_or(AdminError::NotFound)?;
    Ok(Json(entry))
}
