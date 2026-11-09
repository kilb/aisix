//! `GET /v1/generation?id=<request id>`: what one request cost the caller's
//! organization, read with the caller's own API key.
//!
//! The answer comes from the control plane's
//! `GET /dp/v1/keys/{key_id}/requests?id=` for the key the request
//! authenticated with. A request id is the caller's own input (the
//! `x-aisix-request-id` it sent, or the one the gateway minted and returned),
//! it need not be unique, and one request leaves one usage row per upstream
//! attempt. So the answer lists every row recorded under the id that the key
//! may see, newest first, with what each was charged (`cost`) and their sum
//! (`total_cost`). Rows another organization recorded under the same id, and
//! rows the key may not see, are never listed: such an id answers exactly
//! like one never sent. Amounts are the control plane's exact decimal
//! strings, passed through under neutral names, and the answer names its
//! currency (`data.currency`): the gateway does no money arithmetic.
//!
//! `currency` is required, and an answer without it is
//! [`ProxyError::AccountUnavailable`] (`503`), so no amount is ever shown
//! without the unit it is in. Every other field defaults ("A Control-Plane
//! Response Decodes Loosely").
//!
//! The shape follows the hosted gateways' generation lookup (`data.id`,
//! `data.total_cost`) and lists the rows in `data.items`, because the id
//! here names a request rather than one generation.
//!
//! - No account client (a gateway reading a resources file, or a managed
//!   gateway without a certificate bundle): `404`.
//! - No `id`, or one no gateway would have recorded: `400` at `id`.
//! - Nothing recorded under the id that the key may see: `404`, saying that
//!   usage appears a few seconds after a request completes.
//! - A control plane that cannot answer: `503`
//!   ([`ProxyError::AccountUnavailable`]).
//! - A key that already has [`REQUEST_LOOKUPS_PER_KEY`] lookups waiting on
//!   the control plane: `429` `rate_limit_exceeded`, at once.
//!
//! A found request is cached per key and id for [`REQUEST_TTL`], and a
//! failure for [`FAILURE_TTL`]. A `404` is kept for no time at all, so a
//! caller polling for a request it just sent sees it as soon as the control
//! plane has it. Concurrent lookups of one key and id share one fetch, run on
//! a task of its own ([`SharedLookup`]), so a caller that hangs up never
//! cancels the fetch another caller is waiting for.
//!
//! Every id never sent, and every fresh one, costs a control-plane query, so
//! a key may have at most [`REQUEST_LOOKUPS_PER_KEY`] fetches in flight. A
//! fetch takes its key's slot when it starts and gives it back when it ends,
//! a caller hanging up included, so a key cannot pile up queries by hanging
//! up and asking again. A lookup answered from the cache, or one that joins
//! a fetch already running for the same id, takes no slot.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::{
    key_path, AccountClient, AccountError, SharedLookup, ACCOUNT_TIMEOUT, CACHE_CAPACITY,
    FAILURE_TTL,
};
use crate::auth::AuthenticatedKey;
use crate::error::{ErrorEnvelope, ProxyError};
use crate::request_id::RequestId;
use crate::state::ProxyState;

/// How long a found request's answer is served from the cache.
pub const REQUEST_TTL: Duration = Duration::from_secs(3);

/// The most request-cost fetches one key may have waiting on the control
/// plane at once.
pub const REQUEST_LOOKUPS_PER_KEY: usize = 4;

/// How long a key's slots are kept once it stops asking. Far longer than a
/// fetch can run ([`ACCOUNT_TIMEOUT`]), so a key's slots are never dropped
/// while one of its fetches holds one.
const SLOTS_IDLE: Duration = Duration::from_secs(60);

/// One usage row recorded under the request id.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct RequestItem {
    /// When the row's attempt occurred.
    #[serde(rename = "occurred_at")]
    occurred: Option<String>,
    model: Option<String>,
    status: Option<String>,
    error: Option<String>,
    charge_status: Option<String>,
    /// The organization's own charge for the row, re-pricing included.
    charge: Option<String>,
    estimated: Option<bool>,
    /// Billed quantities by dimension name.
    quantities: Option<BTreeMap<String, u64>>,
}

/// The control plane's answer for one key and request id.
///
/// Decoded loosely: unknown fields are ignored and every field but
/// `currency` and `items` — what every amount is in, and the rows the answer
/// exists to give — has a default. The route renders its own shape from these
/// fields, so nothing the control plane adds reaches a caller until it is
/// named here.
#[derive(Debug, Deserialize)]
pub(crate) struct RequestCostAnswer {
    /// Required: a figure with no unit is not an answer.
    currency: String,
    #[serde(default)]
    total: Option<String>,
    #[serde(default)]
    pending: Option<bool>,
    #[serde(default)]
    has_more: Option<bool>,
    items: Vec<RequestItem>,
}

