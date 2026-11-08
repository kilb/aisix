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
//! it, so it never fails that route or holds it back. A fetch, and a
//! listing's wait for it, end at [`PRICES_TIMEOUT`], and every way of not
//! getting an answer (a timeout, a connection that fails, a failure status, a
//! `404`, an answer that does not decode) is `None`, with which the models
//! are listed as they are without platform mode's prices.
//!
//! The cache holds one entry per tenant, keyed by
//! [`crate::platform::tenant_owner`]: the key document's team, or the key
//! itself for a key in no team, so one tenant's entry, a failure included,
//! never answers another. An answer lives [`PRICES_TTL`], or until its
//! `valid_until` (the next scheduled price change) when that comes sooner,
//! but never less than [`PRICES_MIN_TTL`]. A `None` lives [`PRICES_FAILURE_TTL`], so a control plane that cannot answer
//! costs a tenant's listings at most one [`PRICES_TIMEOUT`] per
//! [`PRICES_FAILURE_TTL`]. Concurrent listings of one tenant share one fetch,
//! which a listing hanging up never cancels ([`SharedLookup`]), and the cache
//! holds at most [`CACHE_CAPACITY`](super::CACHE_CAPACITY) tenants.
//!
//! The answer decodes loosely: unknown fields are ignored, and every field
//! but `models` (and, within a model, its `model_id`) has a default.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{key_path, AccountClient, SharedLookup};

/// How long a listing waits for the control plane's prices, connecting
/// included.
pub const PRICES_TIMEOUT: Duration = Duration::from_millis(500);
/// How long a tenant's prices are served from the cache, at most.
pub const PRICES_TTL: Duration = Duration::from_secs(10);
/// How long a tenant's prices are served from the cache, at least: a
/// gateway clock past an answer's `valid_until` (ahead of the control
/// plane's) must not turn every listing into a fetch.
pub const PRICES_MIN_TTL: Duration = Duration::from_secs(1);
/// How long a tenant whose prices could not be fetched is listed without
/// them before the control plane is asked again.
pub const PRICES_FAILURE_TTL: Duration = Duration::from_secs(2);

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

/// A tenant's prices; `None` is the control plane's `404`.
type Prices = Option<Arc<TenantPricesAnswer>>;

/// The prices lookups by tenant.
pub(crate) type PricesCache = SharedLookup<Prices>;

/// An answer lives [`PRICES_TTL`], or until its `valid_until` when that is
/// sooner, and at least [`PRICES_MIN_TTL`]; a `404`, like a failure, lives
/// [`PRICES_FAILURE_TTL`].
fn prices_ttl(prices: &Prices) -> Duration {
    let Some(answer) = prices else {
        return PRICES_FAILURE_TTL;
    };
    match answer.valid_until {
        None => PRICES_TTL,
        // A `valid_until` already past by this gateway's clock (one ahead of
        // the control plane's) still keeps the answer for PRICES_MIN_TTL.
        Some(until) => (until - Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO)
            .clamp(PRICES_MIN_TTL, PRICES_TTL),
    }
}

pub(super) fn prices_cache() -> PricesCache {
    SharedLookup::new(PRICES_TIMEOUT, prices_ttl, PRICES_FAILURE_TTL)
}

