//! The account lookups: routes on which a caller reads its own account with
//! its API key, answered from the control plane.
//!
//! These are the one place the gateway asks the control plane something on a
//! caller's behalf while the caller waits. Model traffic never does, and never
//! shares anything with them: [`AccountClient`] holds a client and connection
//! pool of its own (built by the server from the gateway's mTLS bundle), every
//! request is bounded by [`ACCOUNT_TIMEOUT`], answers are cached for a few
//! seconds in caches bounded at [`CACHE_CAPACITY`] entries, and a control
//! plane that cannot answer fails only the lookup, with `503`
//! ([`crate::ProxyError::AccountUnavailable`]). A failure is cached too, for
//! [`FAILURE_TTL`], so a failing control plane is asked at most once a second
//! per key, and its failures are logged at WARN at most once per
//! [`WARN_INTERVAL`]. Every lookup goes through [`SharedLookup`], which runs
//! each fetch on a task of its own, so a caller that hangs up never cancels
//! a fetch other callers are waiting for.
//!
//! Without a control plane (a gateway reading a resources file, or a managed
//! gateway with no certificate bundle) there is no client, and every lookup
//! answers `404`.
//!
//! The tenant prices of the model listing ([`prices`]) are the exception to
//! the `503` and the `404`: they decorate a route that answers without them,
//! so a fetch has a shorter deadline of its own and no answer leaves the
//! listing as it is without them.
//!
//! Every answer decodes loosely: the control plane may be many releases newer
//! than this gateway, so no answer type denies unknown fields, and every field
//! but the one the lookup hinges on has a default. A route renders its own
//! client shape from what it decoded, so a field a newer control plane adds
//! reaches no caller until the gateway names it.

pub(crate) mod key;
pub(crate) mod prices;

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use moka::future::Cache;
use moka::Expiry;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::de::DeserializeOwned;
use tracing::Instrument;

/// How long one lookup waits for the control plane, connecting included.
pub const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a key's quota answer is served from the cache.
pub const QUOTA_TTL: Duration = Duration::from_secs(3);
/// How long the control plane's "no such key" is served from the cache.
pub const NOT_FOUND_TTL: Duration = Duration::from_secs(1);
/// How long a lookup the control plane could not answer is answered `503`
/// from the cache.
pub const FAILURE_TTL: Duration = Duration::from_secs(1);
/// The most entries any one lookup cache holds.
pub const CACHE_CAPACITY: u64 = 10_000;
/// At most one WARN per this interval for the lookups' failures; the rest
/// are logged at debug.
pub const WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Whether a lookup failure may be logged at WARN now ([`WARN_INTERVAL`]).
fn warn_due() -> bool {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let last = LAST.load(Ordering::Relaxed);
    now.saturating_sub(last) >= WARN_INTERVAL.as_secs()
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// A lookup failure, at WARN when [`warn_due`] and at debug otherwise.
macro_rules! lookup_failed {
    ($($arg:tt)*) => {
        if warn_due() {
            tracing::warn!($($arg)*)
        } else {
            tracing::debug!($($arg)*)
        }
    };
}

/// Why a lookup has no answer to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountError {
    /// The control plane timed out, could not be reached, failed, or answered
    /// with something that does not decode.
    Unavailable,
}

/// The gateway's client for the account lookups. Cheap to clone: a
/// lookup's fetch runs on a task of its own, with a clone.
#[derive(Clone)]
pub struct AccountClient {
    http: reqwest::Client,
    /// The control plane's origin, as configured, without a trailing slash.
    base: String,
    quotas: key::QuotaCache,
    prices: prices::PricesCache,
}

impl AccountClient {
    /// `http` must be a client of its own, never one model traffic or another
    /// control-plane channel uses; `cp_base_url` is the control plane's
    /// gateway listener.
    pub fn new(http: reqwest::Client, cp_base_url: &str) -> AccountClient {
        AccountClient {
            http,
            base: cp_base_url.trim_end_matches('/').to_string(),
            quotas: key::quota_cache(),
            prices: prices::prices_cache(),
        }
    }

