//! `GET /dp/v1/keys/{key_id}/requests?id=`: a gateway asks what one request
//! id cost a key's holder, so it can tell its caller.
//!
//! A request id is the caller's own input, not unique, and one request
//! leaves one usage row per upstream attempt. So the answer lists every row
//! the key's own tenant recorded under the id that the key may see, newest
//! first, at most [`MAX_REQUEST_ROWS`], with the sum of their costs.
//!
//! - Rows are found by the request id as a bind parameter, matched exactly,
//!   and filed under the tenant of the control plane's own key record
//!   (`pricing_charges.tenant_id`), never under the row's `team_id`. Another
//!   tenant's rows under the same id, and an id never sent, answer the same
//!   `404`.
//! - What the key sees follows its member's current role ([`Visibility`]):
//!   an owner's, admin's or billing member's key sees the tenant's rows, a
//!   member's key the rows of the member's keys, and a key bound to no member
//!   its own rows.
//! - Each row is shown as the call log shows it ([`super::calls`]): the same
//!   visibility rule, outcome vocabulary and charge status, and its cost is
//!   the charge plus its re-pricing adjustments. Nothing of the platform's
//!   channel is read: no provider, upstream model, cost price, error code or
//!   upstream status or text.
//!
//! This is a lookup on its own route: it never touches model traffic.

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
    call_model, tenant_charge_status, tenant_outcome, visible_to_tenant, Models, TenantStatus,
    ADJUSTED, EVENT_FIELDS, VISIBLE,
};
use super::money::usd_string;
use crate::db::DbError;
use crate::dp::DpIdentity;
use crate::error::{ApiError, Detail};
use crate::AppState;

/// The most rows one lookup lists; `has_more` says when there are more.
pub const MAX_REQUEST_ROWS: i64 = 50;

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
    /// The charge plus its adjustments; `None` while pending.
    pub cost_micro_usd: Option<i64>,
    pub estimated: bool,
    /// The billed quantities by dimension, under the names the price listing
    /// uses.
    pub quantities: BTreeMap<String, u64>,
}

/// What one request id cost, as far as the key may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestCost {
    pub request_id: String,
    /// The sum of the listed rows' costs; pending rows add nothing.
    pub total_micro_usd: i64,
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
    charge_micro_usd: Option<i64>,
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

fn text(ev: &Value, key: &str) -> String {
    ev.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The rows the key of `access` may see under `request_id`; `None` when it
/// may see none, whether another tenant used the id or nobody did.
pub async fn request_cost(
    conn: &mut PgConnection,
    access: &KeyAccess,
    request_id: &str,
) -> Result<Option<RequestCost>, DbError> {
    let mut sql = QueryBuilder::<Postgres>::new(
        "SELECT pc.occurred_at, pc.requested_model_id, pc.status, pc.charge_micro_usd, \
         pc.estimated, pc.lines, ",
    );
    sql.push(ADJUSTED).push(" AS adjusted, ");
    sql.push(EVENT_FIELDS).push(" AS ev");
    sql.push(
        " FROM usage_events ue JOIN pricing_charges pc ON pc.event_id = ue.event_id \
         WHERE ue.request_id = ",
    )
    .push_bind(request_id.to_string());
    sql.push(" AND pc.tenant_id = ")
        .push_bind(access.tenant_id)
        .push("::text");
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

    let items: Vec<RequestRow> = rows
        .into_iter()
        .filter_map(|r| {
            let is_final = r.ev.get("is_final_attempt").and_then(Value::as_bool) == Some(true);
            let repriced = r.adjusted.is_some();
            if !visible_to_tenant(is_final, &r.status, repriced) {
                return None;
            }
            let status_code =
                r.ev.get("status_code")
                    .and_then(Value::as_u64)
                    .and_then(|c| u16::try_from(c).ok())
                    .unwrap_or(0);
            let (status, _, error) = tenant_outcome(
                status_code,
                &text(&r.ev, "error_class"),
                &text(&r.ev, "error_message"),
                is_final,
            );
            let charge_status = tenant_charge_status(&r.status, repriced);
            let cost_micro_usd = (charge_status != "pending").then(|| {
                r.charge_micro_usd
                    .unwrap_or(0)
                    .saturating_add(r.adjusted.unwrap_or(0))
            });
            Some(RequestRow {
                occurred_at: r.occurred_at,
                model: call_model(
                    &models,
                    &r.requested_model_id,
                    &text(&r.ev, "requested_model"),
                )
                .name,
                status,
                error,
                charge_status,
                cost_micro_usd,
                estimated: r.estimated,
                quantities: quantities(&r.lines),
            })
        })
        .collect();
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(RequestCost {
        request_id: request_id.to_string(),
        total_micro_usd: items
            .iter()
            .filter_map(|i| i.cost_micro_usd)
            .fold(0, i64::saturating_add),
        pending: items.iter().any(|i| i.cost_micro_usd.is_none()),
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

/// `c` as a gateway relays it: every amount an exact decimal string in USD.
fn dp_json(c: &RequestCost) -> Value {
    json!({
        "request_id": c.request_id,
        "total_usd": usd_string(c.total_micro_usd),
        "pending": c.pending,
        "has_more": c.has_more,
        "items": c.items.iter().map(|i| json!({
            "occurred_at": i.occurred_at,
            "model": i.model,
            "status": i.status,
            "error": i.error,
            "charge_status": i.charge_status,
            "cost_usd": i.cost_micro_usd.map(usd_string),
            "estimated": i.estimated,
            "quantities": i.quantities,
        })).collect::<Vec<_>>(),
    })
}

/// `GET /dp/v1/keys/{key_id}/requests?id=`, with the key's own visibility.
/// An unknown or revoked key, and an id under which the key may see no row,
/// are `404`; an id no gateway could have recorded is `422` at `/id`.
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
        .db
        .tx(move |conn| {
            async move {
                match key_access(&mut *conn, key_id).await? {
                    Some(access) => request_cost(conn, &access, &request_id).await,
                    None => Ok::<_, DbError>(None),
                }
            }
            .boxed()
        })
        .await?;
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
