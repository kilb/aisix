//! Tenant prices for the model listing: in platform mode, `GET /v1/models`
//! and `GET /v1/models/{id}` show each model's price for the caller's tenant.
//!
//! The prices come from the control plane's `GET /dp/v1/keys/{key_id}/prices`,
//! asked with the id of the key the request authenticated with. The answer
//! lists what that key's tenant pays per token for each granted model with a
//! price in force, in USD per token as exact decimal strings, which reach the
//! caller exactly as the control plane wrote them: the gateway does no money
//! arithmetic.
//!
//! Unlike the other lookups, this one decorates a route that answers without
//! it, so it never fails that route or holds it back. A fetch waits at most
//! [`PRICES_TIMEOUT`], and every way of not getting an answer (a timeout, a
//! connection that fails, a failure status, a `404`, an answer that does not
//! decode) is `None`, with which the models are listed as they are without
//! platform mode's prices.
//!
//! The cache holds one entry per tenant, keyed by
//! [`crate::platform::tenant_owner`]: the key document's team, or the key
//! itself for a key in no team, so one tenant's entry, a failure included,
//! never answers another. An answer lives [`PRICES_TTL`], or until its
//! `valid_until` (the next scheduled price change) when that comes sooner. A
//! `None` lives [`PRICES_FAILURE_TTL`], so a control plane that cannot answer
//! costs a tenant's listings at most one [`PRICES_TIMEOUT`] per
//! [`PRICES_FAILURE_TTL`]. Concurrent listings of one tenant share one fetch,
//! and the cache holds at most [`CACHE_CAPACITY`] tenants.
//!
//! The answer decodes loosely: unknown fields are ignored, and every field
//! but `models` (and, within a model, its `model_id`) has a default.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moka::future::Cache;
use moka::Expiry;
use serde::{Deserialize, Serialize};

use super::{key_path, AccountClient, AccountError, CACHE_CAPACITY};

/// How long a listing waits for the control plane's prices, connecting
/// included.
pub const PRICES_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a tenant's prices are served from the cache, at most.
pub const PRICES_TTL: Duration = Duration::from_secs(10);
/// How long a tenant whose prices could not be fetched is listed without
/// them before the control plane is asked again.
pub const PRICES_FAILURE_TTL: Duration = Duration::from_secs(2);

/// How much longer than [`PRICES_TIMEOUT`] a listing waits on a fetch it
/// shares. The fetch itself gives up at [`PRICES_TIMEOUT`] and its `None` is
/// cached; this bound only covers a listing whose shared fetch was dropped
/// with the listing that started it, after which another listing starts a
/// fresh one. It is longer than the fetch's own deadline so that the
/// listing that starts a fetch never drops it before the fetch can give up
/// and be cached.
const SHARED_FETCH_GRACE: Duration = Duration::from_millis(100);

/// One model's price for the caller's tenant, in USD per token, as the
/// control plane wrote it. What the listing shows as a model's `pricing`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ModelPricing {
    input: Option<String>,
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_cache_read: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_cache_write: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_cache_write_1h: Option<String>,
}

/// One model of the control plane's answer.
#[derive(Deserialize)]
struct ModelPriceWire {
    model_id: String,
    #[serde(flatten)]
    pricing: ModelPricing,
}

/// The control plane's answer, decoded loosely.
#[derive(Deserialize)]
struct PricesWire {
    #[serde(default)]
    valid_until: Option<DateTime<Utc>>,
    models: Vec<ModelPriceWire>,
}

/// A tenant's prices, by model resource id.
#[derive(Debug)]
pub(crate) struct TenantPricesAnswer {
    /// When a price shown here next changes, if one is scheduled.
    valid_until: Option<DateTime<Utc>>,
    by_model: HashMap<String, ModelPricing>,
}

impl TenantPricesAnswer {
    /// The price of the model whose resource id is `model_id`, if it has one.
    pub(crate) fn price_of(&self, model_id: &str) -> Option<&ModelPricing> {
        self.by_model.get(model_id)
    }
}

impl From<PricesWire> for TenantPricesAnswer {
    fn from(wire: PricesWire) -> Self {
        TenantPricesAnswer {
            valid_until: wire.valid_until,
            by_model: wire
                .models
                .into_iter()
                .map(|m| (m.model_id, m.pricing))
                .collect(),
        }
    }
}

/// A fetch's outcome: `None` for every way of not getting an answer.
type PricesLookup = Option<Arc<TenantPricesAnswer>>;

/// The prices lookups by tenant.
pub(crate) type PricesCache = Cache<String, PricesLookup>;

/// An answer lives [`PRICES_TTL`], or until its `valid_until` when that is
/// sooner; a `None` lives [`PRICES_FAILURE_TTL`].
struct PricesExpiry;

impl PricesExpiry {
    fn ttl(lookup: &PricesLookup) -> Duration {
        let Some(answer) = lookup else {
            return PRICES_FAILURE_TTL;
        };
        match answer.valid_until {
            None => PRICES_TTL,
            // A `valid_until` already past (or a clock behind the control
            // plane's) keeps nothing: the next listing asks again.
            Some(until) => (until - Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO)
                .min(PRICES_TTL),
        }
    }
}

