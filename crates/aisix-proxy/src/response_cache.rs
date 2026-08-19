//! Response caching for the endpoints whose bodies the gateway relays
//! rather than models.
//!
//! `/v1/chat/completions` caches a typed [`aisix_gateway::ChatResponse`]
//! because the chat handler owns that shape end to end. Every other
//! cacheable endpoint hands the caller a body it does not model — an
//! Anthropic message, a Responses object, an embedding matrix — so those
//! entries are stored as bytes plus a content type ([`CachedBody`]).
//!
//! One gate object per request, resolved once, so the read path, the write
//! path and the telemetry all agree on which policy matched and under which
//! key. The endpoints that use it differ only in what they put into the key
//! material and how they rebuild a response from a hit.
//!
//! Deliberately EXACT-layer only. The semantic layer answers a request with
//! a *different* request's response when the two are similar enough, which
//! is sound for a chat completion and wrong for an embedding: the whole
//! point of an embedding is that a paraphrase maps to a different vector, so
//! serving a near-match would hand back a vector for text the caller never
//! sent. Rather than enable it where it is safe and skip it where it is not
//! — a distinction no operator could see from the policy — the layer stays
//! chat-only until the policy can express it.

use std::sync::Arc;
use std::time::Duration;

use aisix_cache::{body_fingerprint, Cache, CachedBody};
use aisix_core::models::CacheScope;
use aisix_core::AisixSnapshot;

use crate::auth::AuthenticatedKey;
use crate::chat::CacheStatus;
use crate::state::ProxyState;

/// Ceiling on a buffered response body considered for caching.
///
/// Sized as a backstop rather than a policy: the endpoints that cache here
/// return a message, a Responses object or an embedding matrix, all far
/// below this. A body above it is relayed uncached rather than rejected.
pub(crate) const MAX_CACHED_BODY_BYTES: usize = 32 << 20;

/// What the cache did for one request, as the usage event reports it.
///
/// One struct rather than three loose parameters: the status and the saved
/// counts are only meaningful together, and an emit site that carried the
/// status but dropped the counts would report a hit that saved nothing.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CacheTelemetry {
    pub(crate) status: CacheStatus,
    /// Upstream tokens the hit avoided spending. Zero unless `status` is
    /// `Hit`.
    pub(crate) saved_input_tokens: u32,
    pub(crate) saved_output_tokens: u32,
}

impl CacheTelemetry {
    pub(crate) fn hit(body: &CachedBody) -> Self {
        Self {
            status: CacheStatus::Hit,
            saved_input_tokens: body.prompt_tokens,
            saved_output_tokens: body.completion_tokens,
        }
    }

    pub(crate) fn of(status: CacheStatus) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    /// `cache_hit_layer` for the usage event — this gate is exact-only.
    pub(crate) fn layer(&self) -> String {
        if matches!(self.status, CacheStatus::Hit) {
            "exact".to_string()
        } else {
            String::new()
        }
    }
}

/// A resolved cache gate for one request on one endpoint.
pub(crate) struct BodyCache {
    cache: Arc<dyn Cache>,
    key: String,
    ttl: Duration,
    policy_name: String,
    /// `Cache-Control: no-cache` — skip the read, still refresh the entry.
    no_cache: bool,
    /// `Cache-Control: no-store` — read allowed, writes suppressed.
    no_store: bool,
}

impl BodyCache {
    /// Resolve the gate for this request, or `None` when no enabled policy
    /// covers it or the policy's backend is unavailable.
    ///
    /// `key_material` is the endpoint's canonical request identity. Callers
    /// pass the fields that change the upstream answer; `serde_json`'s maps
    /// are sorted, so a serialized request body is already independent of
    /// the caller's JSON key order.
    pub(crate) fn resolve(
        state: &ProxyState,
        snapshot: &AisixSnapshot,
        auth: &AuthenticatedKey,
        headers: Option<&axum::http::HeaderMap>,
        endpoint: &str,
        model_name: &str,
        key_material: &str,
    ) -> Option<Self> {
        // Same match rule as the chat gate: the first enabled policy whose
        // `applies_to` accepts (model, caller). A policy has no endpoint
        // dimension, so one written for a model covers that model wherever
        // it is addressed.
        let policies = snapshot.cache_policies.entries();
        let entry = policies.iter().find(|entry| {
            entry.value.enabled
                && entry
                    .value
                    .parsed_applies_to()
                    .matches(model_name, &auth.entry.id)
        })?;
        let cache = state
            .cache
            .as_ref()?
            .for_policy_backend(entry.value.backend, &entry.id, &entry.value.name)?
            .clone();
        let scope_api_key = match entry.value.scope {
            CacheScope::ApiKey => auth.entry.id.as_str(),
            CacheScope::Env => "",
        };
        let generation = entry.value.purge_generation.to_string();
        let key = body_fingerprint(&[
            endpoint,
            model_name,
            key_material,
            &entry.id,
            &generation,
            scope_api_key,
        ]);
        let directives = crate::chat::cache_control_directives_of(headers);
        Some(Self {
            cache,
            key,
            ttl: Duration::from_secs(u64::from(entry.value.ttl_seconds)),
            policy_name: entry.value.name.clone(),
            no_cache: directives.0,
            no_store: directives.1,
        })
    }

    /// Status to report when the gate produced no hit.
    pub(crate) fn miss_status(&self) -> CacheStatus {
        if self.no_cache {
            CacheStatus::Bypass
        } else {
            CacheStatus::Miss
        }
    }

    /// Read path. `None` on a miss, on a caller bypass, or on a backend
    /// failure — a cache that cannot answer is a miss, never an error the
    /// caller sees.
    pub(crate) async fn lookup(&self) -> Option<CachedBody> {
        if self.no_cache {
            return None;
        }
        match self.cache.get_body(&self.key).await {
            Ok(hit @ Some(_)) => hit,
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    target: "aisix::cache",
                    policy_name = %self.policy_name,
                    error = %err,
                    "cache lookup failed; request proceeds uncached",
                );
                None
            }
        }
    }

    /// Write path, called only for a response the caller actually received.
    pub(crate) async fn store(&self, body: CachedBody) {
        if self.no_store {
            return;
        }
        if let Err(err) = self
            .cache
            .put_body_with_ttl(&self.key, body, self.ttl)
            .await
        {
            tracing::warn!(
                target: "aisix::cache",
                policy_name = %self.policy_name,
                error = %err,
                "cache write failed; the entry is simply absent next time",
            );
        }
    }

    /// Record the request's outcome on `aisix_cache_events_total`, the same
    /// series the chat gate feeds, so one dashboard covers every endpoint.
    pub(crate) fn record(&self, state: &ProxyState, status: CacheStatus) {
        state
            .metrics
            .record_cache_event(&self.policy_name, status.as_str());
    }
}

/// Attach the cache headers the chat surface already publishes, so a client
/// can tell a replayed answer from a fresh one on any endpoint.
pub(crate) fn apply_cache_headers(response: &mut axum::response::Response, status: CacheStatus) {
    if matches!(status, CacheStatus::Disabled) {
        return;
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(status.as_str()) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(crate::chat::CACHE_HEADER),
            value,
        );
    }
    if matches!(status, CacheStatus::Hit) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(crate::chat::CACHE_LAYER_HEADER),
            axum::http::HeaderValue::from_static("exact"),
        );
    }
}
