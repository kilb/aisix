//! Stored usage: the events gateways deliver over `POST /dp/telemetry`
//! ([`ingest`]), and the operators' view of them (`/admin/usage/*`).
//!
//! # Each event once
//!
//! Two keys keep a count from doubling, and both are written in the one
//! transaction that stores the events (kine recon §6: no lock beyond the
//! unique indexes is needed):
//! - the batch, `(dp_id, batch_id)`: a gateway re-sends a batch with the id
//!   it minted for it when an answer is lost, and a batch already recorded
//!   stores nothing again. A delivery that arrives while another delivery of
//!   the same batch is still committing waits for it on the batch row's
//!   unique index: once that one commits, the waiting one is a re-send; if
//!   it rolls back, the waiting one stores the batch.
//! - the event, `event_id`: the gateway mints one per row, so an event that
//!   reaches the control plane in two batches is stored once. `request_id`
//!   is never a key: a caller chooses it, and every attempt of a request
//!   shares it.
//!
//! # What is stored
//!
//! Events are decoded loosely, since a gateway may be newer than this
//! control plane: an event needs an `event_id` (1 to 128 characters) and an
//! RFC 3339 `occurred_at`, and is kept whole, unknown fields included.
//! `team_id`, `api_key_id` and `request_id` are copied into columns to be
//! filtered on, when they are text of at most 256 characters. An event
//! without what it needs is refused and kept as sent in `usage_rejects`,
//! with the reason; refusing it is final, so the gateway is told how many
//! there were and does not re-send them.
//!
//! Nothing an event carries may fail its batch, or every event in it would
//! be lost with it once the gateway gives up re-sending. So a NUL character,
//! which neither `jsonb` nor `text` can hold, is stored as U+FFFD, and a
//! filter value too long to index stays only in the event.
//!
//! Logs and errors name counts and ids, never what an event contains.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::auth::{OperatorAuth, OperatorRole};
use crate::db::{Db, DbError};
use crate::error::{ApiError, Detail};
use crate::AppState;

/// The most characters an `event_id` may have.
const MAX_EVENT_ID_CHARS: usize = 128;

/// The most characters of a `team_id`, `api_key_id` or `request_id` copied
/// into its filter column: an index entry stays far below the size Postgres
/// can index, and it is the gateway's own cap on a request id.
const MAX_FILTER_CHARS: usize = 256;

/// What one delivery of a batch stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestOutcome {
    /// Events stored by this delivery.
    pub inserted: u64,
    /// Events already stored, by an earlier batch or earlier in this one.
    pub duplicates: u64,
    /// Events refused, and kept in `usage_rejects`.
    pub rejected: u64,
    /// The batch had been delivered before: nothing was stored.
    pub batch_duplicate: bool,
}

/// An event that can be stored.
struct Row {
    event_id: String,
    occurred_at: DateTime<Utc>,
    team_id: Option<String>,
    api_key_id: Option<String>,
    request_id: Option<String>,
    event: Value,
}

/// An event that cannot be, and why.
struct Reject {
    reason: &'static str,
    raw: Value,
}

/// Stores the events of `dp_id`'s batch `batch_id`, once (see the module
/// docs), in one transaction: either all of it is stored, the batch id
/// included, or nothing is, and the batch can be delivered again.
pub async fn ingest(
    db: &Db,
    dp_id: Uuid,
    batch_id: Uuid,
    events: Vec<Value>,
) -> Result<IngestOutcome, DbError> {
    let total = events.len();
    let (mut rows, mut rejects) = (Vec::new(), Vec::new());
    for event in events {
        match classify(event) {
            Ok(row) => rows.push(row),
            Err(reject) => rejects.push(reject),
        }
    }
    db.tx(move |conn| store(conn, dp_id, batch_id, total, rows, rejects).boxed())
        .await
}