/// What one lookup found.
#[derive(Debug, Clone)]
pub(crate) enum RequestLookup {
    /// The rows the key may see under the id.
    Found(Arc<RequestCostAnswer>),
    /// Nothing the key may see is recorded under the id (the control
    /// plane's `404`).
    NotRecorded,
    /// The key already had [`REQUEST_LOOKUPS_PER_KEY`] fetches in flight, so
    /// the control plane was not asked.
    Busy,
}

/// The request lookups: their answers by [`request_key`], and each key's
/// slots for fetches in flight.
#[derive(Clone)]
pub(crate) struct RequestCache {
    answers: SharedLookup<RequestLookup>,
    /// One semaphore of [`REQUEST_LOOKUPS_PER_KEY`] permits per key id,
    /// bounded at [`CACHE_CAPACITY`] keys and dropped [`SLOTS_IDLE`] after the
    /// key's last lookup.
    slots: Cache<String, Arc<Semaphore>>,
}

impl RequestCache {
    /// Key `key_id`'s slots.
    async fn slots_of(&self, key_id: &str) -> Arc<Semaphore> {
        self.slots
            .get_with(key_id.to_string(), async {
                Arc::new(Semaphore::new(REQUEST_LOOKUPS_PER_KEY))
            })
            .await
    }
}

/// A found request lives [`REQUEST_TTL`] from when it was fetched. A `404`
/// lives no time at all, so the next lookup asks again, and so does a
/// lookup refused for want of a slot. A failure lives [`FAILURE_TTL`].
fn request_ttl(lookup: &RequestLookup) -> Duration {
    match lookup {
        RequestLookup::Found(_) => REQUEST_TTL,
        RequestLookup::NotRecorded | RequestLookup::Busy => Duration::ZERO,
    }
}

pub(super) fn request_cache() -> RequestCache {
    RequestCache {
        answers: SharedLookup::new(ACCOUNT_TIMEOUT, request_ttl, FAILURE_TTL),
        slots: Cache::builder()
            .max_capacity(CACHE_CAPACITY)
            .time_to_idle(SLOTS_IDLE)
            .build(),
    }
}

/// The cache key of request id `request_id` asked with key `key_id`: the
/// request id, a space, then the key id. An acceptable request id holds no
/// space ([`crate::request_id::is_acceptable`]), so the first space tells the
/// two apart and no two pairs share a key.
fn request_key(key_id: &str, request_id: &str) -> String {
    format!("{request_id} {key_id}")
}

impl AccountClient {
    /// What `request_id` cost, as far as key `key_id` may see.
    /// `request_id` must be acceptable ([`crate::request_id::is_acceptable`]).
    /// Concurrent lookups of one key and id share one fetch, which a caller
    /// hanging up never cancels; a fetch that would be the key's
    /// [`REQUEST_LOOKUPS_PER_KEY`]-plus-first in flight is
    /// [`RequestLookup::Busy`] without asking.
    pub(crate) async fn request_cost(
        &self,
        key_id: &str,
        request_id: &str,
    ) -> Result<RequestLookup, AccountError> {
        debug_assert!(crate::request_id::is_acceptable(request_id));
        let slots = self.requests.slots_of(key_id).await;
        self.requests
            .answers
            .get(&request_key(key_id, request_id), || {
                let (client, path, id) = (
                    self.clone(),
                    key_path(key_id, "requests"),
                    request_id.to_string(),
                );
                async move {
                    // Taken by the fetch itself, on its own task, and held
                    // until the fetch ends or reaches its deadline.
                    let Ok(_slot) = slots.try_acquire_owned() else {
                        return Ok(RequestLookup::Busy);
                    };
                    Ok(
                        match client
                            .get_json::<RequestCostAnswer>(&path, &[("id", &id)], ACCOUNT_TIMEOUT)
                            .await?
                        {
                            Some(answer) => RequestLookup::Found(Arc::new(answer)),
                            None => RequestLookup::NotRecorded,
                        },
                    )
                }
            })
            .await
    }
}

/// What `GET /v1/generation` answers with.
#[derive(Serialize)]
struct Generation<'a> {
    data: GenerationData<'a>,
}

#[derive(Serialize)]
struct GenerationData<'a> {
    id: &'a str,
    currency: &'a str,
    total_cost: Option<&'a str>,
    pending: Option<bool>,
    has_more: Option<bool>,
    items: Vec<GenerationItem<'a>>,
}

