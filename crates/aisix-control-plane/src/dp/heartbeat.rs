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
//!   [`BEATS_PER_SECOND`] after that: what [`MAX_INSTANCES`]
//!   instances beating at the gateway's shortest interval (5 s) send. Past
//!   it a beat is `429 busy` and writes nothing; the gateway logs a warning
//!   and beats again at its next tick. The bucket is per control-plane
//!   process.
//! - at most [`MAX_INSTANCES`] instance rows. A gateway mints an
//!   `instance_id` at every boot, so a crash-looping one leaves a row behind
//!   each time; a new instance past the cap therefore takes the place of
//!   the credential's longest-silent offline instance (not heard from within
//!   `CP_INSTANCE_STALE_SECS`), and the live ones always get in. Only when
//!   every row is online is a new instance refused, `429 instance_limit`,
//!   storing nothing; the time is recorded as the credential's
//!   `instance_cap_hit_at`, since the instances shown are then not the whole
//!   fleet. An evicted instance the gateway-version gate still judges (heard
//!   from within `compat::FLEET_WINDOW`) whose version fails a floor
//!   (`compat::meets_every_floor`) is recorded there too, as of when it was
//!   last heard from: the gate no longer sees it, and keeps judging the fleet
//!   incomplete for as long as it would have judged that instance.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::extract::{FromRequest, Request, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use super::telemetry::BODY_READ_WAIT;
use super::DpIdentity;
use crate::compat::{self, FLEET_WINDOW};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::gateways::stale_window_secs;
use crate::http::ApiJson;
use crate::usage::{text_without_nul, without_nul};
use crate::AppState;

/// An `instance_id` is a UUID; this leaves room for any other id a gateway
/// might mint, and keeps the key short enough to index.
const MAX_INSTANCE_ID_CHARS: usize = 128;

/// Instance rows one credential keeps, at most.
pub const MAX_INSTANCES: i64 = 128;

/// Beats one credential may send at once.
pub const BEAT_BURST: f64 = 128.0;

/// Beats one credential may send per second, after a burst: as many
/// instances as it may have, each beating every 5 s.
pub const BEATS_PER_SECOND: f64 = MAX_INSTANCES as f64 / 5.0;

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

impl HeartbeatBody {
    /// The body with every NUL replaced by U+FFFD, as usage events are
    /// stored: neither `text` nor `jsonb` can hold one, and an exporter's
    /// `last_error` quotes whatever a remote answered. A beat that could
    /// not be stored would drop its instance from the fleet view, and from
    /// the gateway-version gate.
    fn without_nul(self) -> HeartbeatBody {
        let list = |values: Vec<Value>| values.into_iter().map(without_nul).collect();
        HeartbeatBody {
            dp_id: text_without_nul(self.dp_id),
            instance_id: text_without_nul(self.instance_id),
            hostname: text_without_nul(self.hostname),
            version: text_without_nul(self.version),
            uptime_seconds: self.uptime_seconds,
            applied_revision: self.applied_revision,
            config_hash: self.config_hash.map(text_without_nul),
            supported_guardrail_kinds: list(self.supported_guardrail_kinds),
            exporter_health: list(self.exporter_health),
            rejected_resources: list(self.rejected_resources),
            partially_compatible_resources: list(self.partially_compatible_resources),
        }
    }
}

/// A heartbeat's body, as [`ApiJson`] reads it, within [`BODY_READ_WAIT`]
/// as a usage batch's: a gateway that vanished mid-body would otherwise
/// hold its connection, and on shutdown the process, until TCP noticed.
/// Past it the beat is `408`, and the gateway beats again at its next tick.
pub struct Beat(HeartbeatBody);

impl<S: Send + Sync> FromRequest<S> for Beat {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Beat, ApiError> {
        let read = ApiJson::<HeartbeatBody>::from_request(request, state);
        match tokio::time::timeout(BODY_READ_WAIT, read).await {
            Ok(read) => read.map(|ApiJson(body)| Beat(body)),
            Err(_) => {
                tracing::warn!(
                    wait_secs = BODY_READ_WAIT.as_secs(),
                    "a heartbeat body did not finish arriving; answered 408"
                );
                Err(ApiError::request_timeout(BODY_READ_WAIT))
            }
        }
    }
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
    Beat(body): Beat,
) -> Result<Json<Value>, ApiError> {
    let body = body.without_nul();
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
    let stale_secs = stale_window_secs(&state.cfg);
    let cp_version = state.cfg.cp_version.clone();
    let recorded = state
        .db
        .tx(move |conn| {
            async move {
                let known = is_known(conn, dp_id, &body.instance_id).await?;
                if !known
                    && !make_room(conn, dp_id, &body.instance_id, stale_secs, &cp_version).await?
                {
                    // Committed with the refusal: what the fleet view is
                    // missing is known.
                    sqlx::query(
                        "UPDATE dp_credentials SET instance_cap_hit_at = now() WHERE dp_id = $1",
                    )
                    .bind(dp_id)
                    .execute(&mut *conn)
                    .await?;
                    return Ok(false);
                }
                record(conn, dp_id, &body).await?;
                Ok::<_, DbError>(true)
            }
            .boxed()
        })
        .await?;
    if !recorded {
        tracing::warn!(%dp_id, "refused a new gateway instance: every one of the credential's \
            instances is online and it has reached its instance cap");
        return Err(ApiError {
            code: "instance_limit",
            message: format!(
                "此网关凭据的 {MAX_INSTANCES} 个实例都在线，暂不接受新实例；\
                 更多网关请使用另一份凭据"
            ),
            ..ApiError::busy()
        });
    }
    Ok(Json(json!({})))
}

