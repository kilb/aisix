//! Two-phase limiter keyed on an opaque `key` (the caller's ApiKey id /
//! policy bucket in production), backed by a pluggable [`RateStore`].
//!
//! Phase 1 — **pre-commit**, called before the upstream request fires:
//! - check concurrency (acquire a slot or fail)
//! - check + increment RPS / RPM / RPH / RPD counters
//! - *check-only* TPM / TPD (we don't know the token cost yet)
//!
//! Phase 2 — **post-deduct**, called after the upstream response
//! completes:
//! - add actual `prompt_tokens + completion_tokens` to TPM / TPD
//! - release the concurrency slot
//!
//! The returned [`Reservation`] handle releases the concurrency slot on
//! drop if `commit_tokens` isn't called, so callers can't forget on the
//! error path.
//!
//! The counters live wherever the [`RateStore`] keeps them: the default
//! [`crate::store::local::LocalStore`] is per-process (historical
//! behaviour), while [`crate::store::redis::RedisStore`] shares them
//! across every DP replica so a cluster enforces one global window
//! (api7/#798).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aisix_core::RateLimit;

use crate::clock::Clock;
use crate::error::RateLimitError;
use crate::store::local::LocalStore;
use crate::store::RateStore;

/// Current window state for a single key, returned by [`Limiter::peek`].
/// Used by the proxy handlers to inject the `x-ratelimit-*` response
/// headers that OpenAI SDK clients expect.
#[derive(Debug, Clone)]
pub struct RateLimitStatus {
    pub rpm_limit: Option<u64>,
    pub rpm_used: u64,
    pub rpm_reset_secs: u64,
    pub tpm_limit: Option<u64>,
    pub tpm_used: u64,
    pub tpm_reset_secs: u64,
    pub concurrency_limit: Option<u32>,
    pub in_flight: u32,
}

impl RateLimitStatus {
    pub fn rpm_remaining(&self) -> Option<u64> {
        self.rpm_limit.map(|lim| lim.saturating_sub(self.rpm_used))
    }
    pub fn tpm_remaining(&self) -> Option<u64> {
        self.tpm_limit.map(|lim| lim.saturating_sub(self.tpm_used))
    }
}

/// Two-phase limiter over a shared or local [`RateStore`].
pub struct Limiter {
    store: Arc<dyn RateStore>,
    /// Process-unique reservation id prefix (`<uuid>:`), so concurrency
    /// members are globally distinct across replicas in the shared store.
    member_prefix: String,
    seq: AtomicU64,
}

impl Limiter {
    /// Default per-process limiter (in-memory `LocalStore`).
    pub fn new() -> Self {
        Self::with_store(Arc::new(LocalStore::new()))
    }

    /// Build over a specific store — the server bootstrap passes a
    /// `RedisStore` when a shared backend is configured.
    pub fn with_store(store: Arc<dyn RateStore>) -> Self {
        Self {
            store,
            member_prefix: format!("{}:", uuid::Uuid::new_v4().simple()),
            seq: AtomicU64::new(0),
        }
    }

    /// Test helper: a local store driven by an injectable clock.
    pub fn local_with_clock<C: Clock>(clock: C) -> Self {
        Self::with_store(Arc::new(LocalStore::with_clock(clock)))
    }

    fn next_member(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{}{n}", self.member_prefix)
    }

    /// Pre-commit phase. Returns a [`Reservation`] that must be finalised
    /// via [`Reservation::commit_tokens`] or dropped to release the
    /// concurrency slot automatically.
    pub async fn pre_commit(
        &self,
        key: &str,
        limits: &RateLimit,
    ) -> Result<Reservation, RateLimitError> {
        self.pre_commit_with_unit(key, limits, CounterUnit::Tokens)
            .await
    }

    /// Same as [`Self::pre_commit`], but names the unit this layer's
    /// counter is denominated in.
    pub async fn pre_commit_with_unit(
        &self,
        key: &str,
        limits: &RateLimit,
        unit: CounterUnit,
    ) -> Result<Reservation, RateLimitError> {
        let member = self.next_member();
        self.store.acquire(key, limits, &member).await?;
        let renewal = limits.concurrency.and_then(|_| {
            self.store
                .concurrency_lease_renewal_interval()
                .and_then(|interval| {
                    spawn_lease_renewal(
                        Arc::clone(&self.store),
                        key.to_string(),
                        member.clone(),
                        interval,
                    )
                })
        });
        Ok(Reservation {
            store: Arc::clone(&self.store),
            key: key.to_string(),
            member,
            renewal,
            committed: false,
            unit,
        })
    }