impl AccountClient {
    /// The prices of tenant `tenant_id` (as [`crate::platform::tenant_owner`]
    /// names it), fetched with the caller's key `key_id` when the cache has
    /// no entry for the tenant. `None` when the control plane gave no answer
    /// in time; never waits past [`PRICES_TIMEOUT`].
    pub(crate) async fn tenant_prices(
        &self,
        tenant_id: &str,
        key_id: &str,
    ) -> Option<Arc<TenantPricesAnswer>> {
        self.prices
            .get(tenant_id, || {
                let (client, path) = (self.clone(), key_path(key_id, "prices"));
                async move {
                    client
                        .get_json::<PricesWire>(&path, &[], PRICES_TIMEOUT)
                        .await
                        .map(|wire| wire.map(|wire| Arc::new(TenantPricesAnswer::from(wire))))
                }
            })
            .await
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::stand_in::StandIn;
    use crate::account::CACHE_CAPACITY;
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
        let stand_in = StandIn::start(Arc::new(|key| match key {
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
        let stand_in = StandIn::start(Arc::new(|key| match key {
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
        let stand_in = StandIn::start(Arc::new(|key| match key {
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
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_| ok(priced("m-1"))), Duration::from_millis(200))
                .await;
        let client = stand_in.client();
        let lookups =
            (0..10).map(|i| client.tenant_prices("team:a", if i % 2 == 0 { "k-1" } else { "k-2" }));
        for answer in futures::future::join_all(lookups).await {
            assert!(answer.is_some());
        }
        assert_eq!(stand_in.hits(), 1);
    }

    /// The listing that started a fetch going away (its client hung up)
    /// cancels nothing: a listing waiting meanwhile gets that fetch's
    /// answer, and the answer is cached for the tenant.
    #[tokio::test]
    async fn a_listing_that_hangs_up_never_cancels_the_shared_fetch() {
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_| ok(priced("m-1"))), Duration::from_millis(300))
                .await;
        let client = stand_in.client();
        let first = tokio::time::timeout(
            Duration::from_millis(50),
            client.tenant_prices("team:a", "k-1"),
        );
        let waiting = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            client.tenant_prices("team:a", "k-2").await
        };
        let (first, waiting) = tokio::join!(first, waiting);
        assert!(first.is_err(), "the first listing hung up");
        assert!(
            waiting.is_some(),
            "the waiting listing got the fetch's answer"
        );
        assert_eq!(stand_in.hits(), 1, "one fetch, never restarted");
        assert!(client.tenant_prices("team:a", "k-3").await.is_some());
        assert_eq!(stand_in.hits(), 1, "the answer was cached");
    }

    /// A fetch nobody waits for any more still runs to its deadline, and
    /// its failure is cached: the tenant's next listing within
    /// `PRICES_FAILURE_TTL` asks nothing.
    #[tokio::test]
    async fn a_fetch_its_listing_abandoned_still_caches_its_failure() {
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_| ok(priced("m-1"))), Duration::from_secs(3))
                .await;
        let client = stand_in.client();
        let abandoned = tokio::time::timeout(
            Duration::from_millis(50),
            client.tenant_prices("team:a", "k-1"),
        )
        .await;
        assert!(abandoned.is_err(), "the listing hung up");
        tokio::time::sleep(PRICES_TIMEOUT + Duration::from_millis(200)).await;
        let started = std::time::Instant::now();
        assert!(client.tenant_prices("team:a", "k-2").await.is_none());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "from the cache"
        );
        assert_eq!(
            stand_in.hits(),
            1,
            "the abandoned fetch's failure was cached"
        );
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let stand_in = StandIn::start(Arc::new(|_| ok(priced("m-1")))).await;
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
        let held = client.prices.entry_count().await;
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
        assert_eq!(prices_ttl(&None), PRICES_FAILURE_TTL);
        assert_eq!(prices_ttl(&answer(None)), PRICES_TTL);
        let far = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(prices_ttl(&answer(Some(far))), PRICES_TTL);
        let soon = prices_ttl(&answer(Some(Utc::now() + chrono::Duration::seconds(3))));
        assert!(
            soon > Duration::from_millis(2_500) && soon <= Duration::from_secs(3),
            "{soon:?}"
        );
        // A change due in less than PRICES_MIN_TTL, or one this gateway's
        // clock already shows as past, keeps the answer PRICES_MIN_TTL.
        let imminent = Utc::now() + chrono::Duration::milliseconds(300);
        assert_eq!(prices_ttl(&answer(Some(imminent))), PRICES_MIN_TTL);
        let past = Utc::now() - chrono::Duration::seconds(3);
        assert_eq!(prices_ttl(&answer(Some(past))), PRICES_MIN_TTL);
    }

    /// An answer whose `valid_until` this gateway's clock shows as past (a
    /// clock ahead of the control plane's) is still served from the cache
    /// for `PRICES_MIN_TTL`, so listings back to back share one fetch.
    #[tokio::test]
    async fn an_answer_already_past_its_valid_until_is_kept_a_moment() {
        let stand_in = StandIn::start(Arc::new(|_| {
            let mut body = priced("m-1");
            body["valid_until"] = json!((Utc::now() - chrono::Duration::seconds(5)).to_rfc3339());
            ok(body)
        }))
        .await;
        let client = stand_in.client();
        for key in ["k-1", "k-2", "k-3"] {
            assert!(client.tenant_prices("team:a", key).await.is_some());
        }
        assert_eq!(stand_in.hits(), 1, "within PRICES_MIN_TTL");
        tokio::time::sleep(PRICES_MIN_TTL + Duration::from_millis(300)).await;
        assert!(client.tenant_prices("team:a", "k-4").await.is_some());
        assert_eq!(stand_in.hits(), 2, "past PRICES_MIN_TTL");
    }
}
