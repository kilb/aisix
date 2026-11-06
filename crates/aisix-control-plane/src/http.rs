//! The browser listener's routes.

use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::AppState;

/// `/healthz` (no session), and, as they are added, `/admin/*` and `/api/*`.
pub fn browser_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Liveness: the process is up and serving. It reads nothing, so a
/// database outage does not take the listener out of a load balancer.
async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}