#[derive(Serialize)]
struct GenerationItem<'a> {
    created_at: Option<&'a str>,
    model: Option<&'a str>,
    status: Option<&'a str>,
    error: Option<&'a str>,
    billing_status: Option<&'a str>,
    cost: Option<&'a str>,
    estimated: Option<bool>,
    quantities: Option<&'a BTreeMap<String, u64>>,
}

impl<'a> Generation<'a> {
    /// The answer for request id `id`, the one the caller asked about.
    fn new(id: &'a str, answer: &'a RequestCostAnswer) -> Self {
        Generation {
            data: GenerationData {
                id,
                currency: &answer.currency,
                total_cost: answer.total.as_deref(),
                pending: answer.pending,
                has_more: answer.has_more,
                items: answer
                    .items
                    .iter()
                    .map(|item| GenerationItem {
                        created_at: item.occurred.as_deref(),
                        model: item.model.as_deref(),
                        status: item.status.as_deref(),
                        error: item.error.as_deref(),
                        billing_status: item.charge_status.as_deref(),
                        cost: item.charge.as_deref(),
                        estimated: item.estimated,
                        quantities: item.quantities.as_ref(),
                    })
                    .collect(),
            },
        }
    }
}

/// The `404` for an id under which the key may see nothing: one body,
/// whoever recorded the id and whether anyone did.
const NOT_RECORDED: &str = "No usage is recorded for this request id yet. Usage appears a few \
                            seconds after a request completes.";
/// The `404` of a gateway with no control plane to ask.
const NO_CONTROL_PLANE: &str = "Usage lookups are not available on this gateway.";
/// The `429` of a key with every slot taken.
const TOO_MANY_LOOKUPS: &str =
    "Too many usage lookups are in progress for this API key. Retry when one completes.";

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorEnvelope::new(message, "invalid_request_error").with_code("not_found")),
    )
        .into_response()
}

/// The `429` for a key whose slots are all taken: the gateway's own
/// rate-limit class, with no `Retry-After`, since a slot frees when a fetch
/// ends rather than at a time the gateway knows.
fn too_many_lookups() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(
            ErrorEnvelope::new(TOO_MANY_LOOKUPS, "rate_limit_exceeded")
                .with_code("rate_limit_exceeded"),
        ),
    )
        .into_response()
}

/// The `400` for a missing or unusable `id`.
fn invalid_id() -> Response {
    let mut envelope = ErrorEnvelope::new(
        "`id` must be a request id: 1 to 256 visible ASCII characters.",
        "invalid_request_error",
    );
    envelope.error.param = Some("id".to_string());
    (StatusCode::BAD_REQUEST, Json(envelope)).into_response()
}

/// The query of `GET /v1/generation`.
#[derive(Deserialize)]
pub(crate) struct GenerationQuery {
    id: Option<String>,
}

/// The id the caller asked about, when it is one a gateway could have
/// recorded ([`crate::request_id::is_acceptable`]). A query that cannot be
/// read (an `id` given twice, say) has none.
fn asked_id(query: Result<Query<GenerationQuery>, QueryRejection>) -> Option<String> {
    query
        .ok()?
        .0
        .id
        .filter(|id| crate::request_id::is_acceptable(id))
}

