//! `GET /v1/key`: the caller's own key, its quota at every level, read with
//! the key itself.
//!
//! The answer comes from the control plane's `GET /dp/v1/keys/{key_id}/quota`
//! for the key the request authenticated with, so a caller can only ever read
//! its own key. A request is refused when any level applying to the key is
//! used up, so the answer names the smallest remaining amount
//! (`effective_remaining_usd`) and the level that holds it (`binding_level`),
//! beside every quota the key's holder may see and, for an owner's, admin's or
//! billing member's key, the organization's balance. Amounts are the control
//! plane's exact decimal strings, passed through: the gateway does no money
//! arithmetic.
//!
//! The shape follows the hosted routers' own-key route (a `data` object with
//! `budgets` of `limit_usd`, `spend_usd` and `remaining_usd`, amounts as
//! strings) and adds `binding_level`, because the smallest remaining level
//! binds.
//!
//! - No account client (a gateway reading a resources file, or a managed
//!   gateway without a certificate bundle), or a key the control plane does
//!   not know: `404`.
//! - A control plane that cannot answer: `503`
//!   ([`ProxyError::AccountUnavailable`]).
//!
//! Answers are cached per key for [`QUOTA_TTL`], a `404` for
//! [`NOT_FOUND_TTL`] and a failure for [`FAILURE_TTL`], so the figures lag
//! spend by a few seconds; `as_of` says when the control plane computed them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Deserializer, Serialize};

use super::{
    key_path, AccountClient, AccountError, SharedLookup, ACCOUNT_TIMEOUT, FAILURE_TTL,
    NOT_FOUND_TTL, QUOTA_TTL,
};
use crate::auth::AuthenticatedKey;
use crate::error::{ErrorEnvelope, ProxyError};
use crate::request_id::RequestId;
use crate::state::ProxyState;

/// The organization's money, shown to an owner's, admin's or billing
/// member's key.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Balance {
    billing_mode: Option<String>,
    balance_usd: Option<String>,
    credit_limit_usd: Option<String>,
    available_usd: Option<String>,
}

/// One quota that applies to the key, with its spend now.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Budget {
    scope: Option<String>,
    set_by: Option<String>,
    kind: Option<String>,
    period: Option<String>,
    limit_usd: Option<String>,
    spend_usd: Option<String>,
    remaining_usd: Option<String>,
    period_start: Option<String>,
    counted_from: Option<String>,
    exhausted: Option<bool>,
}

/// The control plane's answer for one key.
///
/// Decoded loosely: unknown fields are ignored and every field but
/// `effective_remaining_usd`, which the answer exists to give, has a default.
/// The route renders its own shape from these fields, so nothing the
/// control plane adds reaches a caller until it is named here.
#[derive(Debug, Deserialize)]
pub(crate) struct KeyQuotaAnswer {
    #[serde(default)]
    as_of: Option<String>,
    #[serde(default)]
    org: Option<Balance>,
    #[serde(default)]
    quotas: Vec<Budget>,
    /// Present in every answer, `null` when no level applies.
    #[serde(deserialize_with = "present")]
    effective_remaining_usd: Option<String>,
    #[serde(default)]
    binding_level: Option<String>,
}

/// A nullable field that must be written: unlike a plain `Option` field, a
/// missing one fails the decode.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(d)
}

/// A lookup's outcome: `Ok(None)` is the control plane's `404`.
type QuotaLookup = Result<Option<Arc<KeyQuotaAnswer>>, AccountError>;

/// The quota lookups by key id.
pub(crate) type QuotaCache = SharedLookup<Option<Arc<KeyQuotaAnswer>>>;

/// An answer lives [`QUOTA_TTL`] from when it was fetched, a `404`
/// [`NOT_FOUND_TTL`]; a failure lives [`FAILURE_TTL`].
fn quota_ttl(answer: &Option<Arc<KeyQuotaAnswer>>) -> Duration {
    match answer {
        Some(_) => QUOTA_TTL,
        None => NOT_FOUND_TTL,
    }
}

pub(super) fn quota_cache() -> QuotaCache {
    SharedLookup::new(ACCOUNT_TIMEOUT, quota_ttl, FAILURE_TTL)
}