async fn store(
    conn: &mut PgConnection,
    dp_id: Uuid,
    batch_id: Uuid,
    total: usize,
    rows: Vec<Row>,
    rejects: Vec<Reject>,
) -> Result<IngestOutcome, DbError> {
    // A row this statement inserted has no xmax; one it updated carries
    // this transaction's. A delivery that found the row uncommitted waited
    // for it first, and sees it as updated once it has committed.
    let fresh: bool = sqlx::query_scalar(
        "INSERT INTO usage_batches (dp_id, batch_id) VALUES ($1, $2) \
         ON CONFLICT (dp_id, batch_id) DO UPDATE SET deliveries = usage_batches.deliveries + 1 \
         RETURNING (xmax = 0)",
    )
    .bind(dp_id)
    .bind(batch_id)
    .fetch_one(&mut *conn)
    .await?;
    if !fresh {
        return Ok(IngestOutcome {
            batch_duplicate: true,
            ..IngestOutcome::default()
        });
    }

    let rejected = rejects.len();
    if !rejects.is_empty() {
        let (reasons, raws): (Vec<&str>, Vec<Value>) =
            rejects.into_iter().map(|r| (r.reason, r.raw)).unzip();
        sqlx::query(
            "INSERT INTO usage_rejects (dp_id, batch_id, reason, raw) \
             SELECT $1, $2, u.reason, u.raw \
             FROM unnest($3::text[], $4::jsonb[]) WITH ORDINALITY AS u(reason, raw, n) \
             ORDER BY u.n",
        )
        .bind(dp_id)
        .bind(batch_id)
        .bind(reasons)
        .bind(raws)
        .execute(&mut *conn)
        .await?;
    }

    let valid = rows.len();
    let mut inserted = 0;
    if !rows.is_empty() {
        let mut columns = EventColumns::default();
        for row in rows {
            columns.push(row);
        }
        // In event_id order, so two batches sharing events wait for each
        // other in the same order and cannot deadlock.
        inserted = sqlx::query(
            "INSERT INTO usage_events \
             (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, request_id, event) \
             SELECT u.event_id, $1, $2, u.occurred_at, u.team_id, u.api_key_id, u.request_id, u.event \
             FROM unnest($3::text[], $4::timestamptz[], $5::text[], $6::text[], $7::text[], \
                         $8::jsonb[]) \
               AS u(event_id, occurred_at, team_id, api_key_id, request_id, event) \
             ORDER BY u.event_id \
             ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(dp_id)
        .bind(batch_id)
        .bind(columns.event_ids)
        .bind(columns.occurred_at)
        .bind(columns.team_ids)
        .bind(columns.api_key_ids)
        .bind(columns.request_ids)
        .bind(columns.events)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    let outcome = IngestOutcome {
        inserted,
        duplicates: (valid as u64).saturating_sub(inserted),
        rejected: rejected as u64,
        batch_duplicate: false,
    };

    sqlx::query(
        "UPDATE usage_batches SET events = $3, inserted = $4, duplicates = $5, rejected = $6 \
         WHERE dp_id = $1 AND batch_id = $2",
    )
    .bind(dp_id)
    .bind(batch_id)
    .bind(total as i64)
    .bind(outcome.inserted as i64)
    .bind(outcome.duplicates as i64)
    .bind(outcome.rejected as i64)
    .execute(&mut *conn)
    .await?;
    Ok(outcome)
}

/// The rows to store, a column at a time, as `unnest` takes them.
#[derive(Default)]
struct EventColumns {
    event_ids: Vec<String>,
    occurred_at: Vec<DateTime<Utc>>,
    team_ids: Vec<Option<String>>,
    api_key_ids: Vec<Option<String>>,
    request_ids: Vec<Option<String>>,
    events: Vec<Value>,
}

impl EventColumns {
    fn push(&mut self, row: Row) {
        self.event_ids.push(row.event_id);
        self.occurred_at.push(row.occurred_at);
        self.team_ids.push(row.team_id);
        self.api_key_ids.push(row.api_key_id);
        self.request_ids.push(row.request_id);
        self.events.push(row.event);
    }
}

/// Whether `event` can be stored, and what of it goes into the columns.
fn classify(event: Value) -> Result<Row, Reject> {
    let event = without_nul(event);
    let Some(fields) = event.as_object() else {
        return Err(Reject {
            reason: "not_an_object",
            raw: event,
        });
    };
    let (event_id, occurred_at) = match required(fields) {
        Ok(found) => found,
        Err(reason) => return Err(Reject { reason, raw: event }),
    };
    let filter = |name: &str| {
        fields
            .get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty() && v.chars().count() <= MAX_FILTER_CHARS)
            .map(str::to_string)
    };
    Ok(Row {
        event_id,
        occurred_at,
        team_id: filter("team_id"),
        api_key_id: filter("api_key_id"),
        request_id: filter("request_id"),
        event,
    })
}

/// The `event_id` and `occurred_at` every stored event has, or the reason
/// an event is refused.
fn required(fields: &Map<String, Value>) -> Result<(String, DateTime<Utc>), &'static str> {
    let event_id = match fields.get("event_id").and_then(Value::as_str) {
        None | Some("") => return Err("event_id_missing"),
        Some(id) if id.chars().count() > MAX_EVENT_ID_CHARS => return Err("event_id_too_long"),
        Some(id) => id.to_string(),
    };
    let occurred_at = fields
        .get("occurred_at")
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .ok_or("occurred_at_invalid")?;
    Ok((event_id, occurred_at.with_timezone(&Utc)))
}

