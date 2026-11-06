//! `/dp/store/v1/{namespace}/{key}`: the shared store a managed gateway
//! keeps its per-user MCP OAuth records in (`crates/aisix-server/src/dp_store.rs`).
//!
//! This control plane does not offer it: every method answers `404 {}`. The
//! gateway reads that as "no such record" on GET and DELETE, and as the
//! store being unavailable on PUT, so per-user MCP authorization cannot be
//! completed through such a gateway.

use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use super::DpIdentity;

/// Any method on `/dp/store/v1/*` → `404 {}`, for a valid credential.
pub async fn not_found(_dp: DpIdentity) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({})))
}
