//! `POST /dp/heartbeat`: each gateway process reports itself every
//! `heartbeat_interval_secs` (15 s by default), keyed by its credential and
//! the `instance_id` it mints at boot, so replicas sharing a credential are
//! separate instances.
//!
//! The body is the gateway's `HeartbeatBody`
//! (`crates/aisix-server/src/heartbeat.rs`), decoded loosely: only `dp_id`
//! and `instance_id` are required, anything else missing takes its default
//! and anything unknown is ignored, since a gateway may be newer than this
//! control plane: an explicit `null` counts as absent. The gateway reads only
//! the status of the answer.
//!
//! # What one credential can do
//!
//! A credential's holder is trusted to report, not to flood:
//! - its beats pass a token bucket per `dp_id`, [`BEAT_BURST`] at once and
//!   [`BEATS_PER_SECOND`] after that: what [`MAX_INSTANCES_PER_DAY`]
//!   instances beating at the gateway's shortest interval (5 s) send. Past
//!   it a beat is `429 busy` and writes nothing; the gateway logs a warning
//!   and beats again at its next tick. The bucket is per control-plane
//!   process.
//! - at most [`MAX_INSTANCES_PER_DAY`] of its instances heard from in the
//!   last day: a beat from a new `instance_id` past that is `429
//!   instance_limit` and stores nothing, so one credential cannot fill the
//!   instance table with ids. A gateway mints an id at every boot, so a fleet
//!   of R replicas restarted K times a day uses R × (K + 1); 128 leaves room
//!   for one that rolls often.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::extract::State;
use axum::Json;
use futures::FutureExt;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::DpIdentity;
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::ApiJson;
use crate::AppState;

/// An `instance_id` is a UUID; this leaves room for any other id a gateway
/// might mint, and keeps the key short enough to index.
const MAX_INSTANCE_ID_CHARS: usize = 128;

/// Instances of one credential heard from in the last day, at most.
pub const MAX_INSTANCES_PER_DAY: i64 = 128;

/// Beats one credential may send at once.
pub const BEAT_BURST: f64 = 128.0;

/// Beats one credential may send per second, after a burst: as many
/// instances as it may have, each beating every 5 s.
pub const BEATS_PER_SECOND: f64 = MAX_INSTANCES_PER_DAY as f64 / 5.0;

/// The beats each credential may still send: a token bucket per `dp_id`,
/// in this process. Cheap to clone: clones share the buckets. A bucket is
/// kept for each credential that has beaten, which only listed credentials
/// can.
#[derive(Clone, Default)]
pub struct BeatLimiter {
    buckets: Arc<Mutex<HashMap<Uuid, (f64, Instant)>>>,
}

impl BeatLimiter {
    /// Takes a beat from `dp_id`'s bucket, if one is left.
    pub fn admit(&self, dp_id: Uuid) -> bool {
        self.admit_at(dp_id, Instant::now())
    }

    fn admit_at(&self, dp_id: Uuid, now: Instant) -> bool {
        // A bucket is a pair of numbers, never left half written.
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let (left, at) = buckets.entry(dp_id).or_insert((BEAT_BURST, now));
        let refill = now.saturating_duration_since(*at).as_secs_f64() * BEATS_PER_SECOND;
        *left = (*left + refill).min(BEAT_BURST);
        *at = now;
        if *left < 1.0 {
            return false;
        }
        *left -= 1.0;
        true
    }
}

/// What a heartbeat reports. The lists are kept as sent: what a gateway
/// reports in them is for the views that show it.
#[derive(Deserialize)]
pub struct HeartbeatBody {
    dp_id: String,
    instance_id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    hostname: String,
    #[serde(default, deserialize_with = "null_as_default")]
    version: String,
    #[serde(default, deserialize_with = "null_as_default")]
    uptime_seconds: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    applied_revision: i64,
    /// Omitted until the gateway has applied a configuration.
    #[serde(default)]
    config_hash: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    supported_guardrail_kinds: Vec<Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    exporter_health: Vec<Value>,
    /// Omitted when empty.
    #[serde(default, deserialize_with = "null_as_default")]
    rejected_resources: Vec<Value>,
    /// Omitted when empty.
    #[serde(default, deserialize_with = "null_as_default")]
    partially_compatible_resources: Vec<Value>,
}

/// A `null` decodes as the type's default, as an absent field does.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// `POST /dp/heartbeat` → `200 {}`. The instance's row takes everything
/// this beat reports, so a list the gateway now omits (it omits empty ones)
/// is empty, and a rejection it no longer reports is gone. A body naming
/// another credential's `dp_id` is `403`; a credential over its caps (see
/// the module docs) is `429`.
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
    if !state.beats.admit(dp.dp_id) {
        return Err(ApiError::busy());
    }
    let dp_id = dp.dp_id;
    state
        .db
        .tx(move |conn| {
            async move {
                let known: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM dp_instances \
                     WHERE dp_id = $1 AND instance_id = $2)",
                )
                .bind(dp_id)
                .bind(&body.instance_id)
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if !known {
                    admit_new_instance(conn, dp_id).await?;
                }
                record(conn, dp_id, &body).await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(Json(json!({})))
}

/// Refuses a new instance of `dp_id` once it has [`MAX_INSTANCES_PER_DAY`]
/// heard from in the last day. The credential's row is locked first, so
/// two new instances arriving together cannot both take the last place.
async fn admit_new_instance(conn: &mut PgConnection, dp_id: Uuid) -> Result<(), ApiError> {
    sqlx::query("SELECT 1 FROM dp_credentials WHERE dp_id = $1 FOR UPDATE")
        .bind(dp_id)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dp_instances \
         WHERE dp_id = $1 AND last_seen_at > now() - interval '1 day'",
    )
    .bind(dp_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    if recent < MAX_INSTANCES_PER_DAY {
        return Ok(());
    }
    tracing::warn!(%dp_id, "refused a new gateway instance: the credential has reached its daily instance cap");
    Err(ApiError {
        code: "instance_limit",
        message: format!(
            "此网关凭据最近一天内已有 {MAX_INSTANCES_PER_DAY} 个实例上报，暂不接受新实例；\
             更多网关请使用另一份凭据"
        ),
        ..ApiError::busy()
    })
}

/// Upserts the instance's row with everything `body` reports.
async fn record(conn: &mut PgConnection, dp_id: Uuid, body: &HeartbeatBody) -> Result<(), DbError> {
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
    .bind(dp_id)
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
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_bucket_allows_a_burst_then_its_rate() {
        let limiter = BeatLimiter::default();
        let (dp, other) = (Uuid::new_v4(), Uuid::new_v4());
        let start = Instant::now();
        for beat in 0..128 {
            assert!(limiter.admit_at(dp, start), "beat {beat} of the burst");
        }
        assert!(!limiter.admit_at(dp, start), "a beat past the burst");
        // Another credential has a bucket of its own.
        assert!(limiter.admit_at(other, start));
        // Refilled at its rate: 128 per 5 s, so 25 after a second.
        let later = start + Duration::from_secs(1);
        for beat in 0..25 {
            assert!(limiter.admit_at(dp, later), "refilled beat {beat}");
        }
        assert!(!limiter.admit_at(dp, later), "more than a second's refill");
        // And never more than a burst, however long it waited.
        let much_later = later + Duration::from_secs(3600);
        for _ in 0..128 {
            assert!(limiter.admit_at(dp, much_later));
        }
        assert!(!limiter.admit_at(dp, much_later));
    }
}
