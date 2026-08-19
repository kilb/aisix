//! In-process counter store — the historical, default backend.
//!
//! Behaviour-identical to the pre-#798 limiter: a `DashMap` of per-key
//! fixed-window counters guarded by one `parking_lot::Mutex` each. State
//! is per-replica and not shared, so a multi-replica cluster multiplies
//! every limit by the replica count — exactly what [`super::redis`]
//! exists to fix. `member` is ignored here (concurrency is a plain
//! `in_flight` counter).

use aisix_core::{RateLimit, RateLimitScope};
use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::Mutex;
use std::sync::Arc;

use super::{RateStore, DAY_SECS, HOUR_SECS, MINUTE_SECS, SECOND_SECS};
use crate::clock::{Clock, SystemClock};
use crate::error::RateLimitError;
use crate::limiter::RateLimitStatus;
use crate::window::{FixedWindowCounter, WindowCheck};

/// Per-key state guarded by a single mutex. Hot path locks once per
/// request; each operation inside is O(1).
#[derive(Debug)]
struct KeyState {
    rps: FixedWindowCounter,
    rpm: FixedWindowCounter,
    rph: FixedWindowCounter,
    rpd: FixedWindowCounter,
    tpm: FixedWindowCounter,
    tph: FixedWindowCounter,
    tpd: FixedWindowCounter,
    in_flight: u32,
    /// Unix seconds of the last operation on this bucket, so [`LocalStore::reap`]
    /// can tell a live key from one whose config row was deleted hours ago.
    last_touched: u64,
}

impl KeyState {
    fn new(now: u64) -> Self {
        Self {
            rps: FixedWindowCounter::new(SECOND_SECS),
            rpm: FixedWindowCounter::new(MINUTE_SECS),
            rph: FixedWindowCounter::new(HOUR_SECS),
            rpd: FixedWindowCounter::new(DAY_SECS),
            tpm: FixedWindowCounter::new(MINUTE_SECS),
            tph: FixedWindowCounter::new(HOUR_SECS),
            tpd: FixedWindowCounter::new(DAY_SECS),
            in_flight: 0,
            last_touched: now,
        }
    }
}

/// Per-process fixed-window store.
pub struct LocalStore<C: Clock = SystemClock> {
    states: DashMap<String, Arc<Mutex<KeyState>>>,
    clock: C,
}

impl LocalStore<SystemClock> {
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl Default for LocalStore<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> LocalStore<C> {
    pub fn with_clock(clock: C) -> Self {
        Self {
            states: DashMap::new(),
            clock,
        }
    }

    fn state_for(&self, key: &str) -> Arc<Mutex<KeyState>> {
        let now = self.clock.unix_secs();
        if let Some(entry) = self.states.get(key) {
            return entry.clone();
        }
        self.states
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(KeyState::new(now))))
            .clone()
    }

    /// Drop buckets untouched for longer than `idle_for`, so state for
    /// api keys, models and policies the control plane deleted does not
    /// accrue for the process lifetime. Bucket keys are config-derived, so
    /// this is bounded by configuration cardinality rather than by traffic —
    /// a slow leak whose only other remedy is a restart.
    ///
    /// A bucket still holding a concurrency permit is never dropped: the
    /// permit is released from a `Drop`, and releasing into a bucket that no
    /// longer exists would leave the next reservation seeing a fresh zero.
    pub fn reap(&self, idle_for: std::time::Duration) {
        let cutoff = self.clock.unix_secs().saturating_sub(idle_for.as_secs());
        self.states.retain(|_, state| {
            let s = state.lock();
            s.in_flight > 0 || s.last_touched >= cutoff
        });
    }
}

