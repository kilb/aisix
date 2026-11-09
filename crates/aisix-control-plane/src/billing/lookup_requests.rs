//! `GET /dp/v1/keys/{key_id}/requests?id=`: a gateway asks what one request
//! id cost a key's holder, so it can tell its caller.
//!
//! A request id is the caller's own input, not unique, and one request
//! leaves one usage row per upstream attempt. So the answer lists every row
//! the key's own tenant recorded under the id that the key may see, newest
//! first, at most [`MAX_REQUEST_ROWS`], with the sum of what they were
//! charged. "Newest" is by when each attempt occurred (`occurred_at`), ties
//! by arrival (`seq`): a request's attempts usually arrive in one batch,
//! which intake stores in event-id order, so arrival alone would mix them
//! up. The call log orders by `seq` because it pages by it; the two orders
//! differ on purpose.
//!
//! - Rows are found by the request id as a bind parameter, matched exactly,
//!   among the charges filed under the tenant of the control plane's own key
//!   record (`pricing_charges.tenant_id` and `request_id`), never under the
//!   row's `team_id`. Another tenant's rows under the same id are never
//!   read, and they and an id never sent answer the same `404`.
//! - What the key sees follows its member's current role ([`Visibility`]):
//!   an owner's, admin's or billing member's key sees the tenant's rows, a
//!   member's key the rows of the member's keys, and a key bound to no member
//!   its own rows.
//! - Each row is shown as the call log shows it, through the same
//!   [`tenant_view`] and SQL rules, and what it was charged is the charge
//!   plus its re-pricing adjustments. Nothing of the platform's channel is
//!   read: no provider, upstream model, cost price, error code or upstream
//!   status or text.
//!
//! This is a lookup on its own route: it never touches model traffic, and
//! it runs on the gateway lookups' own pool (`access::LOOKUP_POOL_SIZE`), so
//! it never takes a connection usage intake needs. Its statements run under
//! [`LOOKUP_STATEMENT_TIMEOUT_MS`], the time a gateway waits for it, so a slow
//! lookup gives its connection back to the other lookups; one that runs out
//! answers `503`.

use std::collections::BTreeMap;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use super::access::{key_access, KeyAccess, Visibility};
use super::calls::{
    call_model, tenant_view, text, Models, TenantStatus, ADJUSTED, EVENT_FIELDS, VISIBLE,
};
use super::money::{cny_string, CURRENCY};
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::{ApiError, Detail};
use crate::AppState;

/// The most rows one lookup lists; `has_more` says when there are more.
pub const MAX_REQUEST_ROWS: i64 = 50;

/// The longest any statement of a lookup may run: the time a gateway waits
/// for the answer.
pub const LOOKUP_STATEMENT_TIMEOUT_MS: u64 = 2_000;

/// The longest request id a gateway accepts from a caller.
const MAX_REQUEST_ID_BYTES: usize = 256;

/// One row recorded under the request id, as the key's holder may see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestRow {
    pub occurred_at: DateTime<Utc>,
    /// The model name the caller sent, else the granted model's name.
    pub model: String,
    pub status: TenantStatus,
    /// From the fixed vocabulary, for a final failure only.
    pub error: Option<&'static str>,
    pub charge_status: &'static str,
    /// What the row was charged: the charge plus its adjustments; `None`
    /// while pending.
    pub charge_micro_cny: Option<i64>,
    pub estimated: bool,
    /// The billed quantities by dimension, under the names the price listing
    /// uses.
    pub quantities: BTreeMap<String, u64>,
}

/// What one request id cost, as far as the key may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestCost {
    pub request_id: String,
    /// The sum of what the listed rows were charged; pending rows add
    /// nothing.
    pub total_micro_cny: i64,
    /// Whether any listed row is still pending.
    pub pending: bool,
    /// Whether rows past [`MAX_REQUEST_ROWS`] were left out.
    pub has_more: bool,
    pub items: Vec<RequestRow>,
}

#[derive(sqlx::FromRow)]
struct Row {
    occurred_at: DateTime<Utc>,
    requested_model_id: String,
    status: String,
    charge_micro_cny: Option<i64>,
    estimated: bool,
    lines: Value,
    /// The sum of the charge's adjustments, `None` when it has none.
    adjusted: Option<i64>,
    ev: Value,
}

/// A charge line's dimension under the name the price listing gives it.
fn client_name(dimension: &str) -> &str {
    match dimension {
        "cache_read" => "input_cache_read",
        "cache_write" => "input_cache_write",
        "cache_write_1h" => "input_cache_write_1h",
        other => other,
    }
}