    /// `GET <control plane><path>?<query>`, decoded as `T`.
    ///
    /// `Ok(None)` is the control plane's `404`. Every other failure (a
    /// timeout, a connection that fails, any other status, a `2xx` that does
    /// not decode as `T`) is [`AccountError::Unavailable`]. Query values are
    /// encoded by the client; `path` must already be encoded.
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Option<T>, AccountError> {
        let mut request = self
            .http
            .get(format!("{}{path}", self.base))
            .timeout(timeout);
        if !query.is_empty() {
            request = request.query(query);
        }
        // The URL is left out of every line logged here: a configured
        // control-plane URL may carry a password.
        let response = request.send().await.map_err(|error| {
            lookup_failed!(
                error = %aisix_gateway::transport_error_message(&error.without_url()),
                "account lookup: the control plane could not be reached",
            );
            AccountError::Unavailable
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            lookup_failed!(%status, "account lookup: the control plane answered with a failure");
            return Err(AccountError::Unavailable);
        }
        let body = response.bytes().await.map_err(|error| {
            aisix_gateway::log_body_read_causes(&error.without_url());
            AccountError::Unavailable
        })?;
        serde_json::from_slice(&body).map(Some).map_err(|error| {
            lookup_failed!(%error, "account lookup: the control plane's answer does not decode");
            AccountError::Unavailable
        })
    }
}

/// One lookup's answers by key, shared by every caller asking for the same
/// key. Each lookup ([`key`], [`prices`]) keeps one, built with its own
/// deadline and TTLs.
///
/// [`SharedLookup::get`] answers from the cache when it can. Otherwise the
/// fetch runs on a task of its own, so a caller that goes away (a client
/// hanging up) never cancels it. Callers asking for the same key meanwhile
/// wait for that one fetch (single-flight), and its outcome is cached for
/// every caller after them, whether or not any caller is still waiting. The
/// fetch ends at the deadline, so no caller waits longer. An answer is kept
/// for `answer_ttl` of it, and a failure (an `Err`, the deadline included)
/// for `failure_ttl`. The cache holds at most [`CACHE_CAPACITY`] keys.
#[derive(Clone)]
pub(crate) struct SharedLookup<V> {
    cache: Cache<String, Result<V, AccountError>>,
    deadline: Duration,
}

/// How long a [`SharedLookup`] keeps an outcome: an answer for
/// `answer_ttl` of it, a failure for `failure_ttl`, from when it is cached.
struct LookupExpiry<V> {
    answer_ttl: fn(&V) -> Duration,
    failure_ttl: Duration,
}

impl<V> LookupExpiry<V> {
    fn ttl(&self, outcome: &Result<V, AccountError>) -> Duration {
        match outcome {
            Ok(answer) => (self.answer_ttl)(answer),
            Err(_) => self.failure_ttl,
        }
    }
}

impl<V> Expiry<String, Result<V, AccountError>> for LookupExpiry<V> {
    fn expire_after_create(
        &self,
        _key: &String,
        outcome: &Result<V, AccountError>,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(self.ttl(outcome))
    }

    fn expire_after_update(
        &self,
        _key: &String,
        outcome: &Result<V, AccountError>,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(self.ttl(outcome))
    }
}

impl<V: Clone + Send + Sync + 'static> SharedLookup<V> {
    /// A lookup whose fetches end at `deadline`, and which keeps an answer
    /// for `answer_ttl(&answer)` and a failure for `failure_ttl`.
    pub(crate) fn new(
        deadline: Duration,
        answer_ttl: fn(&V) -> Duration,
        failure_ttl: Duration,
    ) -> SharedLookup<V> {
        SharedLookup {
            cache: Cache::builder()
                .max_capacity(CACHE_CAPACITY)
                .expire_after(LookupExpiry {
                    answer_ttl,
                    failure_ttl,
                })
                .build(),
            deadline,
        }
    }

