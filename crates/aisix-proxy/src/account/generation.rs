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
//! strings, passed through: the gateway does no money arithmetic.
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
//!
//! A found request is cached per key and id for [`REQUEST_TTL`], and a
//! failure for [`FAILURE_TTL`]. A `404` is never cached, so a caller polling
//! for a request it just sent sees it as soon as the control plane has it. A
//! lookup the cache cannot answer asks the control plane itself and shares
//! that request with no other caller, so a caller that hangs up cancels only
//! its own request and never one another caller waits on.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use moka::future::Cache;
use moka::Expiry;
use serde::{Deserialize, Serialize};

use super::{key_path, AccountClient, AccountError, ACCOUNT_TIMEOUT, CACHE_CAPACITY, FAILURE_TTL};
use crate::auth::AuthenticatedKey;
use crate::error::{ErrorEnvelope, ProxyError};
use crate::request_id::RequestId;
use crate::state::ProxyState;

/// How long a found request's answer is served from the cache.
pub const REQUEST_TTL: Duration = Duration::from_secs(3);

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
    charge_usd: Option<String>,
    estimated: Option<bool>,
    /// Billed quantities by dimension name.
    quantities: Option<BTreeMap<String, u64>>,
}

/// The control plane's answer for one key and request id.
///
/// Decoded loosely: unknown fields are ignored and every field but `items`,
/// which the answer exists to give, has a default. The route renders its
/// own shape from these fields, so nothing the control plane adds reaches a
/// caller until it is named here.
#[derive(Debug, Deserialize)]
pub(crate) struct RequestCostAnswer {
    #[serde(default)]
    total_usd: Option<String>,
    #[serde(default)]
    pending: Option<bool>,
    #[serde(default)]
    has_more: Option<bool>,
    items: Vec<RequestItem>,
}

/// A cached lookup: a found request, or a failure. A `404` is never cached.
type CachedCost = Result<Arc<RequestCostAnswer>, AccountError>;

/// The request lookups by key id and request id.
pub(crate) type RequestCache = Cache<(String, String), CachedCost>;

/// A found request lives [`REQUEST_TTL`] from when it was fetched, a failure
/// [`FAILURE_TTL`].
struct RequestExpiry;

impl RequestExpiry {
    fn ttl(cached: &CachedCost) -> Duration {
        match cached {
            Ok(_) => REQUEST_TTL,
            Err(_) => FAILURE_TTL,
        }
    }
}

impl Expiry<(String, String), CachedCost> for RequestExpiry {
    fn expire_after_create(
        &self,
        _key: &(String, String),
        cached: &CachedCost,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(Self::ttl(cached))
    }

    fn expire_after_update(
        &self,
        _key: &(String, String),
        cached: &CachedCost,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(Self::ttl(cached))
    }
}

pub(super) fn request_cache() -> RequestCache {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .expire_after(RequestExpiry)
        .build()
}

impl AccountClient {
    /// What `request_id` cost, as far as key `key_id` may see; `None` when
    /// it may see no row under the id.
    pub(crate) async fn request_cost(
        &self,
        key_id: &str,
        request_id: &str,
    ) -> Result<Option<Arc<RequestCostAnswer>>, AccountError> {
        let key = (key_id.to_string(), request_id.to_string());
        if let Some(cached) = self.requests.get(&key).await {
            return cached.map(Some);
        }
        let fetched = self.fetch_request_cost(key_id, request_id).await;
        match &fetched {
            Ok(Some(answer)) => self.requests.insert(key, Ok(Arc::clone(answer))).await,
            // Not cached: the row may be stored a moment later.
            Ok(None) => {}
            Err(error) => self.requests.insert(key, Err(*error)).await,
        }
        fetched
    }

