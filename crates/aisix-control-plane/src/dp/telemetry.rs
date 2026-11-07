//! `POST /dp/telemetry`: a gateway delivers the usage events it recorded,
//! in batches, and each is stored once ([`crate::usage::ingest`]).
//!
//! The gateway's sender (`crates/aisix-server/src/telemetry.rs`) posts
//! `{"events": [...]}` of at most 100 events, about every 250 ms, one batch
//! at a time, with the id it minted for the batch in
//! `X-Aisix-Usage-Batch-Id`. It re-sends a batch with the same id after a
//! failure only while the answer it last got carried
//! `X-Aisix-Usage-Batch-Dedup: 1`, so every answer here carries it, errors
//! and the framework's own rejections included ([`mark_dedup`]). What it
//! does with an answer:
//! - `2xx`: delivered; it reads `rejected` from the body and warns when it
//!   is not 0, since those events are lost.
//! - `5xx`, `408`, `429`: re-sent, with backoff, for up to 30 minutes of
//!   event age or 8 answered failures.
//! - any other `4xx`: dropped. So `400` (no usable batch id, a body that is
//!   not a batch) and `413` (too large) are for batches that re-sending
//!   would not fix, and a body that stopped arriving is `408`.
//!
//! A store that failed is answered by whether sending the same batch again
//! can get past the failure ([`transient`]): `503` when it can (a lost
//! connection, a failover, contention, an exhausted server), so the batch is
//! stored on a later delivery and never dropped for it; `500`, with a neutral
//! message, for anything else, which would fail the same way every time.
//!
//! # Bounds
//!
//! A body is read only once the credential has been checked, up to
//! [`MAX_BODY_BYTES`], and holds at most [`MAX_EVENTS_PER_BATCH`] events:
//! ten times the gateway's largest batch. Past the cap the rest are skipped
//! while the body is read, not held. Storing a batch takes four statements
//! whatever its size, and no lock beyond the rows it writes.
//!
//! At most [`MAX_BODIES_IN_FLIGHT`] bodies are held at once ([`BodySlot`]):
//! a delivery takes a slot before its body is read and keeps it until its
//! store ends, even if the gateway stops waiting for the answer. One that
//! finds no slot free within [`SLOT_WAIT`] is answered `503` unread, and one
//! whose body has not arrived within [`BODY_READ_WAIT`] of taking its slot
//! is answered `408` (which the gateway re-sends), freeing the slot.
//!
//! There is no per-credential rate limit, unlike `/dp/heartbeat`. A
//! gateway process sends one batch at a time and waits for its answer, so
//! its rate follows how fast batches are stored; and every `429` would
//! count against its 8 answered failures, turning load into lost usage.
//! What one credential can make the control plane do is bounded instead by
//! the caps and slots above, and by the database pool, which every store
//! queues for.

use std::time::Duration;

use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use tokio::sync::OwnedSemaphorePermit;
use uuid::Uuid;

use super::DpIdentity;
use crate::db::DbError;
use crate::error::ApiError;
use crate::{usage, AppState};

/// The largest body a batch may have.
pub const MAX_BODY_BYTES: usize = 8 << 20;

/// The most events one batch may carry.
pub const MAX_EVENTS_PER_BATCH: usize = 1000;

/// The batch bodies one process holds at once, from when it starts reading
/// one until its store ends.
pub const MAX_BODIES_IN_FLIGHT: usize = 32;

/// How long a delivery waits for one of the [`MAX_BODIES_IN_FLIGHT`] slots
/// before it is answered `503`: below the gateway's 10 s request timeout, so
/// it gets an answer rather than a timeout.
pub const SLOT_WAIT: Duration = Duration::from_secs(8);

/// How long a delivery's body may take to arrive once its slot is taken,
/// before it is answered `408` and the slot freed.
pub const BODY_READ_WAIT: Duration = Duration::from_secs(10);

/// The id the gateway minted for a batch, the same on each re-send.
pub const BATCH_ID_HEADER: &str = "x-aisix-usage-batch-id";

/// Says that this control plane recognises a re-sent batch, so re-sending
/// one cannot count it twice. Its one defined value is `1`.
pub const DEDUP_HEADER: HeaderName = HeaderName::from_static("x-aisix-usage-batch-dedup");