/// `value` with every NUL in its strings and keys replaced by U+FFFD.
/// Bounded in depth by the JSON parser's own nesting limit.
fn without_nul(value: Value) -> Value {
    fn text(s: String) -> String {
        if s.contains('\0') {
            s.replace('\0', "\u{fffd}")
        } else {
            s
        }
    }
    match value {
        Value::String(s) => Value::String(text(s)),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nul).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (text(k), without_nul(v)))
                .collect(),
        ),
        other => other,
    }
}

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

fn query_rejected(_: QueryRejection) -> ApiError {
    ApiError::validation(vec![Detail {
        path: "query".to_string(),
        message: "查询参数不合法".to_string(),
    }])
}

#[derive(Deserialize)]
pub struct EventsQuery {
    limit: Option<i64>,
    /// Only rows with a smaller `seq`: the `next_before` of the previous
    /// page.
    before: Option<i64>,
    team_id: Option<String>,
    api_key_id: Option<String>,
    request_id: Option<String>,
}

type EventRow = (
    i64,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
    Uuid,
    Uuid,
    Option<String>,
    Option<String>,
    Option<String>,
    Value,
);

/// `GET /admin/usage/events`: stored events, newest first by `seq`,
/// `limit` (default 50, at most 500) at a time, optionally only those of a
/// `team_id`, an `api_key_id` or a `request_id`. `next_before` is the
/// `before` of the next page, null on the last. `seq` orders rows for
/// paging only: it is taken before a row commits, so a row can appear
/// behind a page already read.
pub async fn list_events(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<EventsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // One row more than the page, to tell whether another page follows.
    let mut rows: Vec<EventRow> = sqlx::query_as(
        "SELECT seq, event_id, occurred_at, received_at, dp_id, batch_id, team_id, api_key_id, \
                request_id, event \
         FROM usage_events \
         WHERE ($1::bigint IS NULL OR seq < $1) \
           AND ($2::text IS NULL OR team_id = $2) \
           AND ($3::text IS NULL OR api_key_id = $3) \
           AND ($4::text IS NULL OR request_id = $4) \
         ORDER BY seq DESC LIMIT $5",
    )
    .bind(q.before)
    .bind(q.team_id)
    .bind(q.api_key_id)
    .bind(q.request_id)
    .bind(limit + 1)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_before = if more { rows.last().map(|r| r.0) } else { None };
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                seq,
                event_id,
                occurred_at,
                received_at,
                dp_id,
                batch_id,
                team_id,
                api_key_id,
                request_id,
                event,
            )| {
                json!({
                    "seq": seq,
                    "event_id": event_id,
                    "occurred_at": occurred_at,
                    "received_at": received_at,
                    "dp_id": dp_id,
                    "batch_id": batch_id,
                    "team_id": team_id,
                    "api_key_id": api_key_id,
                    "request_id": request_id,
                    "event": event,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

#[derive(Deserialize)]
pub struct BatchesQuery {
    limit: Option<i64>,
    /// Only rows with a smaller `seq`: the `next_before` of the previous
    /// page.
    before: Option<i64>,
}

type BatchRow = (i64, Uuid, Uuid, DateTime<Utc>, i64, i64, i64, i64, i64);

/// `GET /admin/usage/batches`: the batches gateways delivered, newest first
/// by `seq`, paged as [`list_events`] is. Each counts its events, what its
/// first stored delivery made of them, and how many times it was delivered.
pub async fn list_batches(
    auth: OperatorAuth,
    State(state): State<AppState>,
    query: Result<Query<BatchesQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let Query(q) = query.map_err(query_rejected)?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut rows: Vec<BatchRow> = sqlx::query_as(
        "SELECT seq, dp_id, batch_id, received_at, events, inserted, duplicates, rejected, \
                deliveries \
         FROM usage_batches \
         WHERE ($1::bigint IS NULL OR seq < $1) \
         ORDER BY seq DESC LIMIT $2",
    )
    .bind(q.before)
    .bind(limit + 1)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let next_before = if more { rows.last().map(|r| r.0) } else { None };
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                seq,
                dp_id,
                batch_id,
                received_at,
                events,
                inserted,
                duplicates,
                rejected,
                deliveries,
            )| {
                json!({
                    "seq": seq,
                    "dp_id": dp_id,
                    "batch_id": batch_id,
                    "received_at": received_at,
                    "events": events,
                    "inserted": inserted,
                    "duplicates": duplicates,
                    "rejected": rejected,
                    "deliveries": deliveries,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "items": items, "next_before": next_before })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(event: Value) -> Option<&'static str> {
        classify(event).err().map(|r| r.reason)
    }

    #[test]
    fn an_event_needs_an_event_id_and_an_rfc_3339_occurred_at() {
        let at = "2026-11-06T08:15:30.123Z";
        assert_eq!(reason(json!({"event_id": "e", "occurred_at": at})), None);
        assert_eq!(
            reason(json!({"event_id": "k".repeat(128), "occurred_at": at})),
            None
        );
        assert_eq!(
            reason(json!({"event_id": "k".repeat(129), "occurred_at": at})),
            Some("event_id_too_long")
        );
        // Counted in characters, not bytes.
        assert_eq!(
            reason(json!({"event_id": "é".repeat(128), "occurred_at": at})),
            None
        );
        for id in [json!(""), json!(7), json!(null)] {
            assert_eq!(
                reason(json!({"event_id": id, "occurred_at": at})),
                Some("event_id_missing")
            );
        }
        for at in [
            json!("2026-11-06"),
            json!("yesterday"),
            json!(1_700_000_000),
        ] {
            assert_eq!(
                reason(json!({"event_id": "e", "occurred_at": at})),
                Some("occurred_at_invalid")
            );
        }
        assert_eq!(reason(json!(["e"])), Some("not_an_object"));
    }

    #[test]
    fn only_short_text_is_copied_into_the_filter_columns() {
        let row = classify(json!({
            "event_id": "e",
            "occurred_at": "2026-11-06T08:15:30+08:00",
            "team_id": "t".repeat(256),
            "api_key_id": "k".repeat(257),
            "request_id": 7,
        }))
        .ok()
        .expect("storable");
        assert_eq!(row.team_id, Some("t".repeat(256)));
        assert_eq!((row.api_key_id, row.request_id), (None, None));
        assert_eq!(row.occurred_at.to_rfc3339(), "2026-11-06T00:15:30+00:00");
    }

    #[test]
    fn a_nul_is_replaced_wherever_it_is() {
        let value = without_nul(json!({"a\u{0}": ["x\u{0}y", {"k": "\u{0}"}], "n": 1}));
        assert_eq!(
            value,
            json!({"a\u{fffd}": ["x\u{fffd}y", {"k": "\u{fffd}"}], "n": 1})
        );
    }
}
