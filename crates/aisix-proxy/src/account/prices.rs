//! Tenant prices for the model listing: in platform mode, `GET /v1/models`
//! and `GET /v1/models/{id}` show each model's price for the caller's tenant.
//!
//! The prices come from the control plane's `GET /dp/v1/keys/{key_id}/prices`,
//! asked with the id of the key the request authenticated with. The answer
//! lists what that key's tenant pays per token for each granted model with a
//! price in force, as exact decimal strings in the currency the control plane
//! names, which reach the caller exactly as the control plane wrote them: the
//! gateway does no money arithmetic. The currency is required, and an answer
//! without it lists no price at all, rather than a figure whose unit no one
//! named.
//!
//! Unlike the other lookups, this one decorates a route that answers without
//! it, so it never fails that route or holds it back. A fetch, and a
//! listing's wait for it, end at [`PRICES_TIMEOUT`], and every way of not
//! getting an answer (a timeout, a connection that fails, a failure status, a
//! `404`, an answer that does not decode) is `None`, with which the models
//! are listed as they are without platform mode's prices.
//!
//! The cache holds one entry per [`prices_cache_key`]: the tenant, from
//! [`crate::platform::tenant_owner`] (the key document's team, or the key
//! itself for a key in no team), and the key document's
//! `allowed_provider_groups`, sorted and deduplicated. Two keys of one tenant
//! in two groups therefore hold two entries, since a price is its group's
//! rate (§2.4), and two keys in one group share theirs. One tenant's entry, a
//! failure included, never answers another. An answer lives [`PRICES_TTL`], or until its
//! `valid_until` (the next scheduled price change) when that comes sooner,
//! but never less than [`PRICES_MIN_TTL`]. A `None` lives [`PRICES_FAILURE_TTL`], so a control plane that cannot answer
//! costs a tenant's listings at most one [`PRICES_TIMEOUT`] per
//! [`PRICES_FAILURE_TTL`]. Concurrent listings of one tenant share one fetch,
//! which a listing hanging up never cancels ([`SharedLookup`]), and the cache
//! holds at most [`CACHE_CAPACITY`](super::CACHE_CAPACITY) tenants.
//!
//! The answer decodes loosely: unknown fields are ignored, and every field
//! but `currency`, `items` (and, within an item, its `model_id`) has a
//! default. "A Control-Plane Response Decodes Loosely": a control plane newer
//! than this gateway adds fields it must ignore.

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

/// One model's price for the caller's tenant, per token, as the control plane
/// wrote it. What the listing shows as a model's `pricing`.
///
/// `currency` is the answer's, stamped on every item when it is decoded. It
/// is part of the price because a figure with no unit is not one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct ModelPricing {
    currency: String,
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

/// The control plane's answer, decoded loosely but for the three fields the
/// lookup hinges on: the currency every amount is in, and the items.
///
/// The list is `items`, not `models`: a gateway from before the currency
/// field requires `models` to be an array of prices and has no `currency`, so
/// the rename makes it fail to decode this answer rather than show figures
/// under no currency.
#[derive(Deserialize)]
struct PricesWire {
    currency: String,
    #[serde(default)]
    valid_until: Option<DateTime<Utc>>,
    items: Vec<ModelPriceWire>,
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
        let PricesWire {
            currency,
            valid_until,
            items,
        } = wire;
        TenantPricesAnswer {
            valid_until,
            by_model: items
                .into_iter()
                .map(|m| {
                    let pricing = ModelPricing {
                        currency: currency.clone(),
                        ..m.pricing
                    };
                    (m.model_id, pricing)
                })
                .collect(),
        }
    }
}

/// A tenant's prices; `None` is the control plane's `404`.
type Prices = Option<Arc<TenantPricesAnswer>>;