#[async_trait]
impl<C: Clock> RateStore for LocalStore<C> {
    async fn acquire(
        &self,
        key: &str,
        limits: &RateLimit,
        _member: &str,
    ) -> Result<(), RateLimitError> {
        let now = self.clock.unix_secs();
        let state = self.state_for(key);
        let mut s = state.lock();

        // Concurrency first — cheapest and never consumes a window slot.
        if let Some(max) = limits.concurrency {
            if s.in_flight >= max {
                return Err(RateLimitError::Concurrency);
            }
        }

        // Token limits — checked but not incremented. We refuse new
        // requests if the previous minute/hour/day already overran the cap.
        if let Some(max) = limits.tpm {
            if let Some(retry) = s.tpm.is_exceeded(now, max) {
                return Err(RateLimitError::Tokens {
                    scope: RateLimitScope::Tokens,
                    retry_after_secs: retry,
                });
            }
        }
        if let Some(max) = limits.tph {
            if let Some(retry) = s.tph.is_exceeded(now, max) {
                return Err(RateLimitError::Tokens {
                    scope: RateLimitScope::Tokens,
                    retry_after_secs: retry,
                });
            }
        }
        if let Some(max) = limits.tpd {
            if let Some(retry) = s.tpd.is_exceeded(now, max) {
                return Err(RateLimitError::Tokens {
                    scope: RateLimitScope::Tokens,
                    retry_after_secs: retry,
                });
            }
        }

        // Request limits — checked AND incremented. Layered chain
        // (rps → rpm → rph → rpd) so a tighter window short-circuits a
        // looser one without consuming its slot. If any later layer
        // rejects, every earlier-incremented counter is rolled back by
        // exactly the delta this call contributed — concurrent sibling
        // requests' increments survive.
        let mut rps_incremented = false;
        if let Some(max) = limits.rps {
            if let WindowCheck::Full { retry_after_secs } = s.rps.check_and_increment(now, 1, max) {
                return Err(RateLimitError::Requests {
                    scope: RateLimitScope::Requests,
                    retry_after_secs,
                });
            }
            rps_incremented = true;
        }
        let mut rpm_incremented = false;
        if let Some(max) = limits.rpm {
            if let WindowCheck::Full { retry_after_secs } = s.rpm.check_and_increment(now, 1, max) {
                if rps_incremented {
                    s.rps.decrement(now, 1);
                }
                return Err(RateLimitError::Requests {
                    scope: RateLimitScope::Requests,
                    retry_after_secs,
                });
            }
            rpm_incremented = true;
        }
        let mut rph_incremented = false;
        if let Some(max) = limits.rph {
            if let WindowCheck::Full { retry_after_secs } = s.rph.check_and_increment(now, 1, max) {
                if rpm_incremented {
                    s.rpm.decrement(now, 1);
                }
                if rps_incremented {
                    s.rps.decrement(now, 1);
                }
                return Err(RateLimitError::Requests {
                    scope: RateLimitScope::Requests,
                    retry_after_secs,
                });
            }
            rph_incremented = true;
        }
        if let Some(max) = limits.rpd {
            if let WindowCheck::Full { retry_after_secs } = s.rpd.check_and_increment(now, 1, max) {
                if rph_incremented {
                    s.rph.decrement(now, 1);
                }
                if rpm_incremented {
                    s.rpm.decrement(now, 1);
                }
                if rps_incremented {
                    s.rps.decrement(now, 1);
                }
                return Err(RateLimitError::Requests {
                    scope: RateLimitScope::Requests,
                    retry_after_secs,
                });
            }
        }

        s.in_flight += 1;
        s.last_touched = now;
        Ok(())
    }

    async fn commit(&self, key: &str, tokens: u64, _member: &str) {
        let now = self.clock.unix_secs();
        let state = self.state_for(key);
        let mut s = state.lock();
        s.tpm.add(now, tokens);
        s.tph.add(now, tokens);
        s.tpd.add(now, tokens);
        s.in_flight = s.in_flight.saturating_sub(1);
        s.last_touched = now;
    }

    fn release(&self, key: &str, _member: &str) {
        // Non-inserting: a release for a never-acquired bucket is a no-op,
        // so the Redis store's belt-and-suspenders local release on the
        // happy path doesn't pollute the local map with empty state.
        if let Some(state) = self.states.get(key) {
            let mut s = state.lock();
            s.in_flight = s.in_flight.saturating_sub(1);
            s.last_touched = self.clock.unix_secs();
        }
    }

    fn reap(&self, idle_for: std::time::Duration) {
        LocalStore::reap(self, idle_for);
    }

    fn add_tokens(&self, key: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let now = self.clock.unix_secs();
        let state = self.state_for(key);
        let mut s = state.lock();
        s.tpm.add(now, tokens);
        s.tph.add(now, tokens);
        s.tpd.add(now, tokens);
        s.last_touched = now;
    }

    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        let now = self.clock.unix_secs();
        let state = self.states.get(key)?;
        let mut s = state.lock();

        let rpm_used = s.rpm.current(now);
        let tpm_used = s.tpm.current(now);
        let in_flight = s.in_flight;

        // Seconds remaining in the current minute-window. Zero if the
        // window just started or has already rolled.
        let minute_reset = MINUTE_SECS - (now % MINUTE_SECS);