    /// Add `tokens` to the post-deduct TPM/TPD counters for `key` without
    /// going through a [`Reservation`]. Used by the streaming chat path:
    /// at pre_commit time the upstream token cost isn't known, so the
    /// concurrency slot is held by a [`StreamConcurrencyGuard`] and the
    /// tokens are accounted here when the terminal SSE usage frame lands
    /// (issue #108). No-op on zero tokens.
    pub fn add_tokens_post_stream(&self, key: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.store.add_tokens(key, tokens);
    }

    /// Credit every reservation layer in one batch. Prefer this over a loop
    /// of [`Limiter::add_tokens_post_stream`]: a distributed store waits
    /// once for the whole batch instead of once per layer, and this runs
    /// from the synchronous stream-completion callback on the serving
    /// thread. No-op on zero tokens or an empty key list.
    pub fn add_tokens_post_stream_all(&self, keys: &[String], tokens: u64) {
        if tokens == 0 || keys.is_empty() {
            return;
        }
        self.store.add_tokens_all(keys, tokens);
    }

    /// Reclaim per-key state idle for longer than `idle_for`. Called from
    /// the server's periodic upkeep task so a long-lived process does not
    /// hold buckets for api keys, models and policies that were deleted
    /// from the control plane hours ago.
    pub fn reap(&self, idle_for: std::time::Duration) {
        self.store.reap(idle_for);
    }

    /// Snapshot of the current rate-limit state for a key, used to inject
    /// `x-ratelimit-*` response headers. Returns `None` when there is
    /// nothing meaningful to report. Read-only — affects no counters.
    pub async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        self.store.peek(key, limits).await
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

/// 一个预留层计数的单位。
///
/// store 对此无感——`FixedWindowCounter` 不关心它数的是什么，token 和钱
/// 对它是同一种量。只有 `Reservation` 知道自己那个桶里的数字是什么单位，
/// 所以分派发生在这一层，而不是store 层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterUnit {
    /// 桶里计的是 token 数。
    Tokens,
    /// 桶里计的是 micro-USD（1 USD = 1_000_000）。
    MicroUsd,
}

/// Reservation guard. Dropping without a `commit_tokens` call is still
/// safe — the concurrency slot is released, just no tokens are counted.
pub struct Reservation {
    store: Arc<dyn RateStore>,
    key: String,
    member: String,
    renewal: Option<tokio::task::JoinHandle<()>>,
    committed: bool,
    /// 见 [`CounterUnit`]。默认 `Tokens`，与本字段引入前的行为一致。
    unit: CounterUnit,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("key", &self.key)
            .field("committed", &self.committed)
            .finish()
    }
}

impl Reservation {
    /// Post-deduct phase. Records the actual token cost against TPM/TPD
    /// and releases the concurrency slot.
    pub async fn commit_tokens(mut self, tokens: u64) {
        if let Some(task) = self.renewal.take() {
            task.abort();
        }
        self.store.commit(&self.key, tokens, &self.member).await;
        self.committed = true;
    }

    /// 该层应记的量：token 层记 token 数，花费层记 micro-USD。
    fn amount_for(&self, tokens: u64, spend_micro_usd: u64) -> u64 {
        match self.unit {
            CounterUnit::Tokens => tokens,
            CounterUnit::MicroUsd => spend_micro_usd,
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(task) = self.renewal.take() {
            task.abort();
        }
        self.store.release(&self.key, &self.member);
    }
}

/// Wraps multiple [`Reservation`]s across rate-limit layers (api_key,
/// model, team, member). Commits all with the same token count; dropping
/// releases all concurrency slots.
pub struct MultiReservation {
    reservations: Vec<Reservation>,
}

impl MultiReservation {
    pub fn new(reservations: Vec<Reservation>) -> Self {
        Self { reservations }
    }

    /// 提交实际用量，每层按自己的 [`CounterUnit`] 取对应的数字。
    pub async fn commit(self, tokens: u64, spend_micro_usd: u64) {
        for r in self.reservations {
            let amount = r.amount_for(tokens, spend_micro_usd);
            r.commit_tokens(amount).await;
        }
    }

    /// 等价于 `commit(tokens, 0)`——如果这个预留带花费层，花费桶记的是 0。
    /// 名字刻意长一点：调用它就是在断言"这个调用点没有花费数字要记"，
    /// 而不是顺手图省事漏记了钱。适用场景——命中缓存（没有真实上游花费）、
    /// 错误路径（没跑到能算出花费的地方）、以及任何本来就拿不到花费数字的
    /// 调用点。44 处既有调用点都属于这几类之一；新调用点如果**有**花费数字，
    /// 应该用 [`Self::commit`]，不要为了省事套用这个。
    pub async fn commit_tokens_no_spend(self, tokens: u64) {
        self.commit(tokens, 0).await;
    }

