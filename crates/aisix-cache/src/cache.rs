//! [`Cache`] trait — the storage seam every backend implements against.
//!
//! Returning `Result<Option<…>, _>` rather than `Result<…, NotFound>`
//! makes the call site read naturally: cache miss is an expected control
//! flow, not an error.
//!
//! Held behind `Arc<dyn Cache>` in `ProxyState`. Trait objects need
//! `async_trait` until native async-fn-in-traits become dyn-compatible.

use aisix_gateway::ChatResponse;
use async_trait::async_trait;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache backend error: {0}")]
    Backend(String),
}

/// Outcome of a cache lookup. Public so the proxy can attach the
/// `x-aisix-cache: hit|miss` header without owning string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Miss,
}

impl CacheOutcome {
    pub fn as_header_value(self) -> &'static str {
        match self {
            CacheOutcome::Hit => "hit",
            CacheOutcome::Miss => "miss",
        }
    }
}

/// A cached client-facing response for an endpoint whose responses are not
/// chat-shaped.
///
/// The chat cache stores a typed [`ChatResponse`] because the chat handler
/// owns that shape end to end. Every other cacheable endpoint relays a body
/// it does not model — an Anthropic message, a Responses object, an
/// embedding matrix — so storing bytes is the only lossless option: a
/// round-trip through a chat struct would drop whatever fields the gateway
/// does not itself read.
///
/// The token counts are what the upstream reported when the entry was
/// written. A hit reports them as saved rather than as consumed, which is
/// how the cache's value shows up in the usage ledger at all.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedBody {
    /// Content type to replay, so a JSON body and a binary one both come
    /// back as themselves.
    pub content_type: String,
    #[serde(with = "base64_bytes")]
    pub body: Vec<u8>,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

/// Bodies are arbitrary bytes — including the binary surfaces — so they are
/// base64'd rather than assumed to be UTF-8 JSON.
mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

#[async_trait]
pub trait Cache: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Result<Option<ChatResponse>, CacheError>;
    async fn put(&self, key: &str, value: ChatResponse) -> Result<(), CacheError>;

    /// Insert with an explicit TTL override. Used by the proxy when
    /// the matching `CachePolicy` carries a `ttl_seconds` value, so
    /// each entry expires according to its own policy rather than the
    /// cache backend's global TTL. Backends that can't honor
    /// per-entry TTL must document the gap; the default impl falls
    /// back to `put` (= the backend's global TTL) so adding a new
    /// backend doesn't have to ship per-entry support up front.
    async fn put_with_ttl(
        &self,
        key: &str,
        value: ChatResponse,
        _ttl: Duration,
    ) -> Result<(), CacheError> {
        self.put(key, value).await
    }

    /// Read a stored response body for a non-chat endpoint.
    ///
    /// Required rather than defaulted: a backend that answered `None` by
    /// default would leave those endpoints permanently missing with nothing
    /// to show that caching never engaged — the same invisible-gap shape
    /// that an emit function with no caller has.
    async fn get_body(&self, key: &str) -> Result<Option<CachedBody>, CacheError>;

    /// Store a response body for a non-chat endpoint under an explicit TTL.
    async fn put_body_with_ttl(
        &self,
        key: &str,
        value: CachedBody,
        ttl: Duration,
    ) -> Result<(), CacheError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_outcome_emits_canonical_header_string() {
        assert_eq!(CacheOutcome::Hit.as_header_value(), "hit");
        assert_eq!(CacheOutcome::Miss.as_header_value(), "miss");
    }
}