/// The prices cache's key for `auth`'s request: the tenant plus the provider
/// groups its key document allows, sorted and deduplicated.
///
/// A price depends on the group, so two keys of one tenant in two groups must
/// not share an entry, and two keys in one group must. The groups are the
/// key document's `allowed_provider_groups` — what the key routes by — and
/// not the tenant's, because a price follows the key that asks.
///
/// Which model identity: this keys on **neither** half. It keys on the caller
/// and the groups it may reach, and the prices inside the entry are looked up
/// per requested model by its resource id (`pricing` in `models.rs`), so a
/// wildcard-served name, a routing parent and a direct model all read the
/// price of the model the caller addressed. No dispatched target is involved:
/// the answer is one lookup per key, not per target.
///
/// The key is JSON so that no pair of distinct inputs can render the same
/// string: [`crate::platform::tenant_owner`] already holds a `:` for a
/// tenantless key, and a group name may hold anything.
pub(crate) fn prices_cache_key(auth: &crate::auth::AuthenticatedKey) -> String {
    let mut groups: Vec<&str> = auth
        .key()
        .allowed_provider_groups
        .iter()
        .map(String::as_str)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    serde_json::json!([crate::platform::tenant_owner(auth), groups]).to_string()
}

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
    /// The prices of the tenant and provider groups [`prices_cache_key`]
    /// names, fetched with the caller's key `key_id` when the cache has no
    /// entry for them. `None` when the control plane gave no answer in time;
    /// never waits past [`PRICES_TIMEOUT`].
    pub(crate) async fn tenant_prices(
        &self,
        cache_key: &str,
        key_id: &str,
    ) -> Option<Arc<TenantPricesAnswer>> {
        self.prices
            .get(cache_key, || {
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
            "currency": "CNY",
            "valid_until": null,
            "items": [{"model_id": model_id, "input": "0.000006", "output": "0.00002"}],
        })
    }

    fn pricing(input: &str, output: &str) -> ModelPricing {
        ModelPricing {
            currency: "CNY".to_string(),
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
                body["items"][0]["another_new_field"] = json!(7);
                ok(body)
            }
            // Only what the lookup hinges on.
            _ => ok(json!({
                "currency": "CNY",
                "items": [{"model_id": "m-1", "input_cache_read": "0.0000006"}],
            })),
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
                currency: "CNY".to_string(),
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
            "k-no-items" => ok(json!({"currency": "CNY", "tenant_id": "t-1", "valid_until": null})),
            "k-no-currency" => ok(json!({
                "tenant_id": "t-1",
                "valid_until": null,
                "items": [{"model_id": "m-1", "input": "0.000006"}],
            })),
            "k-numbers" => ok(json!({
                "currency": "CNY",
                "items": [{"model_id": "m-1", "input": 0.000006}],
            })),
            _ => ok(priced("m-1")),
        }))
        .await;
        let client = stand_in.client();
        for key in [
            "k-missing",
            "k-failing",
            "k-garbled",
            "k-no-items",
            "k-no-currency",
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
        assert_eq!(before, 6);
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

    /// The currency is not defaulted either: an answer without it gives no
    /// price at all, so a listing shows the models without pricing rather than
    /// figures whose unit nobody named. The unit is not guessed, and a price
    /// the control plane does send carries it.
    #[tokio::test]
    async fn a_prices_answer_without_its_currency_lists_no_pricing() {
        let stand_in = StandIn::start(Arc::new(|key| match key {
            // Every amount the answer ever gives, under no currency.
            "k-no-currency" => {
                let mut body = priced("m-1");
                body.as_object_mut().unwrap().remove("currency");
                ok(body)
            }
            _ => ok(priced("m-1")),
        }))
        .await;
        let client = stand_in.client();
        assert!(client
            .tenant_prices("team:a", "k-no-currency")
            .await
            .is_none());
        // The same answer with its currency lists the price, and names it.
        let answer = client.tenant_prices("team:b", "k-ok").await.unwrap();
        assert_eq!(
            answer.price_of("m-1").map(|p| p.currency.as_str()),
            Some("CNY")
        );
    }

    /// The cache key is the tenant plus the key document's provider groups,
    /// sorted and deduplicated: two keys of one tenant in two groups hold two
    /// entries, and two keys in one group share theirs.
    #[tokio::test]
    async fn the_prices_cache_is_keyed_by_tenant_and_groups() {
        use aisix_core::resource::ResourceEntry;
        use aisix_core::{AisixSnapshot, ApiKey};

        fn key_of(
            entry_id: &str,
            team: Option<&str>,
            groups: &[&str],
        ) -> crate::auth::AuthenticatedKey {
            let snap = AisixSnapshot::new();
            let doc = json!({
                "key_hash": "h",
                "allowed_models": [],
                "team_id": team,
                "allowed_provider_groups": groups,
            });
            let entry = Arc::new(ResourceEntry::new(
                entry_id,
                serde_json::from_value::<ApiKey>(doc).unwrap(),
                1,
            ));
            crate::auth::AuthenticatedKey::new(&snap, entry, None, false)
        }
        let (k1, k2) = ("k-1", "k-2");

        // Two keys of one tenant in two groups: two entries.
        assert_ne!(
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a"])),
            prices_cache_key(&key_of(k2, Some("team-a"), &["grp-b"])),
            "a price follows the group"
        );
        // Two keys in one group: one entry, whichever key asks.
        assert_eq!(
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a"])),
            prices_cache_key(&key_of(k2, Some("team-a"), &["grp-a"])),
        );
        // The order and the repetition of the document's list do not matter.
        assert_eq!(
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-b", "grp-a"])),
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a", "grp-b"])),
            "a fixed order"
        );
        assert_eq!(
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a", "grp-a"])),
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a"])),
            "deduplicated"
        );
        // Two tenants in one group: two entries.
        assert_ne!(
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a"])),
            prices_cache_key(&key_of(k1, Some("team-b"), &["grp-a"])),
        );
        // Two keys in no team are two tenants, and a key with no group is the
        // tenant alone.
        assert_ne!(
            prices_cache_key(&key_of(k1, None, &["grp-a"])),
            prices_cache_key(&key_of(k2, None, &["grp-a"])),
        );
        assert_ne!(
            prices_cache_key(&key_of(k1, Some("team-a"), &[])),
            prices_cache_key(&key_of(k1, Some("team-a"), &["grp-a"])),
        );
    }

    /// The lookup holds one entry per key of the cache, so two keys of one
    /// tenant in two groups each fetch their own answer, and a second key in
    /// the first group's entry is served from it.
    #[tokio::test]
    async fn two_groups_of_one_tenant_fetch_twice_and_one_group_once() {
        let stand_in = StandIn::start(Arc::new(|key| {
            ok(match key {
                "k-a1" | "k-a2" => priced("m-a"),
                _ => priced("m-b"),
            })
        }))
        .await;
        let client = stand_in.client();
        let a = client
            .tenant_prices("[\"team:a\",[\"grp-a\"]]", "k-a1")
            .await;
        assert!(a.is_some());
        let b = client
            .tenant_prices("[\"team:a\",[\"grp-b\"]]", "k-b1")
            .await;
        assert!(b.is_some());
        assert_eq!(stand_in.hits(), 2, "two groups, two lookups");
        // The second key of the first group is served the first group's
        // answer, not the second group's.
        let a_again = client
            .tenant_prices("[\"team:a\",[\"grp-a\"]]", "k-a2")
            .await
            .unwrap();
        assert_eq!(stand_in.hits(), 2, "one group, one entry");
        assert!(a_again.price_of("m-a").is_some(), "A's own answer");
        assert!(a_again.price_of("m-b").is_none(), "not B's");
    }
}