/// `POST /dp/telemetry` → `200 {"accepted", "rejected", "duplicates"}`:
/// the events this delivery stored, refused, and found stored already. A
/// batch delivered before stores nothing and is answered with the events
/// its first stored delivery refused, so a gateway whose answer was lost
/// still learns of them.
pub async fn telemetry(
    dp: DpIdentity,
    State(state): State<AppState>,
    BatchId(batch_id): BatchId,
    BodySlot(slot): BodySlot,
    Events(events): Events,
) -> Result<Json<Value>, ApiError> {
    let dp_id = dp.dp_id;
    let count = events.len();
    let outcome = usage::ingest(&state.db, dp_id, batch_id, events, slot)
        .await
        .map_err(|e| {
            if transient(&e) {
                tracing::warn!(%dp_id, %batch_id, events = count, cause = %cause(&e),
                    "could not store a usage batch for now; answered 503, so the gateway \
                     sends it again");
                ApiError::unavailable()
            } else {
                // Logged there, by its cause alone.
                ApiError::internal(format!(
                    "could not store usage batch {batch_id} of {dp_id} ({count} events): {}",
                    cause(&e)
                ))
            }
        })?;
    if outcome.batch_duplicate {
        tracing::debug!(%dp_id, %batch_id, events = count,
            "a usage batch was delivered again; it had been stored");
    } else if outcome.rejected > 0 {
        tracing::warn!(%dp_id, %batch_id, events = count, rejected = outcome.rejected,
            "refused usage events without a usable event_id or occurred_at; they are kept \
             in usage_rejects");
    }
    Ok(Json(json!({
        "accepted": outcome.inserted,
        "rejected": outcome.rejected,
        "duplicates": outcome.duplicates,
    })))
}

/// Sets [`DEDUP_HEADER`] on every answer of the route it wraps.
pub async fn mark_dedup(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(DEDUP_HEADER, HeaderValue::from_static("1"));
    response
}

/// Whether sending the same batch again can get past `e`: contention, a
/// lost or refused connection, or a server that is shutting down, failing
/// over or out of resources. Anything else would fail the same way on every
/// re-send.
pub(crate) fn transient(e: &DbError) -> bool {
    match e {
        DbError::LockTimeout => true,
        DbError::Db(
            sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::WorkerCrashed,
        ) => true,
        DbError::Db(sqlx::Error::Database(db)) => db.code().is_some_and(|c| transient_sqlstate(&c)),
        _ => false,
    }
}

/// SQLSTATE classes 08 (connection exception), 40 (transaction rollback:
/// serialisation failure, deadlock), 53 (insufficient resources), 57
/// (operator intervention: shutdown, cancel, statement timeout) and 58
/// (system error: an I/O failure on the server), and 25006 (a read-only
/// transaction, as on a server that has just become a standby).
fn transient_sqlstate(code: &str) -> bool {
    matches!(code.get(..2), Some("08" | "40" | "53" | "57" | "58")) || code == "25006"
}

/// What a failed store is logged as: a database error by its SQLSTATE, as
/// its message can quote a value it refused.
fn cause(e: &DbError) -> String {
    match e {
        DbError::Db(sqlx::Error::Database(db)) => {
            format!("database error {}", db.code().unwrap_or_default())
        }
        other => other.to_string(),
    }
}

/// A `400` for a request that is not a batch.
fn bad_request(message: &str) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        code: "bad_request",
        message: message.to_string(),
        ..ApiError::unavailable()
    }
}

/// The batch's `X-Aisix-Usage-Batch-Id`, a UUID; `400` when it is missing
/// or is not one. Read before the body.
pub struct BatchId(pub Uuid);

impl<S: Send + Sync> FromRequestParts<S> for BatchId {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<BatchId, ApiError> {
        parts
            .headers
            .get(BATCH_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Uuid::parse_str(v).ok())
            .map(BatchId)
            .ok_or_else(|| bad_request("X-Aisix-Usage-Batch-Id 请求头缺失或不是 UUID"))
    }
}

/// One of the [`MAX_BODIES_IN_FLIGHT`] slots: taken before the body is
/// read, and handed to the store, which holds it until its transaction ends.
/// `503` when none frees within [`SLOT_WAIT`]; slots are handed out in the
/// order deliveries asked for them.
pub struct BodySlot(pub OwnedSemaphorePermit);

impl FromRequestParts<AppState> for BodySlot {
    type Rejection = ApiError;

