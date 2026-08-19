//! In-memory backend backed by `moka`.
//!
//! TTL strategy:
//! - The constructor's `ttl` argument is the **fallback** TTL — used
//!   when the proxy calls `put` (no per-policy override available).
//! - When the proxy calls `put_with_ttl` it ships the matching
//!   `CachePolicy::ttl_seconds`. Each entry then expires according to
//!   its own policy. moka's `Expiry` trait reads the per-entry TTL
//!   we stash next to the response.

use aisix_gateway::ChatResponse;
use async_trait::async_trait;
use moka::future::Cache as MokaCache;
use moka::Expiry;
use std::time::{Duration, Instant};

use crate::cache::{Cache, CacheError, CachedBody};

pub const DEFAULT_TTL: Duration = Duration::from_secs(300);
pub const DEFAULT_CAPACITY: u64 = 10_000;

/// What we actually store inside moka — the response plus the TTL the
/// caller asked for. The Expiry impl below reads the second field on
/// `expire_after_create` to set the per-entry deadline.
#[derive(Debug, Clone)]
struct Entry {
    response: ChatResponse,
    ttl: Duration,
}

/// Same per-entry-TTL shape as [`Entry`], for the non-chat endpoints whose
/// responses are stored as bytes.
#[derive(Debug, Clone)]
struct BodyEntry {
    body: CachedBody,
    ttl: Duration,
}

struct PerBodyEntryExpiry;

impl Expiry<String, BodyEntry> for PerBodyEntryExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &BodyEntry,
        _current_time: Instant,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

#[derive(Debug)]
pub struct MemoryCache {
    inner: MokaCache<String, Entry>,
    /// Separate store for the byte-bodied endpoints. A second moka cache
    /// rather than an enum value: the two have different sizes and eviction
    /// pressure, and a shared capacity would let a burst of embedding
    /// matrices evict every chat entry.
    bodies: MokaCache<String, BodyEntry>,
    /// Fallback TTL used by the no-override `put` path. Kept for the
    /// `ttl()` accessor + tests; not consulted by the Expiry impl.
    ttl: Duration,
}

/// Per-entry expiry that defers to the value's stashed `ttl`.
/// `expire_after_read` / `expire_after_update` return `None` so reads
/// don't extend an entry's life (semantic is "expires N seconds from
/// insert", not "expires N seconds from last access").
struct PerEntryExpiry;

impl Expiry<String, Entry> for PerEntryExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Entry,
        _current_time: Instant,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

impl MemoryCache {
    pub fn new(ttl: Duration, capacity: u64) -> Self {
        let inner = MokaCache::builder()
            .max_capacity(capacity)
            .expire_after(PerEntryExpiry)
            .build();
        let bodies = MokaCache::builder()
            .max_capacity(capacity)
            .expire_after(PerBodyEntryExpiry)
            .build();
        Self { inner, bodies, ttl }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_TTL, DEFAULT_CAPACITY)
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[async_trait]
impl Cache for MemoryCache {
    async fn get(&self, key: &str) -> Result<Option<ChatResponse>, CacheError> {
        Ok(self.inner.get(key).await.map(|e| e.response))
    }

    async fn put(&self, key: &str, value: ChatResponse) -> Result<(), CacheError> {
        self.inner
            .insert(
                key.to_string(),
                Entry {
                    response: value,
                    ttl: self.ttl,
                },
            )
            .await;
        Ok(())
    }

    async fn put_with_ttl(
        &self,
        key: &str,
        value: ChatResponse,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        self.inner
            .insert(
                key.to_string(),
                Entry {
                    response: value,
                    ttl,
                },
            )
            .await;
        Ok(())
    }

    async fn get_body(&self, key: &str) -> Result<Option<CachedBody>, CacheError> {
        Ok(self.bodies.get(key).await.map(|e| e.body))
    }

