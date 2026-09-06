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
    /// Micro-USD spent on this bucket since the process started counting.
    ///
    /// No window. It is the one counter here that never rolls, which is what
    /// makes a granted allowance behave like an allowance instead of a
    /// per-window ceiling that refills on its own.
    consumed_micro_usd: u64,
    /// The members currently holding a concurrency slot, by name — not a
    /// bare count.
    ///
    /// A counter cannot tell a release APART from a release: decrementing on
    /// any call lets a caller hand back a slot it never took, and the limit
    /// opens by one every time that happens. The Redis backend has always
    /// been member-aware (a ZSET it `ZREM`s by name), so the two answered
    /// differently for the same sequence — and the unpaired release is not
    /// hypothetical: `RedisStore::release` calls into this store
    /// unconditionally as a belt-and-suspenders local release, so a request
    /// served by Redis can decrement a slot a DEGRADED request is still
    /// holding here.
    in_flight: std::collections::HashSet<String>,
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
            consumed_micro_usd: 0,
            in_flight: std::collections::HashSet::new(),
            last_touched: now,
        }
    }
}

/// Per-process fixed-window store.
pub struct LocalStore<C: Clock = SystemClock> {
    states: DashMap<String, Arc<Mutex<KeyState>>>,
    /// `claim_once` markers, expiring at the stored unix second. Separate
    /// from `states`: those are rate-limit buckets that `reap` prunes by
    /// idleness, these carry their own deadline and must not be reclaimed
    /// early — an expired claim lets the side effect it guards happen twice.
    claims: DashMap<String, u64>,
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
            claims: DashMap::new(),
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
            // `consumed_micro_usd` has NO window and never rolls, so dropping
            // it is not reclaiming a stale bucket — it is handing the caller
            // their whole `granted_micro_usd` back. The idle threshold is
            // chosen so it "can never drop a bucket whose daily window is
            // still open", which is the right rule for rpm/rph/rpd and simply
            // does not apply to a lifetime total: a key that goes quiet for a
            // day would come back with the allowance refilled, turning a
            // cumulative cap into a per-active-day one.
            //
            // The Redis side already states this rule, in the Lua that writes
            // the counter: cumulative spend takes no window and no EXPIRE,
            // "setting an expiry degrades it into a cap". Reaping the bucket
            // is that expiry by another name.
            //
            // Keeping these does not make the map grow with traffic: it is
            // keyed by configured identities (api key / model / policy), so
            // the survivors are bounded by the configuration, and only the
            // ones that actually spent anything survive.
            !s.in_flight.is_empty() || s.last_touched >= cutoff || s.consumed_micro_usd > 0
        });
    }
}