impl AccountClient {
    /// The quota answer for key `key_id`, `None` for a key the control plane
    /// does not know. Concurrent lookups of one key share one request, which
    /// a caller hanging up never cancels.
    pub(crate) async fn key_quota(&self, key_id: &str) -> QuotaLookup {
        self.quotas
            .get(key_id, || {
                let (client, path) = (self.clone(), key_path(key_id, "quota"));
                async move {
                    client
                        .get_json::<KeyQuotaAnswer>(&path, &[], ACCOUNT_TIMEOUT)
                        .await
                        .map(|answer| answer.map(Arc::new))
                }
            })
            .await
    }
}

/// What `GET /v1/key` answers with.
#[derive(Serialize)]
struct KeyInfo<'a> {
    data: KeyData<'a>,
}

#[derive(Serialize)]
struct KeyData<'a> {
    as_of: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    balance: Option<&'a Balance>,
    budgets: &'a [Budget],
    effective_remaining_usd: Option<&'a str>,
    binding_level: Option<&'a str>,
}

impl<'a> From<&'a KeyQuotaAnswer> for KeyInfo<'a> {
    fn from(answer: &'a KeyQuotaAnswer) -> Self {
        KeyInfo {
            data: KeyData {
                as_of: answer.as_of.as_deref(),
                balance: answer.org.as_ref(),
                budgets: &answer.quotas,
                effective_remaining_usd: answer.effective_remaining_usd.as_deref(),
                binding_level: answer.binding_level.as_deref(),
            },
        }
    }
}

/// The one `404` of the route: no control plane, or a key it does not know.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(
            ErrorEnvelope::new(
                "No account information is available for this API key.",
                "invalid_request_error",
            )
            .with_code("not_found"),
        ),
    )
        .into_response()
}