    async fn put_body_with_ttl(
        &self,
        key: &str,
        value: CachedBody,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        self.bodies
            .insert(key.to_string(), BodyEntry { body: value, ttl })
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_body() -> CachedBody {
        CachedBody {
            content_type: "application/json".into(),
            // Deliberately not valid UTF-8: the binary surfaces (TTS audio,
            // image bytes) go through this same store, and a String-typed
            // field would have silently mangled them.
            body: vec![0x00, 0xff, 0x7b, 0x7d],
            prompt_tokens: 7,
            completion_tokens: 0,
        }
    }

    #[tokio::test]
    async fn body_entries_round_trip_bytes_and_token_counts() {
        let cache = MemoryCache::with_defaults();
        assert!(cache.get_body("k").await.unwrap().is_none());
        cache
            .put_body_with_ttl("k", sample_body(), Duration::from_secs(60))
            .await
            .unwrap();
        let got = cache.get_body("k").await.unwrap().expect("stored body");
        assert_eq!(got.body, vec![0x00, 0xff, 0x7b, 0x7d]);
        assert_eq!(got.content_type, "application/json");
        assert_eq!(got.prompt_tokens, 7);
    }

    /// Chat responses and bodies share a key namespace at the call site, so
    /// the two stores must not answer each other's lookups — a chat hit
    /// decoded as a body (or the reverse) would serve one endpoint's answer
    /// on another.
    #[tokio::test]
    async fn body_and_chat_entries_do_not_answer_each_other() {
        let cache = MemoryCache::with_defaults();
        cache.put("same-key", sample_response()).await.unwrap();
        assert!(cache.get_body("same-key").await.unwrap().is_none());

        let fresh = MemoryCache::with_defaults();
        fresh
            .put_body_with_ttl("same-key", sample_body(), Duration::from_secs(60))
            .await
            .unwrap();
        assert!(fresh.get("same-key").await.unwrap().is_none());
    }
    use aisix_gateway::{ChatMessage, FinishReason, UsageStats};

    fn sample_response() -> ChatResponse {
        ChatResponse {
            id: "cmpl-1".into(),
            model: "m".into(),
            message: ChatMessage::assistant("hi back"),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(2, 3),
        }
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let cache = MemoryCache::with_defaults();
        cache.put("k1", sample_response()).await.unwrap();
        let got = cache.get("k1").await.unwrap().unwrap();
        assert_eq!(got.message.content_str(), "hi back");
        assert_eq!(got.usage.total_tokens, 5);
    }

    #[tokio::test]
    async fn get_for_missing_key_returns_none() {
        let cache = MemoryCache::with_defaults();
        assert!(cache.get("absent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ttl_eviction_drops_stale_entries() {
        let cache = MemoryCache::new(Duration::from_millis(50), 100);
        cache.put("k1", sample_response()).await.unwrap();
        assert!(cache.get("k1").await.unwrap().is_some());
        // Wait past TTL. Moka uses lazy eviction on read; one extra
        // milli of slack to clear the boundary.
        tokio::time::sleep(Duration::from_millis(120)).await;
        // Force housekeeping so the test isn't dependent on the random
        // background eviction tick.
        cache.inner.run_pending_tasks().await;
        assert!(cache.get("k1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_overwrites_previous_value_for_same_key() {
        let cache = MemoryCache::with_defaults();
        cache.put("k", sample_response()).await.unwrap();
        let mut updated = sample_response();
        updated.message.content = Some("second".into());
        cache.put("k", updated).await.unwrap();
        let got = cache.get("k").await.unwrap().unwrap();
        assert_eq!(got.message.content_str(), "second");
    }

    /// Per-entry TTL: two keys inserted at the same time with
    /// different TTLs must expire independently. Without the
    /// `Expiry` impl moka would use one global TTL and either both
    /// entries survive or both die — this test catches that
    /// regression.
    #[tokio::test]
    async fn put_with_ttl_uses_per_entry_expiry() {
        // Long-fallback cache so a regression that ignores the
        // per-entry TTL doesn't accidentally pass by global eviction.
        let cache = MemoryCache::new(Duration::from_secs(300), 100);
        cache
            .put_with_ttl("short", sample_response(), Duration::from_millis(50))
            .await
            .unwrap();
        cache
            .put_with_ttl("long", sample_response(), Duration::from_secs(60))
            .await
            .unwrap();

        // Both alive immediately after insert.
        assert!(cache.get("short").await.unwrap().is_some());
        assert!(cache.get("long").await.unwrap().is_some());

        // Wait past the short TTL only.
        tokio::time::sleep(Duration::from_millis(120)).await;
        cache.inner.run_pending_tasks().await;

        assert!(
            cache.get("short").await.unwrap().is_none(),
            "short-TTL entry should have expired",
        );
        assert!(
            cache.get("long").await.unwrap().is_some(),
            "long-TTL entry must survive past the short TTL",
        );
    }
}