    /// 流结束后要记 token 数的桶键（[`CounterUnit::Tokens`] 那些层）。
    ///
    /// 刻意不提供「所有层的键」这一种取法：流式路径拿到键之后是用
    /// [`Limiter::add_tokens_post_stream_all`] 一次批量加同一个数字，
    /// 把两种单位的桶混在一张表里，就会把 token 数记进花费桶——数字看着
    /// 正常，量纲完全不对，且不会有任何报错。
    pub fn token_keys(&self) -> Vec<String> {
        self.keys_of_unit(CounterUnit::Tokens)
    }

    /// 流结束后要记 micro-USD 的桶键（[`CounterUnit::MicroUsd`] 那些层）。
    /// 与 [`Self::token_keys`] 成对使用，各记各的数字。
    pub fn spend_keys(&self) -> Vec<String> {
        self.keys_of_unit(CounterUnit::MicroUsd)
    }

    fn keys_of_unit(&self, unit: CounterUnit) -> Vec<String> {
        self.reservations
            .iter()
            .filter(|r| r.unit == unit)
            .map(|r| r.key.clone())
            .collect()
    }

    /// Absorb another reservation's layers into this one, so a single
    /// `commit_tokens_no_spend` / `into_stream_hold` finalises both. Used by the
    /// routing dispatch to fold the winning target's model-layer
    /// reservation into the request-level reservation once the winner
    /// is known.
    pub fn merge(&mut self, other: MultiReservation) {
        self.reservations.extend(other.reservations);
    }

    /// Convert into an owned [`StreamConcurrencyGuard`] for the streaming
    /// path. The per-layer concurrency slots stay held — they are NOT
    /// released here — and are released only when the returned guard drops,
    /// i.e. at stream completion or cancellation. Token accounting still
    /// happens via [`Limiter::add_tokens_post_stream`].
    ///
    /// A borrow-based reservation couldn't outlive the request handler, so
    /// the pre-fix streaming path dropped it at handler return; that
    /// released the slot before the stream finished, letting a key capped
    /// at N run many more than N simultaneous streams (#450).
    #[must_use = "dropping the returned guard immediately releases the concurrency \
                  slot, recreating the early-release bug this fixes"]
    pub fn into_stream_hold(mut self) -> StreamConcurrencyGuard {
        let mut renewal_tasks = Vec::new();
        let holds = self
            .reservations
            .iter_mut()
            .map(|r| {
                // Defuse each reservation's Drop so it doesn't release the
                // slot now; the returned guard owns release from here on.
                r.committed = true;
                if let Some(task) = r.renewal.take() {
                    renewal_tasks.push(task);
                }
                (Arc::clone(&r.store), r.key.clone(), r.member.clone())
            })
            .collect();
        StreamConcurrencyGuard {
            holds,
            renewal_tasks,
            released: false,
        }
    }
}

impl std::fmt::Debug for MultiReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiReservation")
            .field("layers", &self.reservations.len())
            .finish()
    }
}

/// Owned concurrency hold for the streaming path. Releases the
/// concurrency slot(s) on drop — i.e. when the stream completes or is
/// cancelled — instead of at handler return. See
/// [`MultiReservation::into_stream_hold`].
pub struct StreamConcurrencyGuard {
    /// `(store, key, member)` per held layer.
    holds: Vec<(Arc<dyn RateStore>, String, String)>,
    renewal_tasks: Vec<tokio::task::JoinHandle<()>>,
    released: bool,
}

impl StreamConcurrencyGuard {
    fn release_now(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        for task in self.renewal_tasks.drain(..) {
            task.abort();
        }
        for (store, key, member) in &self.holds {
            store.release(key, member);
        }
    }
}

impl std::fmt::Debug for StreamConcurrencyGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamConcurrencyGuard")
            .field("layers", &self.holds.len())
            .field("renewal_tasks", &self.renewal_tasks.len())
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for StreamConcurrencyGuard {
    fn drop(&mut self) {
        self.release_now();
    }
}