    async fn from_request_parts(_: &mut Parts, state: &AppState) -> Result<BodySlot, ApiError> {
        let slots = state.telemetry_bodies.clone();
        match tokio::time::timeout(SLOT_WAIT, slots.acquire_owned()).await {
            Ok(Ok(slot)) => Ok(BodySlot(slot)),
            // Timed out: the semaphore is never closed.
            _ => {
                tracing::warn!(
                    wait_secs = SLOT_WAIT.as_secs(),
                    "every usage batch slot stayed taken; answered a delivery 503 unread"
                );
                Err(ApiError::unavailable())
            }
        }
    }
}

/// The batch's events, each as sent: `400` for a body that is not
/// `{"events": [...]}` JSON, `413` for one over [`MAX_BODY_BYTES`] or
/// [`MAX_EVENTS_PER_BATCH`] events, `408` for one that has not arrived
/// whole within [`BODY_READ_WAIT`]. Other fields of the body are ignored.
pub struct Events(pub Vec<Value>);

#[derive(Deserialize)]
struct Batch {
    events: Capped,
}

/// A JSON array, of which at most [`MAX_EVENTS_PER_BATCH`] elements are
/// kept; `over_cap` says whether there were more.
struct Capped {
    events: Vec<Value>,
    over_cap: bool,
}

impl<'de> Deserialize<'de> for Capped {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Capped, D::Error> {
        struct CappedVisitor;

        impl<'de> Visitor<'de> for CappedVisitor {
            type Value = Capped;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of usage events")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Capped, A::Error> {
                let mut events =
                    Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_EVENTS_PER_BATCH));
                while events.len() < MAX_EVENTS_PER_BATCH {
                    match seq.next_element::<Value>()? {
                        Some(event) => events.push(event),
                        None => {
                            return Ok(Capped {
                                events,
                                over_cap: false,
                            })
                        }
                    }
                }
                // At the cap, the rest is read past and not kept.
                let mut over_cap = false;
                while seq.next_element::<IgnoredAny>()?.is_some() {
                    over_cap = true;
                }
                Ok(Capped { events, over_cap })
            }
        }

        deserializer.deserialize_seq(CappedVisitor)
    }
}

impl<S: Send + Sync> FromRequest<S> for Events {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Events, ApiError> {
        // A body that stops arriving would hold its slot for as long as its
        // connection stays open.
        let read =
            tokio::time::timeout(BODY_READ_WAIT, Json::<Batch>::from_request(request, state));
        let Ok(read) = read.await else {
            tracing::warn!(
                wait_secs = BODY_READ_WAIT.as_secs(),
                "a usage batch body did not finish arriving; answered 408 and freed its slot"
            );
            return Err(ApiError::request_timeout(BODY_READ_WAIT));
        };
        match read {
            Ok(Json(batch)) if batch.events.over_cap => Err(ApiError {
                message: format!("一个用量批次最多 {MAX_EVENTS_PER_BATCH} 条事件"),
                ..ApiError::too_large()
            }),
            Ok(Json(batch)) => Ok(Events(batch.events.events)),
            Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Err(ApiError::too_large())
            }
            // The rejection's own text can quote the body.
            Err(_) => Err(bad_request(
                "请求体必须是 {\"events\": [...]} 形式的 JSON（Content-Type: application/json）",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_transient_when_a_re_send_can_get_past_it() {
        for code in [
            "08000", "08006", "08P01", "40001", "40P01", "53100", "53200", "53300", "57014",
            "57P01", "57P03", "25006", "58000", "58030",
        ] {
            assert!(transient_sqlstate(code), "{code}");
        }
        for code in [
            "23505", "23514", "22P05", "22001", "42P01", "42601", "54000", "P0001", "25001",
            "XX000", "0", "",
        ] {
            assert!(!transient_sqlstate(code), "{code}");
        }
        assert!(transient(&DbError::LockTimeout));
        for e in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::WorkerCrashed,
            sqlx::Error::Io(std::io::Error::other("reset")),
            sqlx::Error::Tls(Box::new(std::io::Error::other("handshake"))),
        ] {
            assert!(transient(&DbError::Db(e)));
        }
        assert!(!transient(&DbError::UniqueViolation {
            constraint: "usage_events_pkey".to_string()
        }));
        assert!(!transient(&DbError::Db(sqlx::Error::RowNotFound)));
        assert!(!transient(&DbError::Db(sqlx::Error::Protocol(
            "unexpected".to_string()
        ))));
    }
}
