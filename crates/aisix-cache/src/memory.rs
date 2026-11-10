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
/// Bytes of responses each of the two stores holds. In bytes because a response
/// can be anywhere from a line of chat to tens of megabytes of embedding matrix:
/// a count of entries bounds nothing.
pub const DEFAULT_CAPACITY: u64 = 256 << 20;

/// What we actually store inside moka — the response plus the TTL the
/// caller asked for. The Expiry impl below reads the second field on
/// `expire_after_create` to set the per-entry deadline.
#[derive(Debug, Clone)]
struct Entry {
    response: ChatResponse,
    ttl: Duration,
    /// The response's serialized size, what eviction charges it.
    weight: u32,
}

/// Same per-entry-TTL shape as [`Entry`], for the non-chat endpoints whose
/// responses are stored as bytes.
#[derive(Debug, Clone)]
struct BodyEntry {
    body: CachedBody,
    ttl: Duration,
    weight: u32,
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

    /// An overwrite is a fresh entry: it lives its own `ttl` from now. moka's
    /// default keeps the replaced entry's deadline, so a refreshed answer
    /// would expire on the old one's clock.
    fn expire_after_update(
        &self,
        _key: &String,
        value: &BodyEntry,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
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
/// `expire_after_read` keeps moka's default, which leaves the deadline
/// alone, so reads don't extend an entry's life (semantic is "expires N
/// seconds from the write", not "expires N seconds from last access").
/// `expire_after_update` restarts the clock: an overwrite is a fresh write.
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

    /// moka's default keeps the replaced entry's deadline, so a refreshed
    /// answer (a `no-cache` revalidation writes the same key) would expire
    /// on the old one's clock, early or late by its new `ttl`.
    fn expire_after_update(
        &self,
        _key: &String,
        value: &Entry,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

impl MemoryCache {
    /// `capacity` is the bytes each of the two stores holds.
    pub fn new(ttl: Duration, capacity: u64) -> Self {
        let inner = MokaCache::builder()
            .max_capacity(capacity)
            .weigher(|_, entry: &Entry| entry.weight)
            .expire_after(PerEntryExpiry)
            .build();
        let bodies = MokaCache::builder()
            .max_capacity(capacity)
            .weigher(|_, entry: &BodyEntry| entry.weight)
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

fn weight_of(bytes: usize) -> u32 {
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

#[async_trait]
impl Cache for MemoryCache {
    /// moka's own count: O(1), and it trails inserts and expiries until
    /// the cache runs its pending maintenance.
    fn local_entry_count(&self) -> Option<u64> {
        Some(self.inner.entry_count())
    }

    async fn get(&self, key: &str) -> Result<Option<ChatResponse>, CacheError> {
        Ok(self.inner.get(key).await.map(|e| e.response))
    }

    async fn put(&self, key: &str, value: ChatResponse) -> Result<(), CacheError> {
        self.put_with_ttl(key, value, self.ttl).await
    }

    async fn put_with_ttl(
        &self,
        key: &str,
        mut value: ChatResponse,
        ttl: Duration,
    ) -> Result<(), CacheError> {
        // Same as a serialized entry: a hit was served by no upstream.
        value.service_tier = None;
        let weight = serde_json::to_vec(&value).map_or(u32::MAX, |json| weight_of(json.len()));
        self.inner
            .insert(
                key.to_string(),
                Entry {
                    response: value,
                    ttl,
                    weight,
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
        let weight = weight_of(value.body.len());
        self.bodies
            .insert(
                key.to_string(),
                BodyEntry {
                    body: value,
                    ttl,
                    weight,
                },
            )
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

    /// The store is bounded by what it holds, not how many: ten thousand entries
    /// of a few megabytes each is tens of gigabytes.
    #[tokio::test]
    async fn the_store_is_bounded_in_bytes_whatever_the_entry_count() {
        let cache = MemoryCache::new(Duration::from_secs(60), 4 << 20);
        for i in 0..32 {
            let mut body = sample_body();
            body.body = vec![0u8; 1 << 20];
            cache
                .put_body_with_ttl(&format!("k{i}"), body, Duration::from_secs(60))
                .await
                .unwrap();
        }
        cache.bodies.run_pending_tasks().await;
        assert!(
            cache.bodies.weighted_size() <= 4 << 20,
            "held {} bytes",
            cache.bodies.weighted_size()
        );
        assert!(cache.bodies.entry_count() <= 4);

        // One entry that alone exceeds the store does not displace it.
        let mut huge = sample_body();
        huge.body = vec![0u8; 8 << 20];
        cache
            .put_body_with_ttl("huge", huge, Duration::from_secs(60))
            .await
            .unwrap();
        cache.bodies.run_pending_tasks().await;
        assert!(cache.get_body("huge").await.unwrap().is_none());
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
            logprobs: None,
            more_choices: Vec::new(),
            id: "cmpl-1".into(),
            model: "m".into(),
            message: ChatMessage::assistant("hi back"),
            finish_reason: FinishReason::Stop,
            usage: UsageStats::new(2, 3),
            service_tier: None,
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
    async fn a_hit_reports_no_service_tier() {
        // Like a serialized entry: no upstream served a hit.
        let cache = MemoryCache::with_defaults();
        let mut resp = sample_response();
        resp.service_tier = Some(aisix_gateway::ServiceTier::OpenAi("priority".into()));
        cache.put("k1", resp).await.unwrap();
        assert_eq!(cache.get("k1").await.unwrap().unwrap().service_tier, None);
    }

    #[tokio::test]
    async fn get_for_missing_key_returns_none() {
        let cache = MemoryCache::with_defaults();
        assert!(cache.get("absent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ttl_eviction_drops_stale_entries() {
        let cache = MemoryCache::new(Duration::from_millis(50), 1 << 20);
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

    /// An overwrite of a live key lives its own TTL from the overwrite, not
    /// the replaced entry's remaining time: a refresh with an equal TTL must
    /// outlive the original deadline, and one with a shorter TTL must not
    /// inherit the original's longer one.
    #[tokio::test]
    async fn an_overwrite_restarts_the_entrys_ttl() {
        let cache = MemoryCache::new(Duration::from_secs(300), 1 << 20);
        let ttl = Duration::from_millis(400);
        cache
            .put_with_ttl("eq", sample_response(), ttl)
            .await
            .unwrap();
        cache
            .put_body_with_ttl("eq", sample_body(), ttl)
            .await
            .unwrap();
        cache
            .put_with_ttl("shorter", sample_response(), Duration::from_secs(60))
            .await
            .unwrap();
        cache
            .put_body_with_ttl("shorter", sample_body(), Duration::from_secs(60))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(250)).await;
        let mut refreshed = sample_response();
        refreshed.message.content = Some("refreshed".into());
        cache.put_with_ttl("eq", refreshed, ttl).await.unwrap();
        let mut refreshed_body = sample_body();
        refreshed_body.body = b"refreshed".to_vec();
        cache
            .put_body_with_ttl("eq", refreshed_body, ttl)
            .await
            .unwrap();
        cache
            .put_with_ttl("shorter", sample_response(), Duration::from_millis(50))
            .await
            .unwrap();
        cache
            .put_body_with_ttl("shorter", sample_body(), Duration::from_millis(50))
            .await
            .unwrap();

        // Past the first write's deadline (400ms) and the shorter refresh's
        // (50ms), well inside the equal refresh's own (650ms).
        tokio::time::sleep(Duration::from_millis(250)).await;
        cache.inner.run_pending_tasks().await;
        cache.bodies.run_pending_tasks().await;

        let chat = cache
            .get("eq")
            .await
            .unwrap()
            .expect("refreshed chat entry");
        assert_eq!(chat.message.content_str(), "refreshed");
        let body = cache.get_body("eq").await.unwrap().expect("refreshed body");
        assert_eq!(body.body, b"refreshed".to_vec());
        assert!(
            cache.get("shorter").await.unwrap().is_none(),
            "a shorter refresh must not keep the old 60s deadline"
        );
        assert!(cache.get_body("shorter").await.unwrap().is_none());
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
        let cache = MemoryCache::new(Duration::from_secs(300), 1 << 20);
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