impl Expiry<String, PricesLookup> for PricesExpiry {
    fn expire_after_create(
        &self,
        _tenant: &String,
        lookup: &PricesLookup,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(Self::ttl(lookup))
    }

    fn expire_after_update(
        &self,
        _tenant: &String,
        lookup: &PricesLookup,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(Self::ttl(lookup))
    }
}

pub(super) fn prices_cache() -> PricesCache {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .expire_after(PricesExpiry)
        .build()
}

impl AccountClient {
    /// The prices of tenant `tenant_id` (as [`crate::platform::tenant_owner`]
    /// names it), fetched with the caller's key `key_id` when the cache has
    /// no entry for the tenant. `None` when the control plane gave no answer
    /// in time; never waits much past [`PRICES_TIMEOUT`].
    pub(crate) async fn tenant_prices(
        &self,
        tenant_id: &str,
        key_id: &str,
    ) -> Option<Arc<TenantPricesAnswer>> {
        let lookup = self.prices.get_with(tenant_id.to_string(), async {
            match self
                .get_json::<PricesWire>(&key_path(key_id, "prices"), &[], PRICES_TIMEOUT)
                .await
            {
                Ok(Some(wire)) => Some(Arc::new(TenantPricesAnswer::from(wire))),
                Ok(None) | Err(AccountError::Unavailable) => None,
            }
        });
        tokio::time::timeout(PRICES_TIMEOUT + SHARED_FETCH_GRACE, lookup)
            .await
            .ok()
            .flatten()
    }
}

/// A stand-in for the control plane's prices route, for the tests here and
/// in [`crate::models`].
#[cfg(test)]
pub(crate) mod stand_in {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::http::{StatusCode, Uri};

    use crate::account::AccountClient;

    /// How the stand-in answers, by the key id in the path.
    pub(crate) type Answer = Arc<dyn Fn(&str) -> (StatusCode, String) + Send + Sync>;

    /// Serves `GET /dp/v1/keys/{key_id}/prices` on a local port and counts
    /// every request, in total and by path.
    pub(crate) struct PricesStandIn {
        base: String,
        hits: Arc<AtomicUsize>,
        pub(crate) paths: Arc<Mutex<HashMap<String, usize>>>,
    }

    impl PricesStandIn {
        pub(crate) async fn start(answer: Answer) -> PricesStandIn {
            Self::start_with_delay(answer, Duration::ZERO).await
        }

        /// Every answer is sent after `delay`.
        pub(crate) async fn start_with_delay(answer: Answer, delay: Duration) -> PricesStandIn {
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
                        .and_then(|rest| rest.strip_suffix("/prices"))
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
            PricesStandIn {
                base: format!("http://{addr}"),
                hits,
                paths,
            }
        }