/// `GET /v1/generation?id=`.
pub(crate) async fn generation(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    method: Method,
    request_id: Option<Extension<RequestId>>,
    query: Result<Query<GenerationQuery>, QueryRejection>,
) -> Response {
    let started = Instant::now();
    let response = match state.account.as_deref() {
        None => not_found(NO_CONTROL_PLANE),
        Some(account) => match asked_id(query) {
            None => invalid_id(),
            Some(id) => match account.request_cost(&auth.entry.id, &id).await {
                Ok(RequestLookup::Found(answer)) => {
                    Json(Generation::new(&id, &answer)).into_response()
                }
                Ok(RequestLookup::NotRecorded) => not_found(NOT_RECORDED),
                Ok(RequestLookup::Busy) => too_many_lookups(),
                Err(AccountError::Unavailable) => ProxyError::AccountUnavailable.into_response(),
            },
        },
    };
    crate::models::record_discovery(
        &state,
        &auth,
        &method,
        "/v1/generation",
        request_id.as_ref(),
        &response,
        started,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::stand_in::{QueryAnswer, StandIn};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, ProxyConfig};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    /// The owner's request: a failover attempt the upstream billed and the
    /// final row, as the control plane lists them. Every row and the answer
    /// carry the event's own claimed cost and fields this gateway does not
    /// know.
    fn owners_answer() -> Value {
        json!({
            "request_id": "req-7c1",
            "currency": "CNY",
            "total": "0.010680",
            "pending": false,
            "has_more": false,
            "cost_usd": "98.7654",
            "internal_note": "not for callers",
            "items": [
                {
                    "occurred_at": "2026-11-07T10:00:02Z",
                    "model": "chat-bill",
                    "status": "succeeded",
                    "error": null,
                    "charge_status": "charged",
                    "charge": "0.010000",
                    "cost_usd": "98.7654",
                    "estimated": false,
                    "quantities": {"input": 1000, "output": 200},
                    "provider_hint": "upstream-a",
                },
                {
                    "occurred_at": "2026-11-07T10:00:01Z",
                    "model": "chat-bill",
                    "status": "retried",
                    "error": null,
                    "charge_status": "charged",
                    "charge": "0.000680",
                    "cost_usd": "98.7654",
                    "estimated": true,
                    "quantities": {"input": 100, "input_cache_read": 80},
                },
            ],
        })
    }

    /// The shared stand-in, answering by key id and `id`.
    async fn stand_in(answer: QueryAnswer) -> StandIn {
        StandIn::start_answering(answer, Duration::ZERO).await
    }

    /// [`stand_in`], every answer after `delay`.
    async fn stand_in_with_delay(answer: QueryAnswer, delay: Duration) -> StandIn {
        StandIn::start_answering(answer, delay).await
    }

    impl AccountClient {
        /// [`AccountClient::request_cost`], read as found or not: a lookup
        /// refused for want of a slot fails the test.
        async fn request_found(
            &self,
            key_id: &str,
            request_id: &str,
        ) -> Result<Option<Arc<RequestCostAnswer>>, AccountError> {
            Ok(match self.request_cost(key_id, request_id).await? {
                RequestLookup::Found(answer) => Some(answer),
                RequestLookup::NotRecorded => None,
                RequestLookup::Busy => panic!("refused for want of a slot"),
            })
        }
    }

    fn ok(body: Value) -> (StatusCode, String) {
        (StatusCode::OK, body.to_string())
    }

    fn not_found_upstream() -> (StatusCode, String) {
        (
            StatusCode::NOT_FOUND,
            json!({"code": "not_found"}).to_string(),
        )
    }

    #[tokio::test]
    async fn a_request_cost_404_is_not_cached() {
        // The row is stored between the first lookup and the second, and the
        // second follows at once: a 404 kept for any time at all would answer
        // it.
        let asked = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&asked);
        let stand_in = stand_in(Arc::new(move |_, _| {
            if a.fetch_add(1, Ordering::SeqCst) == 0 {
                not_found_upstream()
            } else {
                ok(owners_answer())
            }
        }))
        .await;
        let client = stand_in.client();
        assert!(client
            .request_found("k-owner", "req-7c1")
            .await
            .unwrap()
            .is_none());
        let found = client
            .request_found("k-owner", "req-7c1")
            .await
            .unwrap()
            .expect("found on the next lookup");
        assert_eq!(found.items.len(), 2);
        assert_eq!(stand_in.hits(), 2);
        // A found request is cached.
        assert!(client
            .request_found("k-owner", "req-7c1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 2);
    }

    /// Every character of the id that means something in a URL reaches the
    /// control plane as part of the one `id` value.
    #[tokio::test]
    async fn the_request_id_reaches_the_control_plane_encoded() {
        const ID: &str = "a+b&c=d%e#f/g";
        assert!(crate::request_id::is_acceptable(ID));
        let stand_in = stand_in(Arc::new(|_, _| ok(owners_answer()))).await;

        assert!(stand_in
            .client()
            .request_found("k-owner", ID)
            .await
            .unwrap()
            .is_some());
        // Through the route, with the id as a caller encodes it.
        let app = router(Some(stand_in.client()), false);
        let (status, body) = lookup(app, OWNER, "a%2Bb%26c%3Dd%25e%23f%2Fg").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["id"], ID);

        let want = (
            "/dp/v1/keys/k-owner/requests".to_string(),
            vec![("id".to_string(), ID.to_string())],
        );
        assert_eq!(stand_in.seen(), vec![want.clone(), want]);
    }

    #[tokio::test]
    async fn a_request_id_that_is_not_acceptable_is_refused() {
        let stand_in = stand_in(Arc::new(|_, _| ok(owners_answer()))).await;
        let app = router(Some(stand_in.client()), false);
        let too_long = format!("id={}", "a".repeat(257));
        for query in [
            too_long.as_str(),
            "id=a%01b",
            "id=a%7Fb",
            "id=a%20b",
            // `+` in a query is a space.
            "id=a+b",
            "id=%C3%A9",
            "id=",
            "",
            "ids=req-7c1",
            "id=req-7c1&id=req-7c2",
        ] {
            let (status, body) = lookup_raw(app.clone(), OWNER, query).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
            assert_eq!(body["error"]["type"], "invalid_request_error", "{query}");
            assert_eq!(body["error"]["param"], "id", "{query}");
        }
        assert_eq!(stand_in.hits(), 0, "a refused id is never asked about");

        // The longest acceptable id is asked about.
        let longest = "a".repeat(256);
        let (status, body) = lookup(app, OWNER, &longest).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(stand_in.hits(), 1);
    }

    #[tokio::test]
    async fn an_answer_without_its_items_is_unavailable() {
        let stand_in = stand_in(Arc::new(|_, id| {
            let mut body = owners_answer();
            match id {
                Some("missing") => {
                    body.as_object_mut().unwrap().remove("items");
                }
                _ => body["items"] = Value::Null,
            }
            ok(body)
        }))
        .await;
        let client = stand_in.client();
        for id in ["missing", "null"] {
            assert_eq!(
                client.request_found("k-owner", id).await.unwrap_err(),
                AccountError::Unavailable,
                "{id}"
            );
        }
    }

    /// The currency is not defaulted either: an answer without it is not an
    /// answer, and the route says so with `503` rather than showing a figure
    /// of unknown unit.
    #[tokio::test]
    async fn a_generation_answer_without_its_currency_is_unavailable() {
        let stand_in = stand_in(Arc::new(|_, _| {
            let mut body = owners_answer();
            body.as_object_mut().unwrap().remove("currency");
            ok(body)
        }))
        .await;
        assert_eq!(
            stand_in
                .client()
                .request_found("k-owner", "req-7c1")
                .await
                .unwrap_err(),
            AccountError::Unavailable
        );

        // Through the route, no amount of the answer reaches the caller.
        let (status, body) = lookup(router(Some(stand_in.client()), false), OWNER, "req-7c1").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["code"], "control_plane_unavailable", "{body}");
        for amount in ["0.010680", "0.010000", "0.000680"] {
            assert!(!body.to_string().contains(amount), "{amount} in {body}");
        }
    }

    #[tokio::test]
    async fn a_failure_is_unavailable_and_cached_for_its_own_ttl() {
        let stand_in = stand_in(Arc::new(|_, id| match id {
            Some("garbled") => (StatusCode::OK, "not json".to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "{}".to_string()),
        }))
        .await;
        let client = stand_in.client();
        for id in ["failing", "garbled"] {
            assert_eq!(
                client.request_found("k-owner", id).await.unwrap_err(),
                AccountError::Unavailable,
                "{id}"
            );
        }
        let before = stand_in.hits();
        assert!(client.request_found("k-owner", "failing").await.is_err());
        assert_eq!(stand_in.hits(), before, "within FAILURE_TTL");
        tokio::time::sleep(FAILURE_TTL + Duration::from_millis(300)).await;
        assert!(client.request_found("k-owner", "failing").await.is_err());
        assert_eq!(stand_in.hits(), before + 1, "past FAILURE_TTL");
    }

    #[tokio::test]
    async fn a_request_cost_is_cached_per_key_and_id_for_its_ttl() {
        // Slow enough that all ten lookups below are in flight together.
        let stand_in = stand_in_with_delay(
            Arc::new(|_, _| ok(owners_answer())),
            Duration::from_millis(200),
        )
        .await;
        let client = stand_in.client();
        let lookups = (0..10).map(|_| client.request_found("key-a", "req-1"));
        for answer in futures::future::join_all(lookups).await {
            assert!(answer.unwrap().is_some());
        }
        assert_eq!(stand_in.hits(), 1, "ten concurrent lookups of one id");
        assert!(client
            .request_found("key-a", "req-2")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 2, "another id is its own entry");
        assert!(client
            .request_found("key-b", "req-1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 3, "another key is its own entry");
        assert!(client
            .request_found("key-a", "req-1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 3, "still within the TTL");

        tokio::time::sleep(REQUEST_TTL + Duration::from_millis(300)).await;
        assert!(client
            .request_found("key-a", "req-1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 4, "past the TTL");
    }

    /// A caller that hangs up while its lookup waits on the control plane
    /// cancels nothing another caller waits on: a second caller of the same
    /// id, already waiting, is answered by the one fetch already running, and
    /// is never made to start over.
    #[tokio::test]
    async fn a_caller_that_hangs_up_leaves_another_callers_lookup_alone() {
        const ROUND_TRIP: Duration = Duration::from_millis(1_000);
        let stand_in = stand_in_with_delay(Arc::new(|_, _| ok(owners_answer())), ROUND_TRIP).await;
        let client = Arc::new(stand_in.client());

        let first = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request_found("key-a", "req-1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                let started = Instant::now();
                let answer = client.request_found("key-a", "req-1").await;
                (answer, started.elapsed())
            })
        };
        // The first caller hangs up just before its answer would arrive.
        tokio::time::sleep(Duration::from_millis(850)).await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let (answer, waited) = second.await.unwrap();
        assert!(answer.unwrap().is_some());
        assert!(
            waited < ROUND_TRIP + Duration::from_millis(500),
            "the second caller waited {waited:?}"
        );
        assert_eq!(stand_in.hits(), 1, "one fetch, never restarted");
    }

    #[test]
    fn the_cache_key_names_the_key_and_the_id_apart() {
        assert_eq!(request_key("k-owner", "req-7c1"), "req-7c1 k-owner");
        // Without the space both would be "abc".
        assert_ne!(request_key("bc", "a"), request_key("c", "ab"));
    }

    /// The answers are bounded by the shared lookup (its own test, in
    /// `key.rs`); the slots are a cache of their own, bounded too.
    #[tokio::test]
    async fn the_slots_are_bounded() {
        let cache = request_cache();
        for i in 0..CACHE_CAPACITY + 100 {
            cache.slots_of(&format!("key-{i}")).await;
        }
        cache.slots.run_pending_tasks().await;
        let held = cache.slots.entry_count();
        assert!(held > 0 && held <= CACHE_CAPACITY, "{held} keys held");
    }

    /// A caller that hangs up does not give its fetch's slot back: four
    /// lookups whose callers all left still hold the key's four slots until
    /// their fetches end, so asking again is refused meanwhile.
    #[tokio::test]
    async fn a_caller_hanging_up_does_not_give_its_slot_back() {
        let stand_in =
            stand_in_with_delay(Arc::new(|_, _| ok(owners_answer())), Duration::from_secs(1)).await;
        let client = Arc::new(stand_in.client());
        let callers: Vec<_> = (0..REQUEST_LOOKUPS_PER_KEY)
            .map(|i| {
                let client = Arc::clone(&client);
                tokio::spawn(async move { client.request_cost("key-a", &format!("req-{i}")).await })
            })
            .collect();
        while stand_in.hits() < REQUEST_LOOKUPS_PER_KEY {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Every caller hangs up, and is gone before the key asks again.
        for caller in callers {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
        }
        assert!(matches!(
            client.request_cost("key-a", "req-again").await,
            Ok(RequestLookup::Busy)
        ));
        assert_eq!(stand_in.hits(), REQUEST_LOOKUPS_PER_KEY, "not asked");

        // Once the fetches end their slots are back.
        let slots = client.requests.slots_of("key-a").await;
        while slots.available_permits() < REQUEST_LOOKUPS_PER_KEY {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(client
            .request_found("key-a", "req-again")
            .await
            .unwrap()
            .is_some());
    }

    // ── the route ───────────────────────────────────────────────────────────

    const OWNER: &str = "sk-generation-owner";
    const MEMBER: &str = "sk-generation-member";
    const STRANGER: &str = "sk-generation-stranger";

    fn cfg() -> ProxyConfig {
        serde_json::from_value(json!({"addr": "127.0.0.1:0"})).unwrap()
    }

    /// A router whose snapshot holds three keys, with `account` installed.
    fn router(account: Option<AccountClient>, platform: bool) -> axum::Router {
        let snap = AisixSnapshot::new();
        for (id, plaintext) in [
            ("k-owner", OWNER),
            ("k-member", MEMBER),
            ("k-stranger", STRANGER),
        ] {
            let key: ApiKey = serde_json::from_value(json!({
                "key_hash": ApiKey::hash_bearer(plaintext),
                "allowed_models": ["*"],
            }))
            .unwrap();
            snap.apikeys.insert(ResourceEntry::new(id, key, 1));
        }
        let hub = Arc::new(aisix_gateway::Hub::new());
        let mut state = ProxyState::new(SnapshotHandle::new(snap), hub, &cfg()).without_cache();
        if let Some(account) = account {
            state = state.with_account(Arc::new(account));
        }
        if platform {
            state = state.with_platform(crate::platform::PlatformSettings::from_secret("s"));
        }
        crate::build_router(state)
    }

    /// `GET /v1/generation?<query>`, the query as written.
    async fn lookup_raw(router: axum::Router, bearer: &str, query: &str) -> (StatusCode, Value) {
        let (status, bytes) = lookup_bytes(router, bearer, query).await;
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn lookup_bytes(
        router: axum::Router,
        bearer: &str,
        query: &str,
    ) -> (StatusCode, axum::body::Bytes) {
        let req = Request::builder()
            .uri(format!("/v1/generation?{query}"))
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        (status, to_bytes(resp.into_body(), 1 << 20).await.unwrap())
    }

    /// `GET /v1/generation?id=<id>`, `id` already encoded.
    async fn lookup(router: axum::Router, bearer: &str, id: &str) -> (StatusCode, Value) {
        lookup_raw(router, bearer, &format!("id={id}")).await
    }

    /// The control plane lists `req-7c1` for the owner's key only: the
    /// member's key may not see it and the stranger belongs to another
    /// organization, so both get its `404`, as every key does for an id
    /// nobody sent.
    async fn tenant_stand_in() -> StandIn {
        stand_in(Arc::new(|key, id| match (key, id) {
            ("k-owner", Some("req-7c1")) => ok(owners_answer()),
            _ => not_found_upstream(),
        }))
        .await
    }

    #[tokio::test]
    async fn the_route_answers_its_own_shape_for_the_callers_request() {
        let stand_in = tenant_stand_in().await;
        let app = router(Some(stand_in.client()), false);

        let (status, bytes) = lookup_bytes(app, OWNER, "id=req-7c1").await;
        assert_eq!(status, StatusCode::OK);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            json!({"data": {
                "id": "req-7c1",
                "currency": "CNY",
                "total_cost": "0.010680",
                "pending": false,
                "has_more": false,
                "items": [
                    {
                        "created_at": "2026-11-07T10:00:02Z",
                        "model": "chat-bill",
                        "status": "succeeded",
                        "error": null,
                        "billing_status": "charged",
                        "cost": "0.010000",
                        "estimated": false,
                        "quantities": {"input": 1000, "output": 200},
                    },
                    {
                        "created_at": "2026-11-07T10:00:01Z",
                        "model": "chat-bill",
                        "status": "retried",
                        "error": null,
                        "billing_status": "charged",
                        "cost": "0.000680",
                        "estimated": true,
                        "quantities": {"input": 100, "input_cache_read": 80},
                    },
                ],
            }})
        );
        // The event's own claimed cost never reaches the caller.
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        for absent in [
            "cost_usd",
            "98.7654",
            "charge_usd",
            "internal_note",
            "upstream-a",
        ] {
            assert!(!text.contains(absent), "{absent} in {text}");
        }
        // The lookup named the caller's own key, and only it.
        assert_eq!(
            stand_in.seen(),
            vec![(
                "/dp/v1/keys/k-owner/requests".to_string(),
                vec![("id".to_string(), "req-7c1".to_string())],
            )]
        );
    }

    #[tokio::test]
    async fn an_answer_missing_an_optional_field_still_decodes() {
        let stand_in = stand_in(Arc::new(|_, _| {
            ok(json!({"currency": "CNY", "items": [{}]}))
        }))
        .await;
        let (status, body) = lookup(router(Some(stand_in.client()), false), OWNER, "req-7c1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // What the control plane did not say is rendered `null`, never
        // made up.
        assert_eq!(
            body,
            json!({"data": {
                "id": "req-7c1",
                "currency": "CNY",
                "total_cost": null,
                "pending": null,
                "has_more": null,
                "items": [{
                    "created_at": null,
                    "model": null,
                    "status": null,
                    "error": null,
                    "billing_status": null,
                    "cost": null,
                    "estimated": null,
                    "quantities": null,
                }],
            }})
        );
    }

    /// Another key's request, another organization's, and an id nobody
    /// sent all answer the same bytes, in either mode, the owner's request
    /// included while the owner's answer is cached.
    #[tokio::test]
    async fn a_request_the_key_may_not_see_answers_like_one_never_sent() {
        for platform in [false, true] {
            let stand_in = tenant_stand_in().await;
            let app = router(Some(stand_in.client()), platform);

            let (status, never) = lookup_bytes(app.clone(), MEMBER, "id=req-never-sent").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "platform {platform}");
            let body: Value = serde_json::from_slice(&never).unwrap();
            assert_eq!(
                body,
                json!({"error": {
                    "message": "No usage is recorded for this request id yet. Usage appears a few seconds after a request completes.",
                    "type": "invalid_request_error",
                    "code": "not_found",
                }}),
                "platform {platform}"
            );
            // The owner's answer is cached under the owner's key only.
            let (status, _) = lookup_bytes(app.clone(), OWNER, "id=req-7c1").await;
            assert_eq!(status, StatusCode::OK, "platform {platform}");
            for (bearer, id) in [
                (MEMBER, "req-7c1"),
                (STRANGER, "req-7c1"),
                (STRANGER, "req-never-sent"),
            ] {
                let (status, bytes) = lookup_bytes(app.clone(), bearer, &format!("id={id}")).await;
                assert_eq!(
                    status,
                    StatusCode::NOT_FOUND,
                    "{bearer} {id} platform {platform}"
                );
                assert_eq!(bytes, never, "{bearer} {id} platform {platform}");
            }
            // Each asked with the caller's own key.
            let keys: Vec<String> = stand_in.seen().into_iter().map(|(path, _)| path).collect();
            assert_eq!(
                keys,
                [
                    "/dp/v1/keys/k-member/requests",
                    "/dp/v1/keys/k-owner/requests",
                    "/dp/v1/keys/k-member/requests",
                    "/dp/v1/keys/k-stranger/requests",
                    "/dp/v1/keys/k-stranger/requests",
                ],
                "platform {platform}"
            );
        }
    }

    /// A key may have at most four fetches waiting on the control plane:
    /// a fifth lookup of another id is answered `429` at once, with the
    /// gateway's own rate-limit class and no `Retry-After`, and asks
    /// nothing. Another key is not held back by it, and the refused id is
    /// asked about again once a slot is free.
    #[tokio::test]
    async fn a_key_has_at_most_four_lookups_in_flight() {
        const ROUND_TRIP: Duration = Duration::from_millis(1_000);
        for platform in [false, true] {
            let stand_in =
                stand_in_with_delay(Arc::new(|_, _| ok(owners_answer())), ROUND_TRIP).await;
            let app = router(Some(stand_in.client()), platform);
            let ask = |bearer: &'static str, id: String| {
                let app = app.clone();
                async move {
                    let started = Instant::now();
                    let req = Request::builder()
                        .uri(format!("/v1/generation?id={id}"))
                        .header("authorization", format!("Bearer {bearer}"))
                        .body(Body::empty())
                        .unwrap();
                    let resp = app.oneshot(req).await.unwrap();
                    let (status, headers) = (resp.status(), resp.headers().clone());
                    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
                    let body: Value = serde_json::from_slice(&bytes).unwrap();
                    (id, status, headers, body, started.elapsed())
                }
            };

            let owners = (0..5).map(|i| ask(OWNER, format!("req-{i}")));
            let (owners, member) = tokio::join!(
                futures::future::join_all(owners),
                ask(MEMBER, "req-m".to_string())
            );
            let refused: Vec<_> = owners
                .iter()
                .filter(|(_, status, ..)| *status == StatusCode::TOO_MANY_REQUESTS)
                .collect();
            assert_eq!(refused.len(), 1, "platform {platform}: {owners:?}");
            let (refused_id, _, headers, body, took) = refused[0].clone();
            assert_eq!(
                body,
                json!({"error": {
                    "message": "Too many usage lookups are in progress for this API key. Retry when one completes.",
                    "type": "rate_limit_exceeded",
                    "code": "rate_limit_exceeded",
                }}),
                "platform {platform}"
            );
            assert!(headers.get("retry-after").is_none(), "platform {platform}");
            assert!(
                took < ROUND_TRIP / 2,
                "platform {platform}: waited {took:?}"
            );
            for (id, status, ..) in &owners {
                if *id != refused_id {
                    assert_eq!(*status, StatusCode::OK, "platform {platform}: {id}");
                }
            }
            assert_eq!(member.1, StatusCode::OK, "platform {platform}: {member:?}");
            assert_eq!(stand_in.hits(), 5, "four of the owner's and the member's");

            // The four fetches have ended: the refused id is asked about now.
            let (_, status, ..) = ask(OWNER, refused_id).await;
            assert_eq!(status, StatusCode::OK, "platform {platform}");
            assert_eq!(stand_in.hits(), 6);
        }
    }

    #[tokio::test]
    async fn an_unavailable_control_plane_fails_the_lookup_with_503() {
        let stand_in = stand_in(Arc::new(|_, _| (StatusCode::BAD_GATEWAY, "{}".to_string()))).await;
        for platform in [false, true] {
            let app = router(Some(stand_in.client()), platform);
            let (status, body) = lookup(app, OWNER, "req-7c1").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert_eq!(body["error"]["type"], "api_error", "{body}");
            assert_eq!(body["error"]["code"], "control_plane_unavailable", "{body}");
            assert_eq!(
                body["error"]["message"], "Account information is temporarily unavailable.",
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn the_route_answers_not_found_without_an_account_client() {
        let (status, body) = lookup(router(None, false), OWNER, "req-7c1").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(
            body["error"]["message"],
            "Usage lookups are not available on this gateway."
        );
    }
}
