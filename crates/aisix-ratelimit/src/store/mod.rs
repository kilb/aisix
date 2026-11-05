//! Pluggable counter backend behind the [`crate::Limiter`].
//!
//! The limiter itself only knows about *buckets* (an opaque key) and a
//! [`RateLimit`]; where the counters actually live is a [`RateStore`].
//!
//! - [`local::LocalStore`] keeps the historical per-process in-memory
//!   counters (a `DashMap` of fixed-window counters). This is the default
//!   and is behaviour-identical to the pre-#798 limiter.
//! - [`redis::RedisStore`] keeps the counters in a shared Redis so every
//!   DP replica in a cluster enforces ONE global window — the fix for
//!   api7/#798, where N replicas multiplied every limit by N.
//!
//! Two phases mirror the limiter's contract:
//! - **acquire** (request path, async): concurrency gate + token
//!   check-only + request check-and-increment, all-or-nothing per bucket.
//! - **commit** (request path success, async): post-deduct token add +
//!   concurrency release.
//! - **release** / **add_tokens** (after-the-fact, sync): concurrency
//!   release on drop, and the streaming post-stream token add. These are
//!   sync because they run from `Drop` and from the synchronous SSE
//!   completion callback, both of which run on the serving thread.
//!
//! Both after-the-fact operations are **best-effort and never block**.
//! Token accounting is post-hoc billing: it is decoupled from the request so
//! a slow or half-open Redis costs counter accuracy, never latency. The
//! serving thread is the whole core's event loop under thread-per-core, so a
//! wait there would stall every other in-flight request on that core, not
//! just the stream that ended. A distributed store therefore hands the update
//! to its own I/O worker and returns; the shared counters converge shortly
//! after, not synchronously.

use std::time::Duration;

use aisix_core::RateLimit;
use async_trait::async_trait;

use crate::error::RateLimitError;
use crate::limiter::RateLimitStatus;

pub mod local;
pub mod redis;

pub(crate) const SECOND_SECS: u64 = 1;
pub(crate) const MINUTE_SECS: u64 = 60;
pub(crate) const HOUR_SECS: u64 = 60 * 60;
pub(crate) const DAY_SECS: u64 = 24 * 60 * 60;

/// Dimension names. Both stores name the same window identically —
/// they are the Redis key segment AND the `x-ratelimit-scope` value a
/// 429 reports, so a client cannot be told `rpm` by one backend and
/// `RPM` by the other.
pub(crate) const DIM_RPS: &str = "rps";
pub(crate) const DIM_RPM: &str = "rpm";
pub(crate) const DIM_RPH: &str = "rph";
pub(crate) const DIM_RPD: &str = "rpd";
pub(crate) const DIM_TPM: &str = "tpm";
pub(crate) const DIM_TPH: &str = "tph";
pub(crate) const DIM_TPD: &str = "tpd";

/// Result of trying to renew a distributed streaming concurrency lease.
///
/// Only [`StreamLeaseRefresh::Missing`] is definitive: the member was no
/// longer present in the shared semaphore. The passthrough SSE relay uses
/// that signal to end an already-headed response rather than continue outside
/// its configured concurrency limit. `Unavailable` includes backends that
/// cannot report an outcome and Redis transport failures; those retain the
/// rate limiter's established fail-open behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLeaseRefresh {
    Renewed,
    Missing,
    Unavailable,
}

/// A windowed request/token dimension active on a [`RateLimit`]:
/// `(name, window_secs, limit)`. Shared by both stores so the Redis key
/// layout and the local counter set never drift.
pub(crate) struct Dim {
    pub name: &'static str,
    pub window_secs: u64,
    pub limit: u64,
}

/// Request-count dimensions (rps/rpm/rph/rpd) that carry a limit.
pub(crate) fn request_dims(limits: &RateLimit) -> Vec<Dim> {
    [
        (DIM_RPS, SECOND_SECS, limits.rps),
        (DIM_RPM, MINUTE_SECS, limits.rpm),
        (DIM_RPH, HOUR_SECS, limits.rph),
        (DIM_RPD, DAY_SECS, limits.rpd),
    ]
    .into_iter()
    .filter_map(|(name, window_secs, limit)| {
        limit.map(|limit| Dim {
            name,
            window_secs,
            limit,
        })
    })
    .collect()
}

/// Token-count dimensions (tpm/tph/tpd) that carry a limit.
///
/// There is deliberately no `tps`. Token counts are only known once the
/// upstream has answered, so every token window is charged after the fact; at
/// a 1-second window essentially every request commits after the bucket it was
/// admitted against has rolled, which makes a per-second token cap lag by
/// about its own width. A cap that looks enforced and is not is worse than an
/// absent one, so the sub-minute case stays refused loudly (see
/// `quota::warn_inert_dimension_once`) rather than half-honoured.
pub(crate) fn token_dims(limits: &RateLimit) -> Vec<Dim> {
    [
        (DIM_TPM, MINUTE_SECS, limits.tpm),
        (DIM_TPH, HOUR_SECS, limits.tph),
        (DIM_TPD, DAY_SECS, limits.tpd),
    ]
    .into_iter()
    .filter_map(|(name, window_secs, limit)| {
        limit.map(|limit| Dim {
            name,
            window_secs,
            limit,
        })
    })
    .collect()
}