        Some(RateLimitStatus {
            rpm_limit: limits.rpm,
            rpm_used,
            rpm_reset_secs: minute_reset,
            tpm_limit: limits.tpm,
            tpm_used,
            tpm_reset_secs: minute_reset,
            concurrency_limit: limits.concurrency,
            in_flight,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use std::time::Duration;

    fn limits() -> RateLimit {
        RateLimit {
            rpm: Some(10),
            ..Default::default()
        }
    }

    /// An hour token cap enforces, and resets when its window rolls.
    ///
    /// `tph` was accepted by the control plane and silently ignored by the
    /// gateway (#396): an operator could set an hourly token budget, see it
    /// stored, and have it never refuse anything. The `tpd` counter beside it
    /// worked the whole time, which is what made the gap invisible — the
    /// feature looked present because its sibling was.
    #[tokio::test]
    async fn an_hour_token_cap_refuses_once_the_hour_is_spent_and_frees_on_roll() {
        let clock = TestClock::new(1_000);
        let store = LocalStore::with_clock(clock.clone());
        let l = RateLimit {
            tph: Some(100),
            ..Default::default()
        };

        // Token windows are checked, not incremented, at admission — so the
        // first request is always admitted and the cap bites on the next one.
        store.acquire("k", &l, "m1").await.unwrap();
        store.commit("k", 100, "m1").await;

        let err = store
            .acquire("k", &l, "m2")
            .await
            .expect_err("the hour's tokens are spent");
        assert!(
            matches!(err, RateLimitError::Tokens { .. }),
            "the refusal must name the token dimension, not concurrency: {err:?}",
        );

        // Same hour, one second later: still refused.
        clock.advance(1);
        assert!(store.acquire("k", &l, "m3").await.is_err());

        // The window rolls and the budget is fresh again.
        clock.advance(HOUR_SECS);
        store
            .acquire("k", &l, "m4")
            .await
            .expect("a new hour starts with an unspent budget");
    }

    /// The hour counter is its own bucket: spending the hourly budget must not
    /// consume the daily one, or a `tph` + `tpd` pair would refuse at the
    /// tighter cap forever.
    #[tokio::test]
    async fn the_hour_and_day_token_counters_are_independent() {
        let clock = TestClock::new(1_000);
        let store = LocalStore::with_clock(clock.clone());
        let l = RateLimit {
            tph: Some(100),
            tpd: Some(1_000),
            ..Default::default()
        };

        store.acquire("k", &l, "m1").await.unwrap();
        store.commit("k", 100, "m1").await;
        assert!(
            store.acquire("k", &l, "m2").await.is_err(),
            "the hour is spent",
        );

        clock.advance(HOUR_SECS);
        store
            .acquire("k", &l, "m3")
            .await
            .expect("the day still has 900 tokens left, so the next hour serves");
    }

    /// Buckets are keyed by config cardinality (api key × model × policy),
    /// so a long-lived process accumulates state for rows the control plane
    /// deleted hours ago. A bucket still holding a concurrency permit must
    /// survive: dropping it would leave the `Drop`-based release
    /// decrementing a bucket that no longer exists, and the next
    /// reservation would see a fresh zero.
    #[tokio::test]
    async fn reap_drops_idle_buckets_but_never_one_holding_a_permit() {
        let clock = TestClock::new(1_000);
        let store = LocalStore::with_clock(clock.clone());
        let l = limits();

        store.acquire("idle", &l, "m1").await.unwrap();
        store.release("idle", "m1");
        store.acquire("busy", &l, "m2").await.unwrap();

        clock.advance(DAY_SECS + 1);
        store.reap(Duration::from_secs(DAY_SECS));

        assert!(
            store.peek("idle", &l).await.is_none(),
            "an idle bucket must be reclaimed"
        );
        assert!(
            store.peek("busy", &l).await.is_some(),
            "a bucket with a live concurrency permit must be kept"
        );
    }

    #[tokio::test]
    async fn reap_keeps_a_recently_touched_bucket() {
        let clock = TestClock::new(1_000);
        let store = LocalStore::with_clock(clock.clone());
        let l = limits();

        store.acquire("recent", &l, "m1").await.unwrap();
        store.release("recent", "m1");
        clock.advance(60);
        store.reap(Duration::from_secs(DAY_SECS));

        assert!(store.peek("recent", &l).await.is_some());
    }
}