/// The billed quantity of each dimension of a charge's `lines`.
fn quantities(lines: &Value) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for line in lines.as_array().into_iter().flatten() {
        let (Some(dimension), Some(quantity)) = (
            line.get("dimension").and_then(Value::as_str),
            line.get("quantity").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let sum: &mut u64 = out.entry(client_name(dimension).to_string()).or_default();
        *sum = sum.saturating_add(quantity);
    }
    out
}

/// A sum past the `bigint` range: never reached, and never clamped.
fn overflow() -> DbError {
    DbError::Db(sqlx::Error::Decode(
        "a request's charge leaves the bigint range".into(),
    ))
}

/// The rows the key of `access` may see under `request_id`; `None` when it
/// may see none, whether another tenant used the id or nobody did. The
/// query starts from the key's tenant's charges under the id
/// (`pricing_charges_tenant_request`), so it never reads another tenant's
/// rows, and stops after [`MAX_REQUEST_ROWS`] + 1 visible ones.
pub async fn request_cost(
    conn: &mut PgConnection,
    access: &KeyAccess,
    request_id: &str,
) -> Result<Option<RequestCost>, DbError> {
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT pc.occurred_at, pc.requested_model_id, pc.status, pc.charge_micro_cny, \
         pc.estimated, pc.lines, ",
    );
    sql.push(ADJUSTED).push(" AS adjusted, ");
    sql.push(EVENT_FIELDS).push(" AS ev");
    sql.push(
        " FROM pricing_charges_cny pc JOIN usage_events ue ON ue.event_id = pc.event_id \
         WHERE pc.tenant_id = ",
    )
    .push_bind(access.tenant_id)
    .push("::text AND pc.request_id = ")
    .push_bind(request_id.to_string());
    match (access.visibility, access.member_id) {
        (Visibility::Full, _) => {}
        (Visibility::Own, Some(member)) => {
            sql.push(" AND pc.user_id = ")
                .push_bind(member)
                .push("::text");
        }
        (Visibility::Own, None) => {
            sql.push(" AND pc.api_key_id = ")
                .push_bind(access.key_id)
                .push("::text");
        }
    }
    sql.push(" AND ").push(VISIBLE);
    sql.push(" ORDER BY pc.occurred_at DESC, pc.seq DESC LIMIT ")
        .push_bind(MAX_REQUEST_ROWS + 1);
    let mut rows: Vec<Row> = sql.build_query_as().fetch_all(&mut *conn).await?;
    let has_more = rows.len() as i64 > MAX_REQUEST_ROWS;
    rows.truncate(MAX_REQUEST_ROWS as usize);

    let model_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|r| Uuid::parse_str(&r.requested_model_id).ok())
        .collect();
    let models = Models::load(&mut *conn, access.tenant_id, &model_ids).await?;

    let mut items = Vec::with_capacity(rows.len());
    let mut total_micro_cny: i64 = 0;
    for r in rows {
        let Some(view) = tenant_view(&r.ev, &r.status, r.adjusted.is_some()) else {
            continue;
        };
        let charge_micro_cny = if view.charge_status == "pending" {
            None
        } else {
            let charged = r
                .charge_micro_cny
                .unwrap_or(0)
                .checked_add(r.adjusted.unwrap_or(0))
                .ok_or_else(overflow)?;
            total_micro_cny = total_micro_cny.checked_add(charged).ok_or_else(overflow)?;
            Some(charged)
        };
        items.push(RequestRow {
            occurred_at: r.occurred_at,
            model: call_model(
                &models,
                &r.requested_model_id,
                &text(&r.ev, "requested_model"),
            )
            .name,
            status: view.status,
            error: view.error,
            charge_status: view.charge_status,
            charge_micro_cny,
            estimated: r.estimated,
            quantities: quantities(&r.lines),
        });
    }
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(RequestCost {
        request_id: request_id.to_string(),
        total_micro_cny,
        pending: items.iter().any(|i| i.charge_micro_cny.is_none()),
        has_more,
        items,
    }))
}