/// Backend that holds the rate-limit counters for a bucket.
///
/// `member` is a process-unique reservation id (`<instance>:<seq>`) used
/// by distributed backends to track exactly one in-flight slot in the
/// concurrency set; the local backend ignores it (its `in_flight` is a
/// plain counter).
#[async_trait]
pub trait RateStore: Send + Sync + 'static {
    /// Pre-commit acquire for a single bucket. Atomically (per bucket):
    /// gate concurrency, check (but do not increment) token windows, then
    /// check-and-increment every request window. All-or-nothing: on
    /// rejection nothing is incremented and the concurrency slot is not
    /// taken.
    async fn acquire(
        &self,
        key: &str,
        limits: &RateLimit,
        member: &str,
    ) -> Result<(), RateLimitError>;

    /// Post-deduct: add `tokens` to the tpm/tpd windows AND release the
    /// concurrency slot held by `member`. Like the local backend this
    /// always touches both token windows; the tpd counter is harmless
    /// when no tpd limit is configured (it simply expires unread).
    async fn commit(&self, key: &str, tokens: u64, member: &str);

    /// Release the concurrency slot held by `member` without recording
    /// tokens. Sync so it can run from `Drop`; the Redis impl spawns a
    /// detached release (the concurrency set self-heals via TTL pruning
    /// even if the spawn is lost).
    fn release(&self, key: &str, member: &str);

    /// Post-stream token accounting: add `tokens` to tpm/tpd only (no
    /// concurrency change).
    ///
    /// Sync so it can run from the synchronous SSE completion callback, and
    /// **must return without waiting on I/O**. A distributed store queues the
    /// update for its own worker, so a subsequent acquire — on this replica or
    /// another — converges shortly after rather than immediately.
    fn add_tokens(&self, key: &str, tokens: u64);

    /// Post-stream token accounting for every reservation layer at once.
    ///
    /// A distributed store queues every layer's write and returns; it must
    /// not wait, per [`RateStore::add_tokens`]. The default is the per-key
    /// loop, which is correct for a local store because it does no I/O.
    fn add_tokens_all(&self, keys: &[String], tokens: u64) {
        for key in keys {
            self.add_tokens(key, tokens);
        }
    }

    /// Drop per-key state untouched for longer than `idle_for`.
    ///
    /// Bucket keys are config-derived (api key, model, policy), so a
    /// long-lived process otherwise accumulates state for rows the control
    /// plane deleted and only a restart reclaims it. Called from the
    /// server's periodic upkeep task. Default is a no-op for stores that
    /// hold no in-process state.
    fn reap(&self, _idle_for: std::time::Duration) {}

    /// How often a live streaming reservation should renew its distributed
    /// concurrency lease. Local counters have no lease, so they opt out.
    fn stream_lease_refresh_interval(&self) -> Option<Duration> {
        None
    }

    /// Refresh a live streaming reservation's distributed concurrency lease.
    /// Implementations must never recreate a member that has already been
    /// released: a final refresh racing with stream teardown must be a no-op.
    async fn refresh_stream_lease(&self, _key: &str, _member: &str) {}

    /// Refresh a streaming lease while reporting whether its member still
    /// exists. This preserves the legacy [`RateStore::refresh_stream_lease`]
    /// hook for external stores: an implementation that only provides that
    /// hook remains fail-open because it cannot prove that its member vanished.
    async fn refresh_stream_lease_outcome(&self, key: &str, member: &str) -> StreamLeaseRefresh {
        self.refresh_stream_lease(key, member).await;
        StreamLeaseRefresh::Unavailable
    }

    /// Read-only snapshot for the `x-ratelimit-*` headers. Returns `None`
    /// when there is nothing meaningful to report for the bucket.
    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus>;

    /// Claim a one-shot marker, `true` for the caller that got it.
    ///
    /// Lives in this store, so it lasts exactly as long as the counters do:
    /// a shared store holds the claim across restarts and across instances,
    /// the in-process store only for one process's lifetime. That pairing is
    /// the point — it guards side effects on the counters in this same
    /// store, so a claim can never outlive or fall short of what it protects.
    ///
    /// For work that may legitimately be redone but whose accounting must
    /// not repeat. The after-the-fact spend attribution of a completed batch
    /// is the case: its usage event carries a deterministic id the consumer
    /// dedups on, but crediting a monotonic spend counter and incrementing a
    /// Prometheus counter are not idempotent, and the attribution replays
    /// whenever a process that has forgotten the batch sees it completed
    /// again.
    async fn claim_once(&self, key: &str, ttl: Duration) -> bool;

    /// Keys this process holds counters for; `None` for a shared store
    /// whose counters live elsewhere.
    fn local_key_count(&self) -> Option<u64> {
        None
    }
}