    /// One ask of the control plane, driven by the caller's own request and
    /// shared with no other caller.
    async fn fetch_request_cost(
        &self,
        key_id: &str,
        request_id: &str,
    ) -> Result<Option<Arc<RequestCostAnswer>>, AccountError> {
        let path = key_path(key_id, "requests");
        self.get_json::<RequestCostAnswer>(&path, &[("id", request_id)], ACCOUNT_TIMEOUT)
            .await
            .map(|answer| answer.map(Arc::new))
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
                total_cost: answer.total_usd.as_deref(),
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
                        cost: item.charge_usd.as_deref(),
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

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorEnvelope::new(message, "invalid_request_error").with_code("not_found")),
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
                Ok(Some(answer)) => Json(Generation::new(&id, &answer)).into_response(),
                Ok(None) => not_found(NOT_RECORDED),
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, ProxyConfig};
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, Uri};
    use futures::StreamExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    /// The owner's request: a failover attempt the upstream billed and the
    /// final row, as the control plane lists them. Every row and the answer
    /// carry the event's own claimed cost and fields this gateway does not
    /// know.
    fn owners_answer() -> Value {
        json!({
            "request_id": "req-7c1",
            "total_usd": "0.010680",
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
                    "charge_usd": "0.010000",
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
                    "charge_usd": "0.000680",
                    "cost_usd": "98.7654",
                    "estimated": true,
                    "quantities": {"input": 100, "input_cache_read": 80},
                },
            ],
        })
    }

    /// How the stand-in answers, by the key id in the path and the `id` in
    /// the query.
    type Answer = Arc<dyn Fn(&str, Option<&str>) -> (StatusCode, String) + Send + Sync>;

    /// One request the stand-in received: its path and its query pairs.
    type Seen = (String, Vec<(String, String)>);

    /// A stand-in for the control plane's gateway listener on a local port.
    /// Records every request's path and query pairs, decoded as the control
    /// plane's own router decodes them.
    struct StandIn {
        base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl StandIn {
        async fn start(answer: Answer) -> StandIn {
            Self::start_with_delay(answer, Duration::ZERO).await
        }

        /// Every answer is sent after `delay`.
        async fn start_with_delay(answer: Answer, delay: Duration) -> StandIn {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let s = Arc::clone(&seen);
            let app = axum::Router::new().fallback(
                move |uri: Uri, Query(pairs): Query<Vec<(String, String)>>| {
                    let (answer, s) = (Arc::clone(&answer), Arc::clone(&s));
                    async move {
                        let path = uri.path().to_string();
                        let key = path
                            .strip_prefix("/dp/v1/keys/")
                            .and_then(|rest| rest.strip_suffix("/requests"))
                            .unwrap_or("")
                            .to_string();
                        let id = pairs
                            .iter()
                            .find(|(name, _)| name == "id")
                            .map(|(_, v)| v.clone());
                        s.lock().unwrap().push((path, pairs));
                        let (status, body) = answer(&key, id.as_deref());
                        tokio::time::sleep(delay).await;
                        (
                            status,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            body,
                        )
                    }
                },
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            StandIn {
                base: format!("http://{addr}"),
                seen,
            }
        }

        fn hits(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn client(&self) -> AccountClient {
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            AccountClient::new(http, &format!("{}/", self.base))
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
        // The row is stored between the first lookup and the second.
        let asked = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&asked);
        let stand_in = StandIn::start(Arc::new(move |_, _| {
            if a.fetch_add(1, Ordering::SeqCst) == 0 {
                not_found_upstream()
            } else {
                ok(owners_answer())
            }
        }))
        .await;
        let client = stand_in.client();
        assert!(client
            .request_cost("k-owner", "req-7c1")
            .await
            .unwrap()
            .is_none());
        let found = client
            .request_cost("k-owner", "req-7c1")
            .await
            .unwrap()
            .expect("found on the next lookup");
        assert_eq!(found.items.len(), 2);
        assert_eq!(stand_in.hits(), 2);
        // A found request is cached.
        assert!(client
            .request_cost("k-owner", "req-7c1")
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
        let stand_in = StandIn::start(Arc::new(|_, _| ok(owners_answer()))).await;

        assert!(stand_in
            .client()
            .request_cost("k-owner", ID)
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
        let stand_in = StandIn::start(Arc::new(|_, _| ok(owners_answer()))).await;
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
        let stand_in = StandIn::start(Arc::new(|_, id| {
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
                client.request_cost("k-owner", id).await.unwrap_err(),
                AccountError::Unavailable,
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn a_failure_is_unavailable_and_cached_for_its_own_ttl() {
        let stand_in = StandIn::start(Arc::new(|_, id| match id {
            Some("garbled") => (StatusCode::OK, "not json".to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "{}".to_string()),
        }))
        .await;
        let client = stand_in.client();
        for id in ["failing", "garbled"] {
            assert_eq!(
                client.request_cost("k-owner", id).await.unwrap_err(),
                AccountError::Unavailable,
                "{id}"
            );
        }
        let before = stand_in.hits();
        assert!(client.request_cost("k-owner", "failing").await.is_err());
        assert_eq!(stand_in.hits(), before, "within FAILURE_TTL");
        tokio::time::sleep(FAILURE_TTL + Duration::from_millis(300)).await;
        assert!(client.request_cost("k-owner", "failing").await.is_err());
        assert_eq!(stand_in.hits(), before + 1, "past FAILURE_TTL");
    }

    #[tokio::test]
    async fn a_request_cost_is_cached_per_key_and_id_for_its_ttl() {
        let stand_in = StandIn::start(Arc::new(|_, _| ok(owners_answer()))).await;
        let client = stand_in.client();
        for _ in 0..10 {
            assert!(client
                .request_cost("key-a", "req-1")
                .await
                .unwrap()
                .is_some());
        }
        assert_eq!(stand_in.hits(), 1, "ten lookups of one id");
        assert!(client
            .request_cost("key-a", "req-2")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 2, "another id is its own entry");
        assert!(client
            .request_cost("key-b", "req-1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 3, "another key is its own entry");
        assert!(client
            .request_cost("key-a", "req-1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 3, "still within the TTL");

        tokio::time::sleep(REQUEST_TTL + Duration::from_millis(300)).await;
        assert!(client
            .request_cost("key-a", "req-1")
            .await
            .unwrap()
            .is_some());
        assert_eq!(stand_in.hits(), 4, "past the TTL");
    }

    /// A caller that hangs up while its lookup waits on the control plane
    /// cancels only its own request: a second caller of the same id, already
    /// waiting, is answered one control-plane round trip after it asked, and
    /// is never made to start over.
    #[tokio::test]
    async fn a_caller_that_hangs_up_leaves_another_callers_lookup_alone() {
        const ROUND_TRIP: Duration = Duration::from_millis(1_000);
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_, _| ok(owners_answer())), ROUND_TRIP).await;
        let client = Arc::new(stand_in.client());

        let first = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request_cost("key-a", "req-1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                let started = Instant::now();
                let answer = client.request_cost("key-a", "req-1").await;
                (answer, started.elapsed())
            })
        };
        // The first caller hangs up just before its answer would arrive.
        tokio::time::sleep(Duration::from_millis(850)).await;
        first.abort();

        let (answer, waited) = second.await.unwrap();
        assert!(answer.unwrap().is_some());
        assert!(
            waited < ROUND_TRIP + Duration::from_millis(500),
            "the second caller waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn the_request_cache_is_bounded() {
        let stand_in = StandIn::start(Arc::new(|_, _| ok(owners_answer()))).await;
        let client = stand_in.client();
        let ids = CACHE_CAPACITY as usize + 100;
        futures::stream::iter(0..ids)
            .map(|i| {
                let client = &client;
                async move { client.request_cost("key-a", &format!("req-{i}")).await }
            })
            .buffer_unordered(64)
            .for_each(|answer| async move { assert!(answer.unwrap().is_some()) })
            .await;
        assert_eq!(stand_in.hits(), ids);
        client.requests.run_pending_tasks().await;
        let held = client.requests.entry_count();
        assert!(held > 0 && held <= CACHE_CAPACITY, "{held} entries held");
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
        StandIn::start(Arc::new(|key, id| match (key, id) {
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
        let stand_in = StandIn::start(Arc::new(|_, _| ok(json!({"items": [{}]})))).await;
        let (status, body) = lookup(router(Some(stand_in.client()), false), OWNER, "req-7c1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // What the control plane did not say is rendered `null`, never
        // made up.
        assert_eq!(
            body,
            json!({"data": {
                "id": "req-7c1",
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
    /// sent all answer the same bytes.
    #[tokio::test]
    async fn a_request_the_key_may_not_see_answers_like_one_never_sent() {
        let stand_in = tenant_stand_in().await;
        let app = router(Some(stand_in.client()), false);

        let (status, never) = lookup_bytes(app.clone(), MEMBER, "id=req-never-sent").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: Value = serde_json::from_slice(&never).unwrap();
        assert_eq!(
            body,
            json!({"error": {
                "message": "No usage is recorded for this request id yet. Usage appears a few seconds after a request completes.",
                "type": "invalid_request_error",
                "code": "not_found",
            }})
        );
        for (bearer, id) in [
            (MEMBER, "req-7c1"),
            (STRANGER, "req-7c1"),
            (STRANGER, "req-never-sent"),
        ] {
            let (status, bytes) = lookup_bytes(app.clone(), bearer, &format!("id={id}")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{bearer} {id}");
            assert_eq!(bytes, never, "{bearer} {id}");
        }
        // Each asked with the caller's own key.
        let keys: Vec<String> = stand_in.seen().into_iter().map(|(path, _)| path).collect();
        assert_eq!(
            keys,
            [
                "/dp/v1/keys/k-member/requests",
                "/dp/v1/keys/k-member/requests",
                "/dp/v1/keys/k-stranger/requests",
                "/dp/v1/keys/k-stranger/requests",
            ]
        );
    }

    #[tokio::test]
    async fn an_unavailable_control_plane_fails_the_lookup_with_503() {
        let stand_in =
            StandIn::start(Arc::new(|_, _| (StatusCode::BAD_GATEWAY, "{}".to_string()))).await;
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