fn spawn_lease_renewal(
    store: Arc<dyn RateStore>,
    key: String,
    member: String,
    interval: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    Some(runtime.spawn(async move {
        let start = tokio::time::Instant::now() + interval;
        let mut ticker = tokio::time::interval_at(start, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            store.renew_concurrency_lease(&key, &member).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use async_trait::async_trait;
    use std::time::Duration;

    #[derive(Default)]
    struct RenewingStore {
        renewals: AtomicU64,
    }

    #[async_trait]
    impl RateStore for RenewingStore {
        async fn acquire(
            &self,
            _key: &str,
            _limits: &RateLimit,
            _member: &str,
        ) -> Result<(), RateLimitError> {
            Ok(())
        }

        async fn commit(&self, _key: &str, _tokens: u64, _member: &str) {}

        fn concurrency_lease_renewal_interval(&self) -> Option<Duration> {
            Some(Duration::from_millis(10))
        }

        async fn renew_concurrency_lease(&self, _key: &str, _member: &str) {
            self.renewals.fetch_add(1, Ordering::Relaxed);
        }

        fn release(&self, _key: &str, _member: &str) {}

        fn add_tokens(&self, _key: &str, _tokens: u64) {}

        async fn peek(&self, _key: &str, _limits: &RateLimit) -> Option<RateLimitStatus> {
            None
        }
    }

    fn limits(rpm: Option<u64>, tpm: Option<u64>, concurrency: Option<u32>) -> RateLimit {
        RateLimit {
            rps: None,
            rpm,
            rph: None,
            rpd: None,
            tpm,
            tph: None,
            tpd: None,
            concurrency,
        }
    }

    /// Helper for the rps/rph/compensator tests added by #426.
    fn limits_full(
        rps: Option<u64>,
        rpm: Option<u64>,
        rph: Option<u64>,
        rpd: Option<u64>,
    ) -> RateLimit {
        RateLimit {
            rps,
            rpm,
            rph,
            rpd,
            tpm: None,
            tph: None,
            tpd: None,
            concurrency: None,
        }
    }

    #[tokio::test]
    async fn rpm_caps_request_count_in_window() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(2), None, None);

        let _r1 = limiter.pre_commit("k1", &l).await.unwrap();
        let _r2 = limiter.pre_commit("k1", &l).await.unwrap();
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        match err {
            RateLimitError::Requests {
                retry_after_secs, ..
            } => {
                assert!(retry_after_secs > 0);
            }
            other => panic!("expected Requests, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rpm_resets_after_window_rollover() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(1), None, None);

        let _r1 = limiter.pre_commit("k1", &l).await.unwrap();
        assert!(limiter.pre_commit("k1", &l).await.is_err());

        // Jump past the minute boundary.
        clock.advance(61);
        let _r2 = limiter.pre_commit("k1", &l).await.unwrap();
    }

    #[tokio::test]
    async fn concurrency_limit_blocks_new_reservations() {
        let clock = TestClock::new(0);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(None, None, Some(2));

        let r1 = limiter.pre_commit("k1", &l).await.unwrap();
        let r2 = limiter.pre_commit("k1", &l).await.unwrap();
        assert!(matches!(
            limiter.pre_commit("k1", &l).await.unwrap_err(),
            RateLimitError::Concurrency,
        ));

        // Drop r1 — concurrency should free up.
        drop(r1);
        let _r3 = limiter.pre_commit("k1", &l).await.unwrap();
        drop(r2);
    }

    #[tokio::test]
    async fn stream_guard_renews_distributed_concurrency_lease() {
        let store = Arc::new(RenewingStore::default());
        let limiter = Limiter::with_store(store.clone());
        let reservation = limiter
            .pre_commit("k1", &limits(None, None, Some(1)))
            .await
            .unwrap();
        let guard = MultiReservation::new(vec![reservation]).into_stream_hold();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            store.renewals.load(Ordering::Relaxed) > 0,
            "an active stream must renew before its distributed lease expires",
        );

        drop(guard);
        let renewals_after_drop = store.renewals.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            store.renewals.load(Ordering::Relaxed),
            renewals_after_drop,
            "dropping the stream must stop its renewal task",
        );
    }

    #[tokio::test]
    async fn token_commit_updates_post_deduct_counters() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(10), Some(1_000), None);

        let r1 = limiter.pre_commit("k1", &l).await.unwrap();
        r1.commit_tokens(600).await;

        // TPM now at 600. Next pre_commit with a strict TPM should still
        // succeed because 600 <= 1000.
        let _r2 = limiter.pre_commit("k1", &l).await.unwrap();
    }

    #[tokio::test]
    async fn tpm_blocks_next_request_once_previous_exhausted_the_window() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(10), Some(1_000), None);

        let r1 = limiter.pre_commit("k1", &l).await.unwrap();
        r1.commit_tokens(1_500).await; // overshoot — allowed for the in-flight request

        // Next pre_commit sees tpm > 1000 and refuses.
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(matches!(err, RateLimitError::Tokens { .. }));

        clock.advance(61); // roll the window
        let _r2 = limiter.pre_commit("k1", &l).await.unwrap();
    }

    #[tokio::test]
    async fn tpm_blocks_next_request_at_the_exact_limit() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(Some(10), Some(1_000), None);

        limiter
            .pre_commit("k1", &l)
            .await
            .unwrap()
            .commit_tokens(1_000)
            .await;

        assert!(matches!(
            limiter.pre_commit("k1", &l).await.unwrap_err(),
            RateLimitError::Tokens { .. },
        ));
    }

    #[tokio::test]
    async fn reservations_for_different_keys_do_not_collide() {
        let clock = TestClock::new(0);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(Some(1), None, None);

        let _r_a = limiter.pre_commit("alpha", &l).await.unwrap();
        let _r_b = limiter.pre_commit("beta", &l).await.unwrap();
    }

    #[tokio::test]
    async fn drop_without_commit_still_releases_concurrency_permit() {
        let clock = TestClock::new(0);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(None, None, Some(1));

        {
            let _r = limiter.pre_commit("k1", &l).await.unwrap();
        } // dropped
        let _r2 = limiter.pre_commit("k1", &l).await.unwrap();
    }

    #[tokio::test]
    async fn peek_returns_none_for_unknown_key() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        assert!(limiter
            .peek("unknown", &RateLimit::default())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn peek_reports_current_window_counts() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(60), Some(100_000), Some(10));

        let r = limiter.pre_commit("k1", &l).await.unwrap();
        r.commit_tokens(500).await;

        let status = limiter.peek("k1", &l).await.unwrap();
        assert_eq!(status.rpm_limit, Some(60));
        assert_eq!(status.rpm_used, 1);
        assert_eq!(status.rpm_remaining(), Some(59));
        assert_eq!(status.tpm_limit, Some(100_000));
        assert_eq!(status.tpm_used, 500);
        assert_eq!(status.tpm_remaining(), Some(99_500));
        assert_eq!(status.in_flight, 0); // committed → released
    }

    #[tokio::test]
    async fn peek_reflects_in_flight_count_during_dispatch() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(None, None, Some(5));

        let _r1 = limiter.pre_commit("k1", &l).await.unwrap();
        let _r2 = limiter.pre_commit("k1", &l).await.unwrap();
        let status = limiter.peek("k1", &l).await.unwrap();
        assert_eq!(status.in_flight, 2);
        assert_eq!(status.concurrency_limit, Some(5));
    }

    #[tokio::test]
    async fn no_limits_means_no_rejections() {
        let clock = TestClock::new(0);
        let limiter = Limiter::local_with_clock(clock);
        let l = RateLimit::default();

        for _ in 0..100 {
            let r = limiter.pre_commit("k1", &l).await.unwrap();
            r.commit_tokens(1_000).await;
        }
    }

    // ---- regression coverage for issue #109 -------------------------
    // The previous compensation path overwrote `s.rpm` with a fresh
    // counter, wiping concurrent siblings' increments. The fix replaces
    // the reset with a precise -1 decrement; these tests pin both the
    // "siblings are preserved" and "fresh window is not granted"
    // properties at the level the exploit happens.

    #[tokio::test]
    async fn rpd_rejection_does_not_grant_fresh_rpm_window() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = RateLimit {
            rps: None,
            rpm: Some(10),
            rph: None,
            rpd: Some(20),
            tpm: None,
            tph: None,
            tpd: None,
            concurrency: None,
        };
        // Soak up 19 RPM = 19 RPD across two minutes so RPD is at 19.
        for i in 0..19 {
            if i == 10 {
                clock.advance(61); // roll RPM, keep RPD
            }
            let _r = limiter.pre_commit("k1", &l).await.unwrap();
        }
        // RPM in current minute = 9 (after rollover), RPD = 19. One more
        // goes through (RPM 10/10, RPD 20/20).
        let _r = limiter.pre_commit("k1", &l).await.unwrap();
        // The 21st request must fail — RPD is full.
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(
            matches!(err, RateLimitError::Requests { .. }),
            "expected RPD rejection, got {err:?}"
        );
        // The next request must STILL fail RPM — proving RPM wasn't wiped.
        let err2 = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(
            matches!(err2, RateLimitError::Requests { .. }),
            "RPM should still be capped after RPD rejection; got {err2:?}"
        );
        let status = limiter.peek("k1", &l).await.unwrap();
        assert_eq!(status.rpm_used, 10, "RPM should not have been reset");
    }

    #[tokio::test]
    async fn rpd_rejection_preserves_concurrent_rpm_increments() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = RateLimit {
            rps: None,
            rpm: Some(100), // very high — RPM never trips here
            rph: None,
            rpd: Some(5),
            tpm: None,
            tph: None,
            tpd: None,
            concurrency: None,
        };
        for _ in 0..5 {
            let _r = limiter.pre_commit("k1", &l).await.unwrap();
        }
        // RPM=5, RPD=5/5. Sixth request fails RPD.
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(matches!(err, RateLimitError::Requests { .. }));
        let status = limiter.peek("k1", &l).await.unwrap();
        assert_eq!(
            status.rpm_used, 5,
            "rpd rejection wiped concurrent rpm increments"
        );
    }

    // ---- regression coverage for issue #108 -------------------------

    #[tokio::test]
    async fn add_tokens_post_stream_increments_tpm() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(Some(10), Some(1_000), None);

        {
            let _r = limiter.pre_commit("k1", &l).await.unwrap();
        }
        assert_eq!(
            limiter.peek("k1", &l).await.unwrap().tpm_used,
            0,
            "TPM should be 0 right after pre_commit + drop",
        );

        limiter.add_tokens_post_stream("k1", 750);
        assert_eq!(
            limiter.peek("k1", &l).await.unwrap().tpm_used,
            750,
            "TPM should reflect the post-stream commit",
        );
    }

    #[tokio::test]
    async fn add_tokens_post_stream_all_credits_every_layer() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(Some(10), Some(1_000), None);
        for key in ["key-layer", "model-layer", "policy-layer"] {
            let _r = limiter.pre_commit(key, &l).await.unwrap();
        }

        limiter.add_tokens_post_stream_all(
            &[
                "key-layer".to_string(),
                "model-layer".to_string(),
                "policy-layer".to_string(),
            ],
            750,
        );

        for key in ["key-layer", "model-layer", "policy-layer"] {
            assert_eq!(
                limiter.peek(key, &l).await.unwrap().tpm_used,
                750,
                "{key} must be credited by the batched post-stream commit"
            );
        }
    }

    #[tokio::test]
    async fn reap_reclaims_idle_state_through_the_limiter() {
        let clock = TestClock::new(1_000);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(10), Some(1_000), None);
        {
            let _r = limiter.pre_commit("gone-from-config", &l).await.unwrap();
        }
        assert!(limiter.peek("gone-from-config", &l).await.is_some());

        clock.advance(24 * 60 * 60 + 1);
        limiter.reap(std::time::Duration::from_secs(24 * 60 * 60));

        assert!(
            limiter.peek("gone-from-config", &l).await.is_none(),
            "state for a deleted config row must not survive the process"
        );
    }

    #[tokio::test]
    async fn add_tokens_post_stream_zero_is_a_noop() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        limiter.add_tokens_post_stream("never-seen", 0);
        assert!(
            limiter
                .peek("never-seen", &RateLimit::default())
                .await
                .is_none(),
            "add_tokens_post_stream(0) should not lazily-create state",
        );
    }

    #[tokio::test]
    async fn streaming_path_tpm_cap_blocks_next_request_after_post_stream_commit() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock);
        let l = limits(Some(100), Some(1_000), None);

        {
            let _r = limiter.pre_commit("k1", &l).await.unwrap();
        }
        limiter.add_tokens_post_stream("k1", 1_500);

        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(
            matches!(err, RateLimitError::Tokens { .. }),
            "TPM cap should block the next request after streaming over-shoot; got {err:?}",
        );
    }

    // --- MultiReservation tests ----------------------------------------

    #[tokio::test]
    async fn multi_reservation_commit_tokens_no_spend_updates_all_layers() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(None, Some(1000), None);

        let r1 = limiter.pre_commit("api_key:k1", &l).await.unwrap();
        let r2 = limiter.pre_commit("model:gpt-4o", &l).await.unwrap();
        let multi = MultiReservation::new(vec![r1, r2]);

        multi.commit_tokens_no_spend(500).await;

        let s1 = limiter.peek("api_key:k1", &l).await.unwrap();
        let s2 = limiter.peek("model:gpt-4o", &l).await.unwrap();
        assert_eq!(s1.tpm_used, 500);
        assert_eq!(s2.tpm_used, 500);
    }

    #[tokio::test]
    async fn multi_reservation_drop_releases_all_concurrency() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(None, None, Some(1));

        let r1 = limiter.pre_commit("k1", &l).await.unwrap();
        let r2 = limiter.pre_commit("k2", &l).await.unwrap();
        let multi = MultiReservation::new(vec![r1, r2]);

        assert!(limiter.pre_commit("k1", &l).await.is_err());
        assert!(limiter.pre_commit("k2", &l).await.is_err());

        drop(multi);

        assert!(limiter.pre_commit("k1", &l).await.is_ok());
        assert!(limiter.pre_commit("k2", &l).await.is_ok());
    }

    #[tokio::test]
    async fn stream_hold_keeps_concurrency_until_guard_drop() {
        // #450: a streaming request must keep its concurrency slot for the
        // stream's full lifetime, not release it at handler return.
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(None, None, Some(1));

        let r = limiter.pre_commit("k", &l).await.unwrap();
        let hold = MultiReservation::new(vec![r]).into_stream_hold();

        // Slot still held while the stream runs — a second concurrent
        // request is rejected.
        assert!(matches!(
            limiter.pre_commit("k", &l).await.unwrap_err(),
            RateLimitError::Concurrency
        ));

        // Stream completes/cancels → guard drops → slot released.
        drop(hold);
        assert!(limiter.pre_commit("k", &l).await.is_ok());
    }

    #[tokio::test]
    async fn multi_reservation_keys_are_split_by_counter_unit() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(Some(10), None, None);

        let r1 = limiter.pre_commit("api_key:k1", &l).await.unwrap();
        let r2 = limiter.pre_commit("model:m1", &l).await.unwrap();
        let r3 = limiter
            .pre_commit_with_unit("spend:api_key:k1", &l, CounterUnit::MicroUsd)
            .await
            .unwrap();
        let multi = MultiReservation::new(vec![r1, r2, r3]);

        // 流式路径按单位分两批记账。混在一起会把 token 数记进花费桶。
        assert_eq!(multi.token_keys(), vec!["api_key:k1", "model:m1"]);
        assert_eq!(multi.spend_keys(), vec!["spend:api_key:k1"]);
    }

    #[tokio::test]
    async fn multi_reservation_merge_commits_and_releases_absorbed_layers() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits(None, Some(1000), Some(1));

        let main = limiter.pre_commit("api_key:k1", &l).await.unwrap();
        let member = limiter.pre_commit("model:target", &l).await.unwrap();

        let mut multi = MultiReservation::new(vec![main]);
        multi.merge(MultiReservation::new(vec![member]));
        assert_eq!(multi.token_keys(), vec!["api_key:k1", "model:target"]);

        // One commit finalises both layers: tokens land on each and the
        // absorbed layer's concurrency slot is released.
        multi.commit_tokens_no_spend(300).await;
        let s = limiter.peek("model:target", &l).await.unwrap();
        assert_eq!(s.tpm_used, 300);
        assert!(limiter.pre_commit("model:target", &l).await.is_ok());
    }

    #[tokio::test]
    async fn multi_reservation_partial_failure_releases_acquired_layers() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l_key = limits(None, None, Some(1));
        let l_team = limits(None, None, Some(1));
        let l_model = limits(Some(1), None, None);

        // Exhaust model RPM so the third layer will fail.
        let _exhaust = limiter.pre_commit("model:m1", &l_model).await.unwrap();

        let r_key = limiter.pre_commit("k1", &l_key).await.unwrap();
        let r_team = limiter.pre_commit("team:t1", &l_team).await.unwrap();
        let acquired = vec![r_key, r_team];

        assert!(limiter.pre_commit("k1", &l_key).await.is_err());
        assert!(limiter.pre_commit("team:t1", &l_team).await.is_err());

        // Model layer fails — drop the partially-built reservations.
        assert!(limiter.pre_commit("model:m1", &l_model).await.is_err());
        drop(MultiReservation::new(acquired));

        assert!(limiter.pre_commit("k1", &l_key).await.is_ok());
        assert!(limiter.pre_commit("team:t1", &l_team).await.is_ok());
    }

    // ───────────────────────── #426 rps / rph coverage ─────────────────────────

    #[tokio::test]
    async fn rps_caps_request_count_within_one_second() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits_full(Some(5), None, None, None);

        for i in 0..5 {
            limiter
                .pre_commit("k1", &l)
                .await
                .unwrap_or_else(|e| panic!("request {i}: {e:?}"));
        }
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(
            matches!(err, RateLimitError::Requests { .. }),
            "expected rps rejection, got {err:?}"
        );
    }

    #[tokio::test]
    async fn rps_window_rolls_at_one_second_boundary() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits_full(Some(3), None, None, None);

        for _ in 0..3 {
            limiter.pre_commit("k1", &l).await.unwrap();
        }
        assert!(limiter.pre_commit("k1", &l).await.is_err());

        clock.advance(1);
        for _ in 0..3 {
            limiter.pre_commit("k1", &l).await.unwrap();
        }
        assert!(limiter.pre_commit("k1", &l).await.is_err());
    }

    #[tokio::test]
    async fn rph_caps_request_count_within_one_hour() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits_full(None, None, Some(10), None);

        for i in 0..10 {
            limiter
                .pre_commit("k1", &l)
                .await
                .unwrap_or_else(|e| panic!("request {i}: {e:?}"));
        }
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(
            matches!(err, RateLimitError::Requests { .. }),
            "expected rph rejection, got {err:?}"
        );

        clock.advance(3601);
        limiter.pre_commit("k1", &l).await.unwrap();
    }

    #[tokio::test]
    async fn rpd_rejection_rolls_back_rps_and_rph_increments() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits_full(Some(1000), Some(1000), Some(1000), Some(2));

        limiter.pre_commit("k1", &l).await.unwrap();
        limiter.pre_commit("k1", &l).await.unwrap();
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(matches!(err, RateLimitError::Requests { .. }));

        let status = limiter.peek("k1", &l).await.unwrap();
        assert_eq!(
            status.rpm_used, 2,
            "rpd rejection must roll back rpm by exactly 1, leaving the two earlier accepts"
        );
    }

    #[tokio::test]
    async fn rph_rejection_rolls_back_rps_and_rpm_increments() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits_full(Some(1000), Some(1000), Some(2), None);

        limiter.pre_commit("k1", &l).await.unwrap();
        limiter.pre_commit("k1", &l).await.unwrap();
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(matches!(err, RateLimitError::Requests { .. }));
        let status = limiter.peek("k1", &l).await.unwrap();
        assert_eq!(
            status.rpm_used, 2,
            "rph rejection must roll back rpm by exactly 1, leaving the two earlier accepts"
        );
    }

    #[tokio::test]
    async fn rps_layer_disabled_when_field_unset() {
        let clock = TestClock::new(100);
        let limiter = Limiter::local_with_clock(clock.clone());
        let l = limits_full(None, Some(5), None, None);

        for _ in 0..5 {
            limiter.pre_commit("k1", &l).await.unwrap();
        }
        let err = limiter.pre_commit("k1", &l).await.unwrap_err();
        assert!(matches!(err, RateLimitError::Requests { .. }));
    }

    // --- CounterUnit dispatch tests --------------------------------------

    /// 两个层各自计不同的量：token 层收 token 数，花费层收 micro-USD。
    /// 把两者搞混不会让任何请求失败——只会让预算按 token 数扣钱，
    /// 或让 token 窗口按分币计数，两种都静默。
    #[tokio::test]
    async fn commit_dispatches_each_layer_by_its_unit() {
        let store = Arc::new(LocalStore::new());
        let limiter = Limiter::with_store(Arc::clone(&store) as Arc<dyn RateStore>);
        let limits = RateLimit {
            tpd: Some(1_000_000),
            ..RateLimit::default()
        };

        let tok = limiter
            .pre_commit_with_unit("tok-layer", &limits, CounterUnit::Tokens)
            .await
            .expect("token layer reserves");
        let spend = limiter
            .pre_commit_with_unit("spend-layer", &limits, CounterUnit::MicroUsd)
            .await
            .expect("spend layer reserves");

        MultiReservation::new(vec![tok, spend])
            .commit(150, 4_200)
            .await;

        assert_eq!(
            store.committed_tokens("tok-layer"),
            150,
            "token 层应收到 token 数"
        );
        assert_eq!(
            store.committed_tokens("spend-layer"),
            4_200,
            "花费层应收到 micro-USD"
        );
    }

    /// 旧入口保持原语义：全部按 token 层处理，花费为 0。
    /// 44 处既有调用点依赖这一点。
    #[tokio::test]
    async fn commit_tokens_no_spend_still_treats_every_layer_as_tokens() {
        let store = Arc::new(LocalStore::new());
        let limiter = Limiter::with_store(Arc::clone(&store) as Arc<dyn RateStore>);
        let limits = RateLimit {
            tpd: Some(1_000),
            ..RateLimit::default()
        };
        let r = limiter
            .pre_commit("legacy", &limits)
            .await
            .expect("reserves");
        MultiReservation::new(vec![r])
            .commit_tokens_no_spend(77)
            .await;
        assert_eq!(store.committed_tokens("legacy"), 77);
    }
}