/// Whether `dp_id` already has a row for `instance_id`.
async fn is_known(
    conn: &mut PgConnection,
    dp_id: Uuid,
    instance_id: &str,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM dp_instances WHERE dp_id = $1 AND instance_id = $2)",
    )
    .bind(dp_id)
    .bind(instance_id)
    .fetch_one(conn)
    .await?)
}

/// Whether `dp_id` has room for its new instance `instance_id`. At
/// [`MAX_INSTANCES`] rows it makes room by deleting its least recently seen
/// instance that is offline (silent for longer than `stale_secs`), and has
/// none when every one is online. An evicted instance the version gate
/// still judged, whose version fails a floor by `cp_version`'s judgement,
/// is recorded in `instance_cap_hit_at` (see the module docs).
///
/// The credential's row is locked first, so new instances arriving together
/// take the places there are one at a time; and a first beat of
/// `instance_id` that got in while this one waited is found, rather than
/// counted against it.
async fn make_room(
    conn: &mut PgConnection,
    dp_id: Uuid,
    instance_id: &str,
    stale_secs: f64,
    cp_version: &str,
) -> Result<bool, DbError> {
    sqlx::query("SELECT 1 FROM dp_credentials WHERE dp_id = $1 FOR UPDATE")
        .bind(dp_id)
        .execute(&mut *conn)
        .await?;
    if is_known(conn, dp_id, instance_id).await? {
        return Ok(true);
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM dp_instances WHERE dp_id = $1")
        .bind(dp_id)
        .fetch_one(&mut *conn)
        .await?;
    if rows < MAX_INSTANCES {
        return Ok(true);
    }
    let evicted: Option<(String, DateTime<Utc>, bool)> = sqlx::query_as(&format!(
        "DELETE FROM dp_instances WHERE (dp_id, instance_id) IN ( \
           SELECT dp_id, instance_id FROM dp_instances \
           WHERE dp_id = $1 AND last_seen_at < now() - make_interval(secs => $2) \
           ORDER BY last_seen_at LIMIT 1) \
         RETURNING version, last_seen_at, last_seen_at > now() - {FLEET_WINDOW}"
    ))
    .bind(dp_id)
    .bind(stale_secs)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((version, last_seen_at, judged)) = evicted else {
        return Ok(false);
    };
    tracing::debug!(%dp_id, "a new gateway instance took the place of the longest-silent offline one");
    if judged && !compat::meets_every_floor(&version, cp_version) {
        sqlx::query(
            "UPDATE dp_credentials \
             SET instance_cap_hit_at = GREATEST(instance_cap_hit_at, $2) WHERE dp_id = $1",
        )
        .bind(dp_id)
        .bind(last_seen_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(true)
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

    use axum::body::Body;
    use axum::http::{header, StatusCode};
    use bytes::Bytes;

    use super::*;

    /// A beat whose body stops arriving is answered `408` once
    /// [`BODY_READ_WAIT`] has passed, and not before. The clock is paused,
    /// so the wait costs no real time.
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_body_that_stops_arriving_is_408_after_the_read_wait() {
        let stalled = futures::stream::pending::<Result<Bytes, std::io::Error>>();
        let request = Request::builder()
            .method("POST")
            .uri("/dp/heartbeat")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(stalled))
            .expect("a request");
        let started = tokio::time::Instant::now();
        let read = tokio::time::timeout(BODY_READ_WAIT * 6, Beat::from_request(request, &()))
            .await
            .expect("the read gave up within its wait");
        let Err(refused) = read else {
            panic!("a body that never arrived was read");
        };
        assert_eq!(refused.status, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(refused.code, "request_timeout");
        assert_eq!(started.elapsed(), BODY_READ_WAIT);
    }

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