/// Whether `id` can be a request id a gateway recorded: 1 to 256 visible
/// ASCII characters, the gateway's own rule for a caller's id.
fn acceptable(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_REQUEST_ID_BYTES
        && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn refused_id() -> ApiError {
    ApiError::validation(vec![Detail {
        path: "/id".to_string(),
        message: "id must be 1 to 256 visible ASCII characters".to_string(),
    }])
}

#[derive(Deserialize)]
pub struct RequestsQuery {
    id: Option<String>,
}

/// `c` as a gateway relays it: every amount an exact decimal string, under a
/// neutral name, with the currency they are in.
///
/// The amounts are micro-CNY, and `currency` is required by the gateway:
/// without it no amount is shown, so a caller is never handed a figure whose
/// unit it would have to guess.
fn dp_json(c: &RequestCost) -> Value {
    json!({
        "request_id": c.request_id,
        "currency": CURRENCY,
        "total": cny_string(c.total_micro_cny),
        "pending": c.pending,
        "has_more": c.has_more,
        "items": c.items.iter().map(|i| json!({
            "occurred_at": i.occurred_at,
            "model": i.model,
            "status": i.status,
            "error": i.error,
            "charge_status": i.charge_status,
            "charge": i.charge_micro_cny.map(cny_string),
            "estimated": i.estimated,
            "quantities": i.quantities,
        })).collect::<Vec<_>>(),
    })
}

/// Whether `e` is a statement Postgres cancelled (SQLSTATE `57014`), as
/// `statement_timeout` does.
fn cancelled(e: &DbError) -> bool {
    matches!(e, DbError::Db(sqlx::Error::Database(db)) if db.code().as_deref() == Some("57014"))
}

/// `GET /dp/v1/keys/{key_id}/requests?id=`, with the key's own visibility.
/// An unknown or revoked key, and an id under which the key may see no row,
/// are `404`; an id no gateway could have recorded is `422` at `/id`; a
/// lookup that runs past [`LOOKUP_STATEMENT_TIMEOUT_MS`] is `503`.
pub async fn dp_requests(
    _dp: DpIdentity,
    State(state): State<AppState>,
    Path(key_id): Path<String>,
    query: Result<Query<RequestsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let key_id = Uuid::parse_str(&key_id).map_err(|_| ApiError::not_found())?;
    // `id` is the only parameter, so a query that cannot be read is its fault.
    let Ok(Query(q)) = query else {
        return Err(refused_id());
    };
    let request_id = q.id.filter(|id| acceptable(id)).ok_or_else(refused_id)?;
    let found = state
        .lookups
        .tx(move |conn| {
            async move {
                sqlx::query(&format!(
                    "SET LOCAL statement_timeout = {LOOKUP_STATEMENT_TIMEOUT_MS}"
                ))
                .execute(&mut *conn)
                .await?;
                match key_access(&mut *conn, key_id).await? {
                    Some(access) => request_cost(conn, &access, &request_id).await,
                    None => Ok::<_, DbError>(None),
                }
            }
            .boxed()
        })
        .await
        .map_err(|e| {
            if cancelled(&e) {
                tracing::warn!(
                    timeout_ms = LOOKUP_STATEMENT_TIMEOUT_MS,
                    "a request-cost lookup ran past its statement timeout"
                );
                ApiError::unavailable()
            } else {
                ApiError::from(e)
            }
        })?;
    found
        .map(|c| Json(dp_json(&c)))
        .ok_or_else(ApiError::not_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_id_is_one_to_256_visible_ascii_characters() {
        assert!(acceptable("a"));
        assert!(acceptable(&"a".repeat(256)));
        assert!(acceptable("req/lk+5&a=b%c?d#e"));
        assert!(!acceptable(""));
        assert!(!acceptable(&"a".repeat(257)));
        for bad in ["a b", "a\u{1}", "a\u{7f}", "a\té", "é"] {
            assert!(!acceptable(bad), "{bad:?}");
        }
    }

    #[test]
    fn quantities_take_the_listing_names_and_sum_by_dimension() {
        let lines = json!([
            {"dimension": "input", "quantity": 200, "rate": "1"},
            {"dimension": "cache_read", "quantity": 800, "rate": "0.1"},
            {"dimension": "cache_write", "quantity": 5, "rate": "1.25"},
            {"dimension": "cache_write_1h", "quantity": 7, "rate": "2"},
            {"dimension": "output", "quantity": 100, "rate": "2"},
            {"dimension": "output", "quantity": 100, "rate": "4"},
            {"dimension": "web_search", "quantity": 1, "rate": "10000"},
        ]);
        let want: BTreeMap<String, u64> = [
            ("input", 200),
            ("input_cache_read", 800),
            ("input_cache_write", 5),
            ("input_cache_write_1h", 7),
            ("output", 200),
            ("web_search", 1),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(quantities(&lines), want);
        assert!(quantities(&json!([])).is_empty());
    }
}