        pub(crate) fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        pub(crate) fn client(&self) -> AccountClient {
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            AccountClient::new(http, &self.base)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stand_in::PricesStandIn;
    use super::*;
    use axum::http::StatusCode;
    use futures::StreamExt;
    use serde_json::{json, Value};

    fn ok(body: Value) -> (StatusCode, String) {
        (StatusCode::OK, body.to_string())
    }

    fn priced(model_id: &str) -> Value {
        json!({
            "tenant_id": "t-1",
            "pricing_version": 3,
            "valid_until": null,
            "models": [{"model_id": model_id, "input": "0.000006", "output": "0.00002"}],
        })
    }

    fn pricing(input: &str, output: &str) -> ModelPricing {
        ModelPricing {
            input: Some(input.to_string()),
            output: Some(output.to_string()),
            ..ModelPricing::default()
        }
    }

    #[tokio::test]
    async fn an_answer_with_unknown_or_missing_optional_fields_still_decodes() {
        let stand_in = PricesStandIn::start(Arc::new(|key| match key {
            "k-new" => {
                let mut body = priced("m-1");
                body["a_field_from_a_newer_release"] = json!({"nested": [1]});
                body["models"][0]["another_new_field"] = json!(7);
                ok(body)
            }
            // Only what the lookup hinges on.
            _ => ok(json!({"models": [{"model_id": "m-1", "input_cache_read": "0.0000006"}]})),
        }))
        .await;
        let client = stand_in.client();
        let answer = client.tenant_prices("team:a", "k-new").await.unwrap();
        assert_eq!(
            answer.price_of("m-1"),
            Some(&pricing("0.000006", "0.00002"))
        );
        let answer = client.tenant_prices("team:b", "k-old").await.unwrap();
        assert_eq!(
            answer.price_of("m-1"),
            Some(&ModelPricing {
                input_cache_read: Some("0.0000006".to_string()),
                ..ModelPricing::default()
            })
        );
        assert!(answer.price_of("m-2").is_none());
    }

    /// Every way of not getting an answer is `None`, and is remembered for
    /// the tenant for `PRICES_FAILURE_TTL`.
    #[tokio::test]
    async fn no_answer_is_none_and_is_cached_briefly() {
        let stand_in = PricesStandIn::start(Arc::new(|key| match key {
            "k-missing" => (StatusCode::NOT_FOUND, "{}".to_string()),
            "k-failing" => (StatusCode::INTERNAL_SERVER_ERROR, "{}".to_string()),
            "k-garbled" => (StatusCode::OK, "not json".to_string()),
            "k-no-models" => ok(json!({"tenant_id": "t-1", "valid_until": null})),
            "k-numbers" => ok(json!({"models": [{"model_id": "m-1", "input": 0.000006}]})),
            _ => ok(priced("m-1")),
        }))
        .await;
        let client = stand_in.client();
        for key in [
            "k-missing",
            "k-failing",
            "k-garbled",
            "k-no-models",
            "k-numbers",
        ] {
            assert!(
                client
                    .tenant_prices(&format!("team:{key}"), key)
                    .await
                    .is_none(),
                "{key}"
            );
        }
        let before = stand_in.hits();
        assert_eq!(before, 5);
        assert!(client
            .tenant_prices("team:k-failing", "k-failing")
            .await
            .is_none());
        assert_eq!(stand_in.hits(), before, "within PRICES_FAILURE_TTL");
        tokio::time::sleep(PRICES_FAILURE_TTL + Duration::from_millis(300)).await;
        assert!(client
            .tenant_prices("team:k-failing", "k-failing")
            .await
            .is_none());
        assert_eq!(stand_in.hits(), before + 1, "past PRICES_FAILURE_TTL");
    }

    #[tokio::test]
    async fn one_tenants_failure_never_answers_another() {
        let stand_in = PricesStandIn::start(Arc::new(|key| match key {
            "k-a" => (StatusCode::SERVICE_UNAVAILABLE, "{}".to_string()),
            _ => ok(priced("m-1")),
        }))
        .await;
        let client = stand_in.client();
        assert!(client.tenant_prices("team:a", "k-a").await.is_none());
        let theirs = client.tenant_prices("team:b", "k-b").await.unwrap();
        assert!(theirs.price_of("m-1").is_some());
        // A key in no team is a tenant of its own.
        assert!(client.tenant_prices("key:k-c", "k-c").await.is_some());
        assert_eq!(stand_in.hits(), 3);
        // Each tenant was asked about with its own key.
        let paths = stand_in.paths.lock().unwrap().clone();
        assert_eq!(
            paths,
            HashMap::from([
                ("/dp/v1/keys/k-a/prices".to_string(), 1),
                ("/dp/v1/keys/k-b/prices".to_string(), 1),
                ("/dp/v1/keys/k-c/prices".to_string(), 1),
            ])
        );
    }

    #[tokio::test]
    async fn concurrent_listings_of_one_tenant_share_one_fetch() {
        // Slow enough that all ten lookups below are in flight together.
        let stand_in = PricesStandIn::start_with_delay(
            Arc::new(|_| ok(priced("m-1"))),
            Duration::from_millis(200),
        )
        .await;
        let client = stand_in.client();
        let lookups =
            (0..10).map(|i| client.tenant_prices("team:a", if i % 2 == 0 { "k-1" } else { "k-2" }));
        for answer in futures::future::join_all(lookups).await {
            assert!(answer.is_some());
        }
        assert_eq!(stand_in.hits(), 1);
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let stand_in = PricesStandIn::start(Arc::new(|_| ok(priced("m-1")))).await;
        let client = stand_in.client();
        let tenants = CACHE_CAPACITY as usize + 100;
        futures::stream::iter(0..tenants)
            .map(|i| {
                let client = &client;
                async move { client.tenant_prices(&format!("team:{i}"), "k").await }
            })
            .buffer_unordered(64)
            .for_each(|answer| async move { assert!(answer.is_some()) })
            .await;
        assert_eq!(stand_in.hits(), tenants);
        client.prices.run_pending_tasks().await;
        let held = client.prices.entry_count();
        assert!(held > 0 && held <= CACHE_CAPACITY, "{held} entries held");
    }

    #[test]
    fn an_answer_lives_until_its_valid_until_at_most_its_ttl() {
        let answer = |valid_until: Option<DateTime<Utc>>| {
            Some(Arc::new(TenantPricesAnswer {
                valid_until,
                by_model: HashMap::new(),
            }))
        };
        assert_eq!(PricesExpiry::ttl(&None), PRICES_FAILURE_TTL);
        assert_eq!(PricesExpiry::ttl(&answer(None)), PRICES_TTL);
        let far = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(PricesExpiry::ttl(&answer(Some(far))), PRICES_TTL);
        let soon = PricesExpiry::ttl(&answer(Some(Utc::now() + chrono::Duration::seconds(3))));
        assert!(
            soon > Duration::from_millis(2_500) && soon <= Duration::from_secs(3),
            "{soon:?}"
        );
        let past = Utc::now() - chrono::Duration::seconds(3);
        assert_eq!(PricesExpiry::ttl(&answer(Some(past))), Duration::ZERO);
    }
}