#[async_trait]
impl<C: Clock> RateStore for LocalStore<C> {
    async fn acquire(
        &self,
        key: &str,
        limits: &RateLimit,
        member: &str,
    ) -> Result<(), RateLimitError> {
        let now = self.clock.unix_secs();
        let state = self.state_for(key);
        let mut s = state.lock();

        // Concurrency first — cheapest and never consumes a window slot.
        if let Some(max) = limits.concurrency {
            if s.in_flight.len() as u32 >= max {
                return Err(RateLimitError::Concurrency);
            }
        }

        // Cumulative allowance — checked ahead of the windows, because it is
        // the one that does not come back on its own, so when both would
        // reject it is the more useful thing to report.
        if let Some(granted) = limits.granted_micro_usd {
            if s.consumed_micro_usd >= granted {
                return Err(RateLimitError::AllowanceExhausted);
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

        s.in_flight.insert(member.to_string());
        s.last_touched = now;
        Ok(())
    }

    async fn commit(&self, key: &str, tokens: u64, member: &str) {
        let now = self.clock.unix_secs();
        let state = self.state_for(key);
        let mut s = state.lock();
        s.tpm.add(now, tokens);
        s.tph.add(now, tokens);
        s.tpd.add(now, tokens);
        // Spend buckets pass micro-USD through this same argument, so the
        // cumulative counter accumulates next to the windowed ones.
        s.consumed_micro_usd = s.consumed_micro_usd.saturating_add(tokens);
        s.in_flight.remove(member);
        s.last_touched = now;
    }

    fn release(&self, key: &str, member: &str) {
        // Non-inserting: a release for a never-acquired bucket is a no-op,
        // so the Redis store's belt-and-suspenders local release on the
        // happy path doesn't pollute the local map with empty state.
        if let Some(state) = self.states.get(key) {
            let mut s = state.lock();
            s.in_flight.remove(member);
            s.last_touched = self.clock.unix_secs();
        }
    }

    fn reap(&self, idle_for: std::time::Duration) {
        LocalStore::reap(self, idle_for);
    }

    async fn claim_once(&self, key: &str, ttl: std::time::Duration) -> bool {
        let now = self.clock.unix_secs();
        // 过期的顺手清掉：这些标记按自己的截止时间活，`reap` 的空闲判据不适用。
        self.claims.retain(|_, expires_at| *expires_at > now);
        // 只在缺失时写 —— 被拒的一次不能重写截止时间，否则反复来问就能把标记
        // 无限续命，而 Redis 的 `SET NX` 在已存在时什么都不做。两个后端在这一点
        // 上不一致，换存储就会改变收费次数。
        match self.claims.entry(key.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => false,
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                // `.max(1)` mirrors the Redis side. `as_secs()` TRUNCATES, so
                // any sub-second ttl became 0 — the marker then expired the
                // instant it was written and `retain` above dropped it on the
                // next call, so `claim_once` granted every time and the guard
                // against a repeated non-idempotent side effect (a second
                // charge) was a no-op. Redis clamps because `EX 0` is not a
                // valid expiry; the same clamp is what keeps the two backends
                // answering alike, which is the property the comment above is
                // about.
                slot.insert(now.saturating_add(ttl.as_secs().max(1)));
                true
            }
        }
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
        // 与 `commit` 完全一致：花费层通过同一个参数传 micro-USD，累计计数器
        // 必须跟着涨。
        //
        // 这条路是**流式**的记账入口（`add_tokens_post_stream_all`），非流式走
        // `commit`。少了这一行，累计额度闸对流式流量就等于不存在 —— 而流式是
        // LLM 客户端的主流模式，所以那道闸看起来配好了、实际上从不触发。
        // Redis 那侧的 `ADD_TOKENS_LUA` 一直有 `INCRBY prefix:consumed`，是本地
        // store 落了单。
        s.consumed_micro_usd = s.consumed_micro_usd.saturating_add(tokens);
        s.last_touched = now;
    }

    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        let now = self.clock.unix_secs();
        let state = self.states.get(key)?;
        let mut s = state.lock();

        let rpm_used = s.rpm.current(now);
        let tpm_used = s.tpm.current(now);
        let in_flight = s.in_flight.len() as u32;

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
impl<C: Clock> LocalStore<C> {
    /// 读回某个桶已提交的 tpd 计数，仅供测试断言。
    pub fn committed_tokens(&self, key: &str) -> u64 {
        let now = self.clock.unix_secs();
        self.states
            .get(key)
            .map(|s| s.lock().tpd.current(now))
            .unwrap_or(0)
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
    /// 一次性标记：第一个调用者拿到，之后的拿不到，直到 TTL 过去。
    ///
    /// 进程内这份的寿命就是进程的寿命 —— 它保护的计数器也是，两者同寿是刻意
    /// 的（见 `RateStore::claim_once`）。共享存储那侧的跨实例语义由
    /// `tests/redis_allowance.rs` 覆盖。
    #[tokio::test]
    async fn a_one_shot_claim_is_granted_once_until_it_expires() {
        let clock = TestClock::new(1_000);
        let store = LocalStore::with_clock(clock.clone());
        let ttl = std::time::Duration::from_secs(60);

        assert!(store.claim_once("b1", ttl).await, "第一次该拿到");
        assert!(!store.claim_once("b1", ttl).await, "第二次不该拿到");
        assert!(store.claim_once("b2", ttl).await, "别的键互不影响");

        // 边界要和共享存储那侧一致：Redis 的 `EX 60` 在第 60 秒键就没了，
        // 所以「有效期」是左闭右开的 [t, t+ttl)。两个后端在这一点上不一致的话，
        // 换存储就会改变收费次数。
        clock.advance(59);
        assert!(!store.claim_once("b1", ttl).await, "还在有效期内就放行了");

        // 上面那次被拒的调用不得把截止时间往后推：能推的话，反复来问就能让
        // 标记永不过期。`SET NX` 在键已存在时什么都不做，这里要一样。
        clock.advance(1);
        assert!(
            store.claim_once("b1", ttl).await,
            "到 t+ttl 该已过期 —— 与 Redis `EX` 同一边界；这里若不过期，多半是\
             被拒的那次调用重写了截止时间"
        );
    }

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

#[cfg(test)]
mod allowance_tests {
    use super::*;
    use crate::clock::TestClock;

    fn granted(n: u64) -> RateLimit {
        RateLimit {
            granted_micro_usd: Some(n),
            ..RateLimit::default()
        }
    }

    #[tokio::test]
    async fn 累计额度用尽后拒绝() {
        let s = LocalStore::with_clock(TestClock::new(0));
        let lim = granted(1_000);
        s.acquire("k", &lim, "").await.unwrap();
        s.commit("k", 600, "").await;
        // 还没到，放行。
        s.acquire("k", &lim, "").await.unwrap();
        s.commit("k", 600, "").await;
        // 已消费 1200 ≥ 1000，拒绝。
        assert!(matches!(
            s.acquire("k", &lim, "").await,
            Err(RateLimitError::AllowanceExhausted)
        ));
    }

    /// 流式响应走的是 `add_tokens`，不是 `commit` —— 它也必须记进累计额。
    ///
    /// 流式的记账时机跟非流式不同：非流式在拿到完整应答时 `commit`，流式在流
    /// 结束的同步回调里 `add_tokens`。两条路径记的都是同一份账，漏掉任何一条，
    /// 那道闸对该模式的流量就等于不存在 —— 而流式是 LLM 客户端的主流模式。
    /// Redis 那侧的脚本一直是两条都记的，本地 store 曾经只记窗口桶。
    #[tokio::test]
    async fn 流式的记账也要计入累计额() {
        let s = LocalStore::with_clock(TestClock::new(0));
        let lim = granted(1_000);
        s.acquire("k", &lim, "").await.unwrap();
        s.add_tokens("k", 600);
        s.acquire("k", &lim, "").await.unwrap();
        s.add_tokens("k", 600);
        // 已消费 1200 ≥ 1000。只记窗口桶的话这里照样放行 —— 用户可以一直流下去。
        assert!(
            matches!(
                s.acquire("k", &lim, "").await,
                Err(RateLimitError::AllowanceExhausted)
            ),
            "流式记的账没有计入累计额，这道闸对流式流量形同不存在"
        );
    }

    /// 两条记账路径必须等价：同样的数字，同样的结果。
    #[tokio::test]
    async fn 流式与非流式记账对累计额等价() {
        let a = LocalStore::with_clock(TestClock::new(0));
        let b = LocalStore::with_clock(TestClock::new(0));
        let lim = granted(1_000);
        a.acquire("k", &lim, "").await.unwrap();
        a.commit("k", 1_000, "").await;
        b.acquire("k", &lim, "").await.unwrap();
        b.add_tokens("k", 1_000);
        assert_eq!(
            a.acquire("k", &lim, "").await.is_err(),
            b.acquire("k", &lim, "").await.is_err(),
            "同样的花费，流式与非流式给出了不同的判定"
        );
    }

    #[tokio::test]
    async fn 跨天不重置_这是它跟窗口上限的全部区别() {
        let clock = TestClock::new(0);
        let s = LocalStore::with_clock(clock.clone());
        let lim = granted(1_000);
        s.acquire("k", &lim, "").await.unwrap();
        s.commit("k", 1_500, "").await;
        assert!(s.acquire("k", &lim, "").await.is_err());

        // 往后跳两天。窗口型上限到这里就「续杯」了 —— 累计额不能。
        clock.set(2 * DAY_SECS + 10);
        assert!(
            matches!(
                s.acquire("k", &lim, "").await,
                Err(RateLimitError::AllowanceExhausted)
            ),
            "跨天之后额度自己回来了 —— 那就退化成了窗口上限",
        );
    }

    #[tokio::test]
    async fn 提高发放额即可继续_不需要重置任何东西() {
        let s = LocalStore::with_clock(TestClock::new(0));
        s.acquire("k", &granted(1_000), "").await.unwrap();
        s.commit("k", 1_500, "").await;
        assert!(s.acquire("k", &granted(1_000), "").await.is_err());
        // 把总额提到 5000：已消费 1500 < 5000，放行。补额就是「换一个更大的数」。
        s.acquire("k", &granted(5_000), "").await.unwrap();
    }

    #[tokio::test]
    async fn 没配发放额时不参与判断() {
        let s = LocalStore::with_clock(TestClock::new(0));
        let none = RateLimit::default();
        s.acquire("k", &none, "").await.unwrap();
        s.commit("k", 10_000_000, "").await;
        // 消费再多，没配额度就不该被这条拦住。
        s.acquire("k", &none, "").await.unwrap();
    }

    #[tokio::test]
    async fn 累计额与窗口上限同时生效_互不替代() {
        let clock = TestClock::new(0);
        let s = LocalStore::with_clock(clock.clone());
        let both = RateLimit {
            granted_micro_usd: Some(10_000),
            tpd: Some(1_000),
            ..RateLimit::default()
        };
        s.acquire("k", &both, "").await.unwrap();
        s.commit("k", 1_200, "").await;
        // 当天上限已破，拒绝。
        assert!(s.acquire("k", &both, "").await.is_err());
        // 跨天：窗口回来了，累计额还剩 8800，所以放行。
        clock.set(DAY_SECS + 10);
        s.acquire("k", &both, "").await.unwrap();
    }

    /// Reaping an idle bucket must not hand back a CUMULATIVE allowance.
    ///
    /// `reap` exists for the windowed counters, and its rationale says so:
    /// one day plus a margin, "so it can never drop a bucket whose daily
    /// window is still open" — a bucket idle longer than that has rolled and
    /// carries nothing worth keeping. True of rpm/rph/rpd. NOT true of
    /// `consumed_micro_usd`, which has no window and never rolls: dropping it
    /// resets the spend to zero and gives the caller their whole
    /// `granted_micro_usd` back.
    ///
    /// That turns a lifetime allowance into a per-active-day one, silently,
    /// for any key that goes quiet for a day.
    #[tokio::test]
    async fn 回收闲置桶不能把累计额度还回去() {
        let clock = TestClock::new(0);
        let s = LocalStore::with_clock(clock.clone());
        let lim = granted(1_000);

        s.acquire("k", &lim, "").await.unwrap();
        s.commit("k", 1_500, "").await;
        assert!(
            matches!(
                s.acquire("k", &lim, "").await,
                Err(RateLimitError::AllowanceExhausted)
            ),
            "spent past the allowance, so it must refuse"
        );

        // 闲置超过回收阈值，然后回收 —— 生产里每 5 分钟跑一次。
        clock.set(DAY_SECS + 3600);
        s.reap(std::time::Duration::from_secs(DAY_SECS + 300));

        assert!(
            matches!(
                s.acquire("k", &lim, "").await,
                Err(RateLimitError::AllowanceExhausted)
            ),
            "the allowance is cumulative — going quiet for a day must not \
             refill it"
        );
    }
}