/// `GET /v1/key`.
pub(crate) async fn key_info(
    State(state): State<ProxyState>,
    auth: AuthenticatedKey,
    method: Method,
    request_id: Option<Extension<RequestId>>,
) -> Response {
    let started = Instant::now();
    let response = match state.account.as_deref() {
        None => not_found(),
        Some(account) => match account.key_quota(&auth.entry.id).await {
            Ok(Some(answer)) => Json(KeyInfo::from(&*answer)).into_response(),
            Ok(None) => not_found(),
            Err(AccountError::Unavailable) => ProxyError::AccountUnavailable.into_response(),
        },
    };
    crate::models::record_discovery(
        &state,
        &auth,
        &method,
        "/v1/key",
        request_id.as_ref(),
        &response,
        started,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use aisix_core::resource::ResourceEntry;
    use aisix_core::snapshot::SnapshotHandle;
    use aisix_core::{AisixSnapshot, ApiKey, ProxyConfig};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use futures::StreamExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use crate::account::stand_in::StandIn;
    use crate::account::CACHE_CAPACITY;

    /// A control plane's answer for a key with every level visible.
    fn full_answer() -> Value {
        json!({
            "key_id": "k-owner",
            "as_of": "2026-11-07T10:00:00Z",
            "org": {
                "billing_mode": "prepaid",
                "balance_usd": "0.970000",
                "credit_limit_usd": "0.000000",
                "available_usd": "0.970000",
            },
            "quotas": [{
                "scope": "team",
                "set_by": "operator",
                "kind": "periodic",
                "period": "month",
                "limit_usd": "0.500000",
                "spend_usd": "0.030000",
                "remaining_usd": "0.470000",
                "period_start": "2026-11-01T00:00:00Z",
                "counted_from": null,
                "exhausted": false,
            }],
            "effective_remaining_usd": "0.470000",
            "binding_level": "organization",
        })
    }

    fn ok(body: Value) -> (StatusCode, String) {
        (StatusCode::OK, body.to_string())
    }

    #[tokio::test]
    async fn an_answer_with_unknown_fields_still_decodes() {
        let stand_in = StandIn::start(Arc::new(|_| {
            let mut body = full_answer();
            body["a_field_from_a_newer_release"] = json!({"nested": [1, 2]});
            body["quotas"][0]["another_new_field"] = json!(7);
            body["org"]["yet_another"] = json!("x");
            ok(body)
        }))
        .await;
        let answer = stand_in
            .client()
            .key_quota("k-owner")
            .await
            .unwrap()
            .expect("an answer");
        assert_eq!(answer.effective_remaining_usd.as_deref(), Some("0.470000"));
        assert_eq!(answer.quotas[0].limit_usd.as_deref(), Some("0.500000"));
        assert_eq!(
            answer.org.as_ref().unwrap().available_usd.as_deref(),
            Some("0.970000")
        );
    }

    #[tokio::test]
    async fn an_answer_missing_an_optional_field_still_decodes() {
        let stand_in = StandIn::start(Arc::new(|_| {
            ok(json!({"quotas": [{"scope": "api_key"}], "effective_remaining_usd": null}))
        }))
        .await;
        let answer = stand_in
            .client()
            .key_quota("k-member")
            .await
            .unwrap()
            .expect("an answer");
        assert!(answer.org.is_none());
        assert!(answer.binding_level.is_none());
        assert!(answer.as_of.is_none());
        assert_eq!(answer.quotas[0].scope.as_deref(), Some("api_key"));
        assert!(answer.quotas[0].limit_usd.is_none());
    }

    /// The one field the answer exists to give is not defaulted: an answer
    /// without it is not an answer.
    #[tokio::test]
    async fn an_answer_without_its_effective_remaining_is_unavailable() {
        let stand_in = StandIn::start(Arc::new(|_| {
            let mut body = full_answer();
            body.as_object_mut()
                .unwrap()
                .remove("effective_remaining_usd");
            ok(body)
        }))
        .await;
        assert_eq!(
            stand_in.client().key_quota("k-owner").await.unwrap_err(),
            AccountError::Unavailable
        );
    }

    #[tokio::test]
    async fn a_control_plane_404_is_none_and_a_timeout_is_unavailable() {
        let stand_in = StandIn::start(Arc::new(|key| match key {
            "missing" => (
                StatusCode::NOT_FOUND,
                json!({"code": "not_found"}).to_string(),
            ),
            "failing" => (StatusCode::INTERNAL_SERVER_ERROR, "{}".to_string()),
            "garbled" => (StatusCode::OK, "not json".to_string()),
            _ => ok(full_answer()),
        }))
        .await;
        let client = stand_in.client();
        assert!(client.key_quota("missing").await.unwrap().is_none());
        assert_eq!(
            client.key_quota("failing").await.unwrap_err(),
            AccountError::Unavailable
        );
        assert_eq!(
            client.key_quota("garbled").await.unwrap_err(),
            AccountError::Unavailable
        );
        // A failure is answered from the cache for FAILURE_TTL, then asked
        // again.
        let before = stand_in.hits();
        assert!(client.key_quota("failing").await.is_err());
        assert_eq!(stand_in.hits(), before, "within FAILURE_TTL");
        tokio::time::sleep(FAILURE_TTL + Duration::from_millis(300)).await;
        assert!(client.key_quota("failing").await.is_err());
        assert_eq!(stand_in.hits(), before + 1, "past FAILURE_TTL");

        let stalled =
            StandIn::start_with_delay(Arc::new(|_| ok(full_answer())), Duration::from_secs(3))
                .await;
        let started = Instant::now();
        assert_eq!(
            stalled.client().key_quota("k-owner").await.unwrap_err(),
            AccountError::Unavailable
        );
        let waited = started.elapsed();
        assert!(
            waited >= ACCOUNT_TIMEOUT && waited < Duration::from_millis(2_800),
            "gave up after {waited:?}"
        );
    }

    #[tokio::test]
    async fn a_key_lookup_is_cached_per_key_for_its_ttl() {
        // Slow enough that all ten lookups below are in flight together.
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_| ok(full_answer())), Duration::from_millis(200))
                .await;
        let client = stand_in.client();
        let lookups = (0..10).map(|_| client.key_quota("key-a"));
        for answer in futures::future::join_all(lookups).await {
            assert!(answer.unwrap().is_some());
        }
        assert_eq!(stand_in.hits(), 1, "ten concurrent lookups of one key");
        assert!(client.key_quota("key-b").await.unwrap().is_some());
        assert_eq!(stand_in.hits(), 2, "another key is its own entry");
        assert!(client.key_quota("key-a").await.unwrap().is_some());
        assert_eq!(stand_in.hits(), 2, "still within the TTL");

        tokio::time::sleep(QUOTA_TTL + Duration::from_millis(300)).await;
        assert!(client.key_quota("key-a").await.unwrap().is_some());
        assert_eq!(stand_in.hits(), 3, "past the TTL");
    }

    /// The caller that started a lookup going away (its client hung up)
    /// cancels nothing: a caller waiting meanwhile gets that lookup's
    /// answer, and the answer is cached.
    #[tokio::test]
    async fn a_caller_that_hangs_up_never_cancels_the_shared_lookup() {
        let stand_in =
            StandIn::start_with_delay(Arc::new(|_| ok(full_answer())), Duration::from_millis(300))
                .await;
        let client = stand_in.client();
        let first = tokio::time::timeout(Duration::from_millis(50), client.key_quota("k-owner"));
        let waiting = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            client.key_quota("k-owner").await
        };
        let (first, waiting) = tokio::join!(first, waiting);
        assert!(first.is_err(), "the first caller hung up");
        assert!(
            waiting.unwrap().is_some(),
            "the waiting caller got the answer"
        );
        assert_eq!(stand_in.hits(), 1, "one lookup, never restarted");
        assert!(client.key_quota("k-owner").await.unwrap().is_some());
        assert_eq!(stand_in.hits(), 1, "the answer was cached");
    }

    /// An answer is kept for `QUOTA_TTL`, past the shorter TTLs of a `404`
    /// and a failure.
    #[tokio::test]
    async fn a_quota_answer_outlives_the_shorter_ttls() {
        let stand_in = StandIn::start(Arc::new(|_| ok(full_answer()))).await;
        let client = stand_in.client();
        assert!(client.key_quota("key-a").await.unwrap().is_some());
        tokio::time::sleep(FAILURE_TTL.max(NOT_FOUND_TTL) + Duration::from_millis(300)).await;
        assert!(client.key_quota("key-a").await.unwrap().is_some());
        assert_eq!(stand_in.hits(), 1, "still within QUOTA_TTL");
    }

    #[tokio::test]
    async fn a_not_found_is_cached_for_its_own_shorter_ttl() {
        let stand_in =
            StandIn::start(Arc::new(|_| (StatusCode::NOT_FOUND, "{}".to_string()))).await;
        let client = stand_in.client();
        assert!(client.key_quota("gone").await.unwrap().is_none());
        assert!(client.key_quota("gone").await.unwrap().is_none());
        assert_eq!(stand_in.hits(), 1);
        // Past NOT_FOUND_TTL and well within QUOTA_TTL.
        tokio::time::sleep(NOT_FOUND_TTL + Duration::from_millis(300)).await;
        assert!(client.key_quota("gone").await.unwrap().is_none());
        assert_eq!(stand_in.hits(), 2);
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let stand_in = StandIn::start(Arc::new(|_| ok(full_answer()))).await;
        let client = stand_in.client();
        let keys = CACHE_CAPACITY as usize + 100;
        futures::stream::iter(0..keys)
            .map(|i| {
                let client = &client;
                async move { client.key_quota(&format!("key-{i}")).await }
            })
            .buffer_unordered(64)
            .for_each(|answer| async move { assert!(answer.unwrap().is_some()) })
            .await;
        assert_eq!(stand_in.hits(), keys);
        let held = client.quotas.entry_count().await;
        assert!(held > 0 && held <= CACHE_CAPACITY, "{held} entries held");
    }

    #[test]
    fn a_key_id_is_one_path_segment() {
        assert_eq!(key_path("abc-1", "quota"), "/dp/v1/keys/abc-1/quota");
        assert_eq!(
            key_path("a/b?c#d", "quota"),
            "/dp/v1/keys/a%2Fb%3Fc%23d/quota"
        );
    }

    // ── the route ───────────────────────────────────────────────────────────

    const OWNER: &str = "sk-key-lookup-owner";
    const MEMBER: &str = "sk-key-lookup-member";
    const STRANGER: &str = "sk-key-lookup-stranger";

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

    async fn get_key(router: axum::Router, bearer: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .uri("/v1/key")
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// The stand-in answers the owner's key with every level, the member's
    /// with no organization figures, the stranger's with `404`, and fails
    /// for every other key. Every answer carries fields this gateway does not
    /// know.
    async fn tenant_stand_in() -> StandIn {
        StandIn::start(Arc::new(|key| match key {
            "k-owner" => {
                let mut body = full_answer();
                body["internal_note"] = json!("not for callers");
                body["quotas"][0]["target_live"] = json!(true);
                ok(body)
            }
            "k-member" => ok(json!({
                "key_id": "k-member",
                "as_of": "2026-11-07T10:00:00Z",
                "org": null,
                "quotas": [{"scope": "api_key", "set_by": "member", "kind": "cumulative",
                            "period": null, "limit_usd": "0.050000", "spend_usd": "0.030000",
                            "remaining_usd": "0.020000", "period_start": null,
                            "counted_from": "2026-11-07T09:00:00Z", "exhausted": false}],
                "effective_remaining_usd": "0.020000",
                "binding_level": "key",
            })),
            "k-stranger" => (StatusCode::NOT_FOUND, "{}".to_string()),
            _ => (StatusCode::SERVICE_UNAVAILABLE, "{}".to_string()),
        }))
        .await
    }

    #[tokio::test]
    async fn the_route_answers_its_own_shape_for_the_callers_key() {
        let stand_in = tenant_stand_in().await;
        let app = router(Some(stand_in.client()), false);

        let (status, body) = get_key(app.clone(), OWNER).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"data": {
                "as_of": "2026-11-07T10:00:00Z",
                "balance": {
                    "billing_mode": "prepaid",
                    "balance_usd": "0.970000",
                    "credit_limit_usd": "0.000000",
                    "available_usd": "0.970000",
                },
                "budgets": [{
                    "scope": "team", "set_by": "operator", "kind": "periodic", "period": "month",
                    "limit_usd": "0.500000", "spend_usd": "0.030000", "remaining_usd": "0.470000",
                    "period_start": "2026-11-01T00:00:00Z", "counted_from": null, "exhausted": false,
                }],
                "effective_remaining_usd": "0.470000",
                "binding_level": "organization",
            }})
        );

        let (status, body) = get_key(app, MEMBER).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["data"].get("balance").is_none(), "{body}");
        assert_eq!(body["data"]["effective_remaining_usd"], "0.020000");
        assert_eq!(body["data"]["binding_level"], "key");
        assert_eq!(
            body["data"]["budgets"][0]["counted_from"],
            "2026-11-07T09:00:00Z"
        );

        // Each lookup named the caller's own key, and only it.
        let paths = stand_in.paths.lock().unwrap().clone();
        assert_eq!(
            paths,
            HashMap::from([
                ("/dp/v1/keys/k-owner/quota".to_string(), 1),
                ("/dp/v1/keys/k-member/quota".to_string(), 1),
            ])
        );
    }

    #[tokio::test]
    async fn a_key_the_control_plane_does_not_know_is_not_found() {
        let stand_in = tenant_stand_in().await;
        let (status, body) = get_key(router(Some(stand_in.client()), false), STRANGER).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn an_unavailable_control_plane_fails_only_the_lookup_with_503() {
        // A control plane that answers every key with a failure.
        let stand_in =
            StandIn::start(Arc::new(|_| (StatusCode::BAD_GATEWAY, "{}".to_string()))).await;
        let (status, body) = get_key(router(Some(stand_in.client()), false), OWNER).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["code"], "control_plane_unavailable");
        assert_eq!(
            body["error"]["message"],
            "Account information is temporarily unavailable."
        );

        // It names no upstream, so platform mode answers it as itself.
        let (status, body) = get_key(router(Some(stand_in.client()), true), OWNER).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["code"], "control_plane_unavailable");
        assert_eq!(
            body["error"]["message"],
            "Account information is temporarily unavailable."
        );
    }
}
