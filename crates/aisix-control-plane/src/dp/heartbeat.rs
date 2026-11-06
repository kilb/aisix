//! `POST /dp/heartbeat`: each gateway process reports itself every
//! `heartbeat_interval_secs` (15 s by default), keyed by its credential and
//! the `instance_id` it mints at boot, so replicas sharing a credential are
//! separate instances.
//!
//! The body is the gateway's `HeartbeatBody`
//! (`crates/aisix-server/src/heartbeat.rs`), decoded loosely: only `dp_id`
//! and `instance_id` are required, anything else missing takes its default
//! and anything unknown is ignored, since a gateway may be newer than this
//! control plane. The gateway reads only the status of the answer.

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::DpIdentity;
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::AppState;

/// An `instance_id` is a UUID; this leaves room for any other id a gateway
/// might mint, and keeps the key short enough to index.
const MAX_INSTANCE_ID_CHARS: usize = 128;

/// What a heartbeat reports. The lists are kept as sent: what a gateway
/// reports in them is for the views that show it.
#[derive(Deserialize)]
pub struct HeartbeatBody {
    dp_id: String,
    instance_id: String,
    #[serde(default)]
    hostname: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    uptime_seconds: i64,
    #[serde(default)]
    applied_revision: i64,
    /// Omitted until the gateway has applied a configuration.
    #[serde(default)]
    config_hash: Option<String>,
    #[serde(default)]
    supported_guardrail_kinds: Vec<Value>,
    #[serde(default)]
    exporter_health: Vec<Value>,
    /// Omitted when empty.
    #[serde(default)]
    rejected_resources: Vec<Value>,
    /// Omitted when empty.
    #[serde(default)]
    partially_compatible_resources: Vec<Value>,
}

/// `POST /dp/heartbeat` → `200 {}`. The instance's row takes everything
/// this beat reports, so a list the gateway now omits (it omits empty ones)
/// is empty, and a rejection it no longer reports is gone. A body naming
/// another credential's `dp_id` is `403`.
pub async fn heartbeat(
    dp: DpIdentity,
    State(state): State<AppState>,
    ApiJson(body): ApiJson<HeartbeatBody>,
) -> Result<Json<Value>, ApiError> {
    // A credential speaks only for itself.
    if Uuid::parse_str(&body.dp_id).ok() != Some(dp.dp_id) {
        return Err(ApiError::forbidden());
    }
    let id_chars = body.instance_id.chars().count();
    if id_chars == 0 || id_chars > MAX_INSTANCE_ID_CHARS {
        return Err(ApiError::validation(vec![Detail {
            path: "/instance_id".to_string(),
            message: format!("实例 ID 不能为空，且不能超过 {MAX_INSTANCE_ID_CHARS} 个字符"),
        }]));
    }
    sqlx::query(
        "INSERT INTO dp_instances (dp_id, instance_id, hostname, version, uptime_seconds, \
         applied_revision, config_hash, supported_guardrail_kinds, exporter_health, \
         rejected_resources, partially_compatible_resources) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (dp_id, instance_id) DO UPDATE SET hostname = EXCLUDED.hostname, \
         version = EXCLUDED.version, uptime_seconds = EXCLUDED.uptime_seconds, \
         applied_revision = EXCLUDED.applied_revision, config_hash = EXCLUDED.config_hash, \
         supported_guardrail_kinds = EXCLUDED.supported_guardrail_kinds, \
         exporter_health = EXCLUDED.exporter_health, \
         rejected_resources = EXCLUDED.rejected_resources, \
         partially_compatible_resources = EXCLUDED.partially_compatible_resources, \
         last_seen_at = now()",
    )
    .bind(dp.dp_id)
    .bind(&body.instance_id)
    .bind(&body.hostname)
    .bind(&body.version)
    .bind(body.uptime_seconds)
    .bind(body.applied_revision)
    .bind(&body.config_hash)
    .bind(sqlx::types::Json(&body.supported_guardrail_kinds))
    .bind(sqlx::types::Json(&body.exporter_health))
    .bind(sqlx::types::Json(&body.rejected_resources))
    .bind(sqlx::types::Json(&body.partially_compatible_resources))
    .execute(state.db.pool())
    .await
    .map_err(DbError::from)?;
    Ok(Json(json!({})))
}