    /// `key`'s outcome: the cached one, or else the outcome of the fetch
    /// already running for `key`, or else of `fetch()`, started now on a task
    /// of its own. A fetch still running at the deadline is dropped, and its
    /// outcome is `Err(AccountError::Unavailable)`. `fetch` is called only on
    /// a cache miss, and its future is dropped unpolled when another caller's
    /// fetch for `key` is already running.
    pub(crate) async fn get<Fut>(
        &self,
        key: &str,
        fetch: impl FnOnce() -> Fut,
    ) -> Result<V, AccountError>
    where
        Fut: Future<Output = Result<V, AccountError>> + Send + 'static,
    {
        if let Some(outcome) = self.cache.get(key).await {
            return outcome;
        }
        let (cache, key, deadline, fetch) =
            (self.cache.clone(), key.to_string(), self.deadline, fetch());
        let shared = tokio::spawn(
            async move {
                cache
                    .get_with(key, async move {
                        tokio::time::timeout(deadline, fetch)
                            .await
                            .unwrap_or(Err(AccountError::Unavailable))
                    })
                    .await
            }
            .instrument(tracing::Span::current()),
        );
        // A task that panicked has no outcome to give.
        shared.await.unwrap_or(Err(AccountError::Unavailable))
    }

    /// How many keys the cache holds, once its pending evictions ran.
    #[cfg(test)]
    pub(crate) async fn entry_count(&self) -> u64 {
        self.cache.run_pending_tasks().await;
        self.cache.entry_count()
    }
}

/// Characters a key id keeps when written into a URL path.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// The control plane's path for lookup `what` of key `key_id`.
pub(crate) fn key_path(key_id: &str, what: &str) -> String {
    format!(
        "/dp/v1/keys/{}/{what}",
        utf8_percent_encode(key_id, PATH_SEGMENT)
    )
}

/// A stand-in for the control plane's gateway listener, for the lookups'
/// tests.
#[cfg(test)]
pub(crate) mod stand_in {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::http::{StatusCode, Uri};

    use super::AccountClient;

    /// How the stand-in answers, by the key id in the path.
    pub(crate) type Answer = Arc<dyn Fn(&str) -> (StatusCode, String) + Send + Sync>;

    /// Serves every `GET /dp/v1/keys/{key_id}/{lookup}` on a local port,
    /// answering by key id, and counts every request, in total and by path.
    pub(crate) struct StandIn {
        base: String,
        hits: Arc<AtomicUsize>,
        pub(crate) paths: Arc<Mutex<HashMap<String, usize>>>,
    }

    impl StandIn {
        pub(crate) async fn start(answer: Answer) -> StandIn {
            Self::start_with_delay(answer, Duration::ZERO).await
        }

        /// Every answer is sent after `delay`.
        pub(crate) async fn start_with_delay(answer: Answer, delay: Duration) -> StandIn {
            let hits = Arc::new(AtomicUsize::new(0));
            let paths = Arc::new(Mutex::new(HashMap::new()));
            let (h, p) = (Arc::clone(&hits), Arc::clone(&paths));
            let app = axum::Router::new().fallback(move |uri: Uri| {
                let (answer, h, p) = (Arc::clone(&answer), Arc::clone(&h), Arc::clone(&p));
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    *p.lock().unwrap().entry(uri.path().to_string()).or_insert(0) += 1;
                    let key = uri
                        .path()
                        .strip_prefix("/dp/v1/keys/")
                        .and_then(|rest| rest.split('/').next())
                        .unwrap_or("")
                        .to_string();
                    let (status, body) = answer(&key);
                    tokio::time::sleep(delay).await;
                    (
                        status,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            StandIn {
                base: format!("http://{addr}"),
                hits,
                paths,
            }
        }

        pub(crate) fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// A client of this stand-in, configured with a trailing slash.
        pub(crate) fn client(&self) -> AccountClient {
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            AccountClient::new(http, &format!("{}/", self.base))
        }
    }
}
