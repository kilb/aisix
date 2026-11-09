//! Shared-counter tests for `RedisStore` against a live Redis.
//!
//! Runs only when `RATELIMIT_TEST_REDIS_URL` is set (CI spins
//! `redis:7-alpine` as a service; absence is a no-op so local unit runs
//! stay hermetic). Two `RedisStore` instances stand in for two DP
//! replicas pointed at one Redis: a limit hit on one replica must be hit
//! on the other.
//!
//! Admission-time counters (requests, concurrency) are shared
//! synchronously — the acquire round-trips to Redis, so the second replica
//! sees the first one's increment before it answers. Post-hoc counters
//! (tokens) are not: their totals are only known once the upstream
//! has replied, and the update is handed to a worker rather than awaited,
//! so a wedged Redis cannot add latency to the stream that just finished.
//! For those, the shared counter is reached promptly, not instantly, and
//! the tests wait for it with an asserted bound.
//!
//! Getting that distinction wrong is not hypothetical: the post-stream
//! hand-off became a queue while a test still asserted the synchronous
//! shape, and because these tests are gated behind an environment variable
//! that a local run leaves unset, the failure went unseen.

use std::sync::Arc;
use std::time::Duration;

use aisix_core::{RateLimit, RateLimitScope, RedisConnConfig, RedisMode};
use aisix_obs::metrics::Metrics;
use aisix_ratelimit::{
    store::redis::DEFAULT_PREFIX, Limiter, MultiReservation, RateLimitError, RateStore, RedisStore,
    StreamLeaseRefresh,
};

/// 读一个**闸门**环境变量：没设就跳过这条用例，但在 CI 上直接失败。
///
/// 本地不设是常态，跳过是对的。CI 上不设意味着那份配置掉了 —— 而后果是这套
/// 测试从此安静地停跑：一行 stderr，没人会看见。这个仓库刚在
/// `aisix-ratelimit` 那套上吃过一次同样的亏。
fn gate_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => {
            assert!(
                std::env::var("CI").is_err(),
                "CI 上没有设 {name}：跳过就是静默无覆盖"
            );
            None
        }
    }
}

fn redis_url() -> Option<String> {
    gate_var("RATELIMIT_TEST_REDIS_URL")
}

fn single(url: &str) -> RedisConnConfig {
    RedisConnConfig {
        mode: RedisMode::Single,
        url: Some(url.to_string()),
        ..Default::default()
    }
}

/// Cluster seed nodes, e.g. `RATELIMIT_TEST_REDIS_CLUSTER_NODES=redis://127.0.0.1:7000,redis://127.0.0.1:7001`.
fn cluster_cfg() -> Option<RedisConnConfig> {
    let nodes = gate_var("RATELIMIT_TEST_REDIS_CLUSTER_NODES")?;
    Some(RedisConnConfig {
        mode: RedisMode::Cluster,
        nodes: nodes.split(',').map(|s| s.trim().to_string()).collect(),
        ..Default::default()
    })
}

/// Sentinel topology, e.g. `RATELIMIT_TEST_REDIS_SENTINELS=redis://127.0.0.1:26379`
/// plus `RATELIMIT_TEST_REDIS_MASTER=mymaster`.
fn sentinel_cfg() -> Option<RedisConnConfig> {
    let sentinels = gate_var("RATELIMIT_TEST_REDIS_SENTINELS")?;
    let master = gate_var("RATELIMIT_TEST_REDIS_MASTER")?;
    Some(RedisConnConfig {
        mode: RedisMode::Sentinel,
        sentinels: sentinels.split(',').map(|s| s.trim().to_string()).collect(),
        master_name: Some(master),
        ..Default::default()
    })
}

/// Unique bucket key per test so they don't clobber each other (the store
/// prefixes with a fixed `aisix:rl`; isolation comes from the key).
fn unique_key(tag: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("test:{tag}:{nanos:x}")
}

fn rl() -> RateLimit {
    RateLimit::default()
}

async fn store(url: &str) -> RedisStore {
    RedisStore::connect(&single(url))
        .await
        .expect("redis connect")
}

async fn redis_subsecond_micros(url: &str) -> i64 {
    let client = redis::Client::open(url).expect("redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("redis connection");
    let time: Vec<i64> = redis::cmd("TIME")
        .query_async(&mut conn)
        .await
        .expect("redis TIME");
    *time.get(1).expect("TIME microseconds")
}

#[tokio::test]
async fn rpm_counter_is_shared_across_replicas() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("rpm");
    let limits = RateLimit {
        rpm: Some(1),
        ..rl()
    };

    // Replica A burns the only slot in the minute window.
    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");

    // Replica B sees the SAME counter → rejected. Pre-#798 (per-replica
    // memory) this would have been allowed, doubling the limit.
    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("second replica must be rejected by shared counter");
    assert!(
        matches!(
            err,
            aisix_ratelimit::RateLimitError::Requests {
                scope: RateLimitScope::Requests,
                ..
            }
        ),
        "got {err:?}"
    );
}

/// The shared backend must describe a refusal exactly as the local one
/// does: the 429's `x-ratelimit-*` headers are the same contract on a
/// clustered deployment as on a single node.
///
/// The Lua script reports the refused dimension as an INDEX into the
/// same `request_dims` / `token_dims` list the caller pushed into ARGV,
/// so the name is reconstructed on the Rust side. A key carrying several
/// windows is what makes that mapping observable — with a single window
/// any index would decode to the same name.
#[tokio::test]
async fn refusal_detail_survives_the_shared_backend() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("detail");
    // rps is looser than rpm here, so the SECOND dimension in the
    // request list is the one that refuses — an off-by-one in the index
    // mapping would report `rps`.
    let limits = RateLimit {
        rps: Some(10),
        rpm: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("shared counter must refuse the second replica");

    let detail = err.detail();
    assert_eq!(detail.dimension, "rpm", "got {err:?}");
    assert_eq!(detail.limit, 1);
    assert_eq!(detail.remaining, 0);
    assert!(
        (1..=60).contains(&detail.reset_secs),
        "a minute window resets within the minute, got {}",
        detail.reset_secs
    );
}

/// A concurrency refusal from the shared backend carries the gauge's
/// own state plus the fixed hint — the same shape the local store
/// produces, so a client cannot tell the backends apart.
#[tokio::test]
async fn concurrency_refusal_detail_survives_the_shared_backend() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("conc-detail");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("shared concurrency gauge must refuse");

    let detail = err.detail();
    assert_eq!(detail.dimension, "concurrency", "got {err:?}");
    assert_eq!(detail.limit, 1);
    assert_eq!(detail.remaining, 0);
    assert_eq!(
        detail.reset_secs,
        aisix_ratelimit::CONCURRENCY_RETRY_AFTER_SECS
    );
    assert_eq!(detail.reset_secs, 60);
}

/// Sleep until the next whole second starts.
///
/// The rps counter buckets server-side: the Lua script reads `now` from
/// `redis.call('TIME')` and derives `ws = now - (now % window)`. So the
/// boundary that matters is Redis's clock at script-execution time, and
/// a test that starts at an arbitrary offset into a second is racing
/// whatever is left of it. Aligning first puts a full second between the
/// pair and the next boundary, so only a Redis stalled for more than a
/// second could still split them — which is a real failure, not a race.
///
/// This uses the client clock to align against a server-side bucket,
/// which holds because CI runs Redis as a service container on the same
/// host. A remote Redis with clock skew would need the alignment read
/// from `TIME` instead.
async fn wait_for_second_boundary() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the epoch");
    let remainder = Duration::from_nanos(u64::from(now.subsec_nanos()));
    tokio::time::sleep(Duration::from_secs(1) - remainder).await;
}

#[tokio::test]
async fn rps_window_rolls_over_on_the_shared_counter() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("rps");
    let limits = RateLimit {
        rps: Some(1),
        ..rl()
    };

    // The rps counter buckets by whole wall-clock second, so both
    // acquires have to land inside the SAME bucket for the second one to
    // be rejected. Starting mid-second, two Redis round-trips can
    // straddle the boundary and the second call is legitimately allowed
    // — the assertion below then fails with nothing wrong in the code.
    // Wait out whatever is left of the current second first, so the pair
    // begins with a full one ahead of it.
    wait_for_second_boundary().await;

    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    assert!(
        b.acquire(&key, &limits, "b-1").await.is_err(),
        "same second is shared-rejected"
    );

    // Cross the 1s boundary — the next-second key is fresh.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    b.acquire(&key, &limits, "b-2")
        .await
        .expect("next second has a fresh window");
}

#[tokio::test]
async fn token_usage_is_shared_across_replicas() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("tpm");
    let limits = RateLimit {
        tpm: Some(1_000),
        ..rl()
    };

    // A admits then over-commits the minute's token budget.
    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    a.commit(&key, 1_500, "a-1").await;

    // B's pre-check sees tpm > 1000 on the shared counter → rejected.
    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("token cap is shared");
    assert!(
        matches!(err, aisix_ratelimit::RateLimitError::Tokens { .. }),
        "got {err:?}"
    );
}

/// #950: the shared backend must draw the boundary where the local one
/// does. Committed usage landing EXACTLY on the token cap is a spent
/// budget, so the next request — on any replica — is refused. A `>` here
/// would make the observable limit depend on which backend is
/// configured, which is the one thing swapping them must never change.
#[tokio::test]
async fn a_token_window_exactly_on_the_limit_refuses_the_next_request() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("tpm-exact");
    let limits = RateLimit {
        tpm: Some(1_000),
        ..rl()
    };

    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    a.commit(&key, 1_000, "a-1").await; // the whole budget, exactly

    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("a fully consumed token budget must stop admitting");
    assert!(
        matches!(err, aisix_ratelimit::RateLimitError::Tokens { .. }),
        "got {err:?}"
    );

    // One token short still admits, so the assertion above is about the
    // budget being spent rather than about any usage at all.
    let under = unique_key("tpm-under");
    a.acquire(&under, &limits, "a-2")
        .await
        .expect("first allowed");
    a.commit(&under, 999, "a-2").await;
    b.acquire(&under, &limits, "b-2")
        .await
        .expect("one token of budget left still admits");
}

#[tokio::test]
async fn token_usage_at_exact_limit_blocks_the_next_replica() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("tpm-exact");
    let limits = RateLimit {
        tpm: Some(1_000),
        ..rl()
    };

    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    a.commit(&key, 1_000, "a-1").await;

    assert!(matches!(
        b.acquire(&key, &limits, "b-1").await,
        Err(aisix_ratelimit::RateLimitError::Tokens { .. }),
    ));
}

/// A limit reached on one replica must be reached on every other — that is
/// the whole point of pointing them at one Redis.
///
/// It arrives asynchronously. `add_tokens_post_stream` hands the update to
/// a worker and returns without waiting, so that a wedged Redis cannot add
/// latency to the stream that just finished; the guarantee is that the
/// other replica sees it, not that it sees it on the next instruction. This
/// test asserted the synchronous shape, and went on asserting it after the
/// hand-off became a queue — it was gated behind an unset environment
/// variable, so nobody saw it fail.
///
/// Waiting for it here is not a weakened assertion: the original claim (the
/// second replica refuses) still has to hold, and the bound is asserted
/// too, so the delay cannot quietly grow into something that matters.
/// Measured on a local Redis: 179µs min, 495µs median, 2.4ms max over 20
/// runs — the bound below is orders of magnitude above that, because a
/// flaky test is worse than a loose one.
#[tokio::test]
async fn post_stream_token_usage_reaches_the_next_replica_promptly() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    const BOUND: Duration = Duration::from_millis(250);

    let a = Limiter::with_store(Arc::new(store(&url).await));
    let b = Limiter::with_store(Arc::new(store(&url).await));
    let key = unique_key("tpm-stream-exact");
    let limits = RateLimit {
        tpm: Some(1_000),
        ..rl()
    };

    let started = std::time::Instant::now();
    a.add_tokens_post_stream(&key, 1_000);

    loop {
        if matches!(
            b.pre_commit(&key, &limits).await,
            Err(aisix_ratelimit::RateLimitError::Tokens { .. }),
        ) {
            break;
        }
        assert!(
            started.elapsed() < BOUND,
            "the second replica still admits requests {:?} after the first \
             recorded a full window's tokens — the shared counter is not \
             being shared",
            started.elapsed(),
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn concurrency_slot_is_shared_and_released_across_replicas() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let key = unique_key("conc");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    // A takes the only in-flight slot.
    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed");
    // B is blocked while A holds it.
    assert!(
        matches!(
            b.acquire(&key, &limits, "b-1").await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "concurrency slot must be shared across replicas"
    );

    // A finishes → releases the slot (sync + detached ZREM). The ZREM is
    // fire-and-forget, so poll until the slot frees (bounded) rather than
    // assuming a fixed propagation delay that could flake on slow CI.
    a.release(&key, "a-1");
    let mut acquired = false;
    for _ in 0..50 {
        if b.acquire(&key, &limits, "b-2").await.is_ok() {
            acquired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(acquired, "slot must free up cluster-wide after release");
}

#[tokio::test]
async fn stale_concurrency_slot_is_reclaimed_after_ttl() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    // A 1s slot lifetime plus the one-second rolling-upgrade compatibility
    // margin: a never-released slot (crashed replica) is eventually pruned.
    let a = store(&url).await.with_conc_ttl(1);
    let b = store(&url).await.with_conc_ttl(1);
    let key = unique_key("conc-ttl");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "a-leaked")
        .await
        .expect("first allowed");
    // Never release — simulate a crashed replica holding the slot.
    assert!(
        b.acquire(&key, &limits, "b-1").await.is_err(),
        "slot held while fresh"
    );

    tokio::time::sleep(Duration::from_millis(2_200)).await;
    b.acquire(&key, &limits, "b-2")
        .await
        .expect("stale slot reclaimed after conc_ttl");
}

#[tokio::test]
async fn concurrency_ttl_does_not_round_acquisition_down_to_the_second() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await.with_conc_ttl(1);
    let b = store(&url).await.with_conc_ttl(1);
    let key = unique_key("conc-ms");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    // Acquire late in a Redis server second, then probe shortly after the
    // boundary. A whole-second score makes this ~400ms-old lease look one
    // second old and prunes it; a millisecond score keeps the full TTL.
    loop {
        let micros = redis_subsecond_micros(&url).await;
        if (700_000..=800_000).contains(&micros) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    a.acquire(&key, &limits, "late-in-second")
        .await
        .expect("initial slot");
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert!(matches!(
        b.acquire(&key, &limits, "too-early").await,
        Err(aisix_ratelimit::RateLimitError::Concurrency { .. }),
    ));
    a.release(&key, "late-in-second");
}

#[tokio::test]
async fn late_renewal_cannot_recreate_a_released_concurrency_member() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await.with_conc_ttl(5);
    let b = store(&url).await.with_conc_ttl(5);
    let key = unique_key("conc-release-renew");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "released-member")
        .await
        .expect("initial slot");
    a.release(&key, "released-member");
    for _ in 0..100 {
        if a.peek(&key, &limits).await.unwrap().in_flight == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a.peek(&key, &limits).await.unwrap().in_flight, 0);

    // Force the hazardous ordering: renewal executes after ZREM completed.
    a.refresh_stream_lease(&key, "released-member").await;
    assert_eq!(a.peek(&key, &limits).await.unwrap().in_flight, 0);
    b.acquire(&key, &limits, "replacement")
        .await
        .expect("late renewal must not consume the freed slot");
}

#[tokio::test]
async fn commit_racing_renewal_always_removes_the_concurrency_member() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await.with_conc_ttl(5);
    let key = unique_key("conc-commit-renew");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "committed-member")
        .await
        .expect("initial slot");
    tokio::join!(
        a.refresh_stream_lease(&key, "committed-member"),
        a.commit(&key, 0, "committed-member"),
    );

    assert_eq!(a.peek(&key, &limits).await.unwrap().in_flight, 0);
    a.refresh_stream_lease(&key, "committed-member").await;
    assert_eq!(a.peek(&key, &limits).await.unwrap().in_flight, 0);
}

#[tokio::test]
async fn active_stream_renews_its_concurrency_slot_past_ttl() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let limiter = Limiter::with_store(Arc::new(store(&url).await.with_conc_ttl(1)));
    let other = store(&url).await.with_conc_ttl(1);
    let key = unique_key("conc-renew");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    let reservation = limiter
        .pre_commit(&key, &limits)
        .await
        .expect("stream takes the slot");
    let stream_guard = MultiReservation::new(vec![reservation]).into_stream_hold();

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(matches!(
        other.acquire(&key, &limits, "other-1").await,
        Err(aisix_ratelimit::RateLimitError::Concurrency { .. }),
    ));

    drop(stream_guard);
}

#[tokio::test]
async fn refreshing_released_member_does_not_recreate_slot() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await.with_conc_ttl(1);
    let b = store(&url).await.with_conc_ttl(1);
    let key = unique_key("conc-refresh-release");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "a-stream")
        .await
        .expect("first stream allowed");
    // `commit` removes the member synchronously in Redis and clears the
    // store's ownership record. A queued refresh must neither recreate it
    // nor convert this normal teardown into a terminal stream signal.
    a.commit(&key, 0, "a-stream").await;
    assert_eq!(
        a.refresh_stream_lease_outcome(&key, "a-stream").await,
        StreamLeaseRefresh::Unavailable,
        "a released member is no longer a live shared lease"
    );

    b.acquire(&key, &limits, "b-stream")
        .await
        .expect("refresh after release must not recreate the slot");
}

/// 合并 upstream 之前，这个仓库把并发成员的分数写成毫秒。升级时正在跑的请求不会被
/// release，它们的成员就以毫秒分数留在 ZSET 里。秒制的清理范围够不到这么大的分数，
/// 而每次 acquire 都会续这个键的过期，所以不单独处理的话，这个桶的并发上限会一直少一个。
/// 还没过期的那些仍然要算数：它们可能是还活着的请求。
#[tokio::test]
async fn a_millisecond_member_from_before_the_upgrade_ages_out_by_its_ttl() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let b = store(&url).await.with_conc_ttl(5);
    let key = unique_key("conc-ms-upgrade");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };
    let client = redis::Client::open(url.as_str()).expect("raw Redis client");
    let mut raw = client
        .get_multiplexed_async_connection()
        .await
        .expect("raw Redis connection");
    let time: Vec<String> = redis::cmd("TIME")
        .query_async(&mut raw)
        .await
        .expect("Redis TIME");
    let now_ms: u64 = time[0].parse::<u64>().expect("TIME seconds") * 1_000;
    let conc_key = format!("{DEFAULT_PREFIX}:{{{key}}}:conc");

    let _: i64 = redis::cmd("ZADD")
        .arg(&conc_key)
        .arg(now_ms)
        .arg("before-upgrade")
        .query_async(&mut raw)
        .await
        .expect("install a live millisecond member");
    assert!(
        matches!(
            b.acquire(&key, &limits, "too-early").await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "a millisecond member inside its ttl may still be a live request"
    );

    // 同一个成员退回到 TTL 之外：崩溃副本留下的就是这样。
    let _: i64 = redis::cmd("ZADD")
        .arg(&conc_key)
        .arg(now_ms - 7_000)
        .arg("before-upgrade")
        .query_async(&mut raw)
        .await
        .expect("age the millisecond member");
    b.acquire(&key, &limits, "after-ttl")
        .await
        .expect("a stale millisecond member must not hold the slot forever");
}

#[tokio::test]
async fn legacy_integer_member_survives_the_upgrade_prune_boundary() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let b = store(&url).await.with_conc_ttl(1);
    let key = unique_key("conc-legacy-upgrade");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };
    let client = redis::Client::open(url.as_str()).expect("raw Redis client");
    let mut raw = client
        .get_multiplexed_async_connection()
        .await
        .expect("raw Redis connection");

    // A pre-renewal gateway wrote one integer-second score. Install a score
    // that is exactly on the new build's stale boundary while still early in
    // the current server second. An inclusive or fractional prune erases it
    // too early; the conservative exclusive boundary keeps it.
    let legacy_second = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let time: Vec<String> = redis::cmd("TIME")
                .query_async(&mut raw)
                .await
                .expect("Redis TIME");
            let seconds: u64 = time[0].parse().expect("TIME seconds");
            let micros: u64 = time[1].parse().expect("TIME microseconds");
            if micros < 500_000 {
                return seconds;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reach an early Redis second");
    let conc_key = format!("{DEFAULT_PREFIX}:{{{key}}}:conc");
    let _: i64 = redis::cmd("ZADD")
        .arg(&conc_key)
        .arg(legacy_second.saturating_sub(1))
        .arg("legacy-stream")
        .query_async(&mut raw)
        .await
        .expect("install legacy member");
    let _: i64 = redis::cmd("EXPIRE")
        .arg(&conc_key)
        .arg(5)
        .query_async(&mut raw)
        .await
        .expect("keep legacy member for the boundary check");

    assert!(
        matches!(
            b.acquire(&key, &limits, "new-stream").await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "new code must not prune a live legacy member at the upgrade boundary"
    );
}

#[tokio::test]
async fn stream_hold_renews_redis_lease_until_drop() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    // Keep this short to prove a live stream is not mistaken for a crashed
    // replica. The stores model two DPs sharing the same Redis backend.
    let a = Limiter::with_store(Arc::new(store(&url).await.with_conc_ttl(1)));
    let b = Limiter::with_store(Arc::new(store(&url).await.with_conc_ttl(1)));
    let key = unique_key("conc-stream-lease");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    let hold =
        MultiReservation::new(vec![a.pre_commit(&key, &limits).await.unwrap()]).into_stream_hold();

    // This is longer than conc_ttl plus the one-second rolling-upgrade
    // compatibility slack. Without the stream lease heartbeat, B's acquire
    // prunes A and incorrectly admits a second live stream.
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert!(
        matches!(
            b.pre_commit(&key, &limits).await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "a live stream must keep its shared concurrency slot beyond conc_ttl"
    );

    drop(hold);
    // The release is detached, so bound the wait instead of assuming a
    // propagation delay on a slow CI runner.
    let mut acquired = false;
    for _ in 0..50 {
        if b.pre_commit(&key, &limits).await.is_ok() {
            acquired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(acquired, "slot must free cluster-wide when the stream ends");
}

#[tokio::test]
async fn stream_hold_reports_a_definitively_missing_redis_lease() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = Limiter::with_store(Arc::new(store(&url).await.with_conc_ttl(1)));
    let key = unique_key("conc-stream-lease-loss");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };
    let hold =
        MultiReservation::new(vec![a.pre_commit(&key, &limits).await.unwrap()]).into_stream_hold();

    let client = redis::Client::open(url.as_str()).expect("raw Redis client");
    let mut raw = client
        .get_multiplexed_async_connection()
        .await
        .expect("raw Redis connection");
    let conc_key = format!("{DEFAULT_PREFIX}:{{{key}}}:conc");
    let members: Vec<String> = redis::cmd("ZRANGE")
        .arg(&conc_key)
        .arg(0)
        .arg(-1)
        .query_async(&mut raw)
        .await
        .expect("read live stream member");
    assert_eq!(members.len(), 1, "one stream hold owns one Redis member");
    let removed: i64 = redis::cmd("ZREM")
        .arg(&conc_key)
        .arg(&members[0])
        .query_async(&mut raw)
        .await
        .expect("remove live stream member");
    assert_eq!(removed, 1, "remove the member the hold is renewing");

    // The response body normally subscribes only after headers are sent. Let
    // the heartbeat observe `Missing` before this test subscribes, then prove
    // the one-way state survives that handoff with no receiver present.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut lost = hold.lease_loss_receiver();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if *lost.borrow_and_update() {
                return;
            }
            lost.changed()
                .await
                .expect("stream hold retains the lease-loss sender");
        }
    })
    .await
    .expect("next Redis renewal must report the missing member");

    drop(hold);
}

/// The response-header phase can outlive Redis's crash-recovery TTL before
/// the gateway knows that the response is an SSE stream. That live request
/// must keep its slot through the later handoff to `StreamConcurrencyGuard`.
#[tokio::test]
async fn reservation_renews_redis_lease_before_stream_handoff() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = Limiter::with_store(Arc::new(store(&url).await.with_conc_ttl(1)));
    let b = Limiter::with_store(Arc::new(store(&url).await.with_conc_ttl(1)));
    let key = unique_key("conc-pre-stream-lease");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    // Simulate a slow upstream header phase. Before this fix, the member
    // expired here because renewal did not start until `into_stream_hold`.
    let reservation = MultiReservation::new(vec![a.pre_commit(&key, &limits).await.unwrap()]);
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert!(
        matches!(
            b.pre_commit(&key, &limits).await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "an active pre-handoff request must keep its shared concurrency slot"
    );

    let hold = reservation.into_stream_hold();
    tokio::time::sleep(Duration::from_millis(2_200)).await;
    assert!(
        matches!(
            b.pre_commit(&key, &limits).await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "the same lease must remain held after the stream handoff"
    );

    drop(hold);
    let mut acquired = false;
    for _ in 0..50 {
        if b.pre_commit(&key, &limits).await.is_ok() {
            acquired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(acquired, "slot must free cluster-wide when the stream ends");
}

/// Redis Cluster: the multi-key acquire/commit Lua must route to the slot
/// owning the `{bucket}` hash tag and enforce one shared window. A wrong
/// (or missing) routing key would surface as a CROSSSLOT/MOVED error here.
#[tokio::test]
async fn cluster_shared_counter_routes_multi_key_lua() {
    let Some(cfg) = cluster_cfg() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_CLUSTER_NODES not set");
        return;
    };
    let a = RedisStore::connect(&cfg).await.expect("cluster connect");
    let b = RedisStore::connect(&cfg).await.expect("cluster connect");
    let key = unique_key("cluster-rpm");
    let limits = RateLimit {
        rpm: Some(1),
        tpm: Some(1_000),
        concurrency: Some(5),
        ..rl()
    };

    // First acquire (touches conc ZSET + rpm + tpm keys, all hash-tagged
    // to one slot) must succeed — proves the EVAL routed correctly.
    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed on cluster");
    // commit also runs a multi-key script on the same slot.
    a.commit(&key, 10, "a-1").await;
    // Second replica is rejected by the shared rpm counter — assert the
    // specific rejection, not just any error (which would also pass on a
    // routing/connection failure and mask a real cluster regression).
    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("second replica must be rejected by the shared rpm counter");
    assert!(
        matches!(
            err,
            aisix_ratelimit::RateLimitError::Requests {
                scope: RateLimitScope::Requests,
                ..
            }
        ),
        "got {err:?}"
    );
}

/// Redis Sentinel: connect resolves the master through the sentinels, and
/// the shared-counter semantics work end-to-end against the discovered
/// master.
#[tokio::test]
async fn sentinel_shared_counter_round_trips() {
    let Some(cfg) = sentinel_cfg() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_SENTINELS / _MASTER not set");
        return;
    };
    let a = RedisStore::connect(&cfg).await.expect("sentinel connect");
    let b = RedisStore::connect(&cfg).await.expect("sentinel connect");
    let key = unique_key("sentinel-rpm");
    let limits = RateLimit {
        rpm: Some(1),
        ..rl()
    };

    a.acquire(&key, &limits, "a-1")
        .await
        .expect("first allowed via sentinel master");
    let err = b
        .acquire(&key, &limits, "b-1")
        .await
        .expect_err("second replica must be rejected by the shared rpm counter");
    assert!(
        matches!(
            err,
            aisix_ratelimit::RateLimitError::Requests {
                scope: RateLimitScope::Requests,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn env_namespace_isolates_model_alias_bucket() {
    // The model inline rate limit buckets on the env-local alias
    // (`model:<name>`), which is NOT a globally-unique id. Two environments
    // sharing one Redis must keep independent counters for the same alias —
    // `with_env_namespace` is what separates them. (The api_key / policy
    // buckets are UUIDs and never collided; the prefix just covers them too.)
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    // A shared run tag keeps the test hermetic; the env-a / env-b suffixes are
    // what must isolate the two counters.
    let run = unique_key("modelns");
    let env_a_ns = format!("{run}:env-a");
    let env_b_ns = format!("{run}:env-b");

    let env_a = store(&url).await.with_env_namespace(&env_a_ns);
    let env_b = store(&url).await.with_env_namespace(&env_b_ns);

    // Identical alias bucket in both environments.
    let key = "model:gpt-4o";
    let limits = RateLimit {
        rpm: Some(1),
        ..rl()
    };

    // env-a burns its single rpm slot for the alias.
    env_a
        .acquire(key, &limits, "a-1")
        .await
        .expect("env-a first allowed");
    env_a
        .acquire(key, &limits, "a-2")
        .await
        .expect_err("env-a is now at its own limit");

    // env-b, identical alias bucket, is unaffected — a distinct counter.
    env_b
        .acquire(key, &limits, "b-1")
        .await
        .expect("different env must not share the model:<alias> counter");

    // Control: a second handle in env-a shares the exhausted counter, proving
    // the isolation comes from the env namespace, not the handle identity.
    let env_a2 = store(&url).await.with_env_namespace(&env_a_ns);
    env_a2
        .acquire(key, &limits, "a-3")
        .await
        .expect_err("same env shares the counter");
}

/// Counts what the store reports, so a test can assert on the shed signal
/// without a Prometheus registry.
#[derive(Default)]
struct CountingSink {
    shed: std::sync::atomic::AtomicU64,
    calls: std::sync::atomic::AtomicU64,
}

impl aisix_core::RateLimitMetricsSink for CountingSink {
    fn record_redis_failure(&self, _op: &str) {}

    fn record_post_stream_shed(&self, dropped: u64) {
        self.shed
            .fetch_add(dropped, std::sync::atomic::Ordering::Relaxed);
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Shedding a post-stream update means a caller's tokens never reach the
/// shared counters, so they can run past a token limit by the dropped
/// amount.
///
/// The store logs that once per process. A sustained shed therefore looks
/// identical to one that recovered a second later, which is the state an
/// operator most needs to tell apart. Every occurrence must be counted.
#[tokio::test]
async fn every_shed_post_stream_update_is_counted_not_just_the_first() {
    let Some(url) = redis_url() else {
        eprintln!("RATELIMIT_TEST_REDIS_URL unset — skipping");
        return;
    };
    let sink = Arc::new(CountingSink::default());
    let store = store(&url).await.with_metrics(sink.clone());

    // Fill the hand-off queue past its depth. Each key is one offer, so a
    // batch far larger than the queue guarantees the tail is shed whatever
    // the worker manages to drain concurrently.
    let keys: Vec<String> = (0..20_000)
        .map(|i| unique_key(&format!("shed:{i}")))
        .collect();
    store.add_tokens_all(&keys, 7);

    let shed = sink.shed.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        shed > 0,
        "queue depth was never exceeded — test drove too little work"
    );

    // Drive a second batch: the log latch has already fired, so this is the
    // window where a warn-once-only signal goes silent.
    let before = shed;
    let more: Vec<String> = (0..20_000)
        .map(|i| unique_key(&format!("shed2:{i}")))
        .collect();
    store.add_tokens_all(&more, 7);
    assert!(
        sink.shed.load(std::sync::atomic::Ordering::Relaxed) > before,
        "shedding stopped being reported after the first batch — a sustained \
         shed must stay visible, that is the whole point of the counter",
    );
}

/// 可开关的 TCP 转发器，夹在 store 与 Redis 之间模拟一次断线：`down` 切断所有
/// 在途连接，并在新连接一到就关掉；`up` 恢复转发。
struct FlakyProxy {
    url: String,
    up: Arc<std::sync::atomic::AtomicBool>,
    links: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl FlakyProxy {
    async fn start(redis_url: &str) -> Self {
        use std::sync::atomic::Ordering;
        let target = redis_url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .expect("redis url carries host:port")
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind proxy");
        let url = format!("redis://{}", listener.local_addr().expect("proxy addr"));
        let up = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let links = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (up_flag, link_set) = (Arc::clone(&up), Arc::clone(&links));
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                if !up_flag.load(Ordering::SeqCst) {
                    continue; // 丢弃即关闭
                }
                let target = target.clone();
                let link = tokio::spawn(async move {
                    if let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                link_set.lock().expect("links").push(link);
            }
        });
        Self { url, up, links }
    }

    fn down(&self) {
        self.up.store(false, std::sync::atomic::Ordering::SeqCst);
        for link in self.links.lock().expect("links").drain(..) {
            link.abort();
        }
    }

    fn up(&self) {
        self.up.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 断线期间的 acquire 会降级到本地 store、在本地占一个并发位。Redis 恢复后
/// 这次请求的 commit 走 Redis，本地那个位也必须一并还掉 —— 否则它永远留在
/// 本地，下一次断线时本地并发上限就少一个，而且每次抖动都会再少一个。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slot_taken_while_degraded_is_released_by_a_commit_after_recovery() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let proxy = FlakyProxy::start(&url).await;
    let flaky = store(&proxy.url).await;
    let direct = store(&url).await;
    let key = unique_key("degraded-slot");
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    // 断线：acquire 降级，在本地占位。
    proxy.down();
    flaky
        .acquire(&key, &limits, "m1")
        .await
        .expect("a degraded acquire is served by the local store");

    // 恢复：等到 flaky 的 acquire 确实走到了 Redis —— 直连的那个副本能看见它
    // 占的位 —— 再让 m1 的 commit 走 Redis 那条路。
    proxy.up();
    let probe = unique_key("degraded-probe");
    let mut recovered = false;
    // 断线打开了连接层的熔断器，它的探测一个窗口才试一次，恢复最多要等这么久。
    let deadline =
        std::time::Instant::now() + aisix_redis::BREAKER_WINDOW + Duration::from_secs(10);
    let mut i = 0u32;
    while std::time::Instant::now() < deadline {
        i += 1;
        let member = format!("p{i}");
        if flaky.acquire(&probe, &limits, &member).await.is_ok() {
            let seen = direct.acquire(&probe, &limits, "d").await.is_err();
            flaky.release(&probe, &member);
            if seen {
                recovered = true;
                break;
            }
            direct.release(&probe, "d");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        recovered,
        "the store never went back to Redis after the outage"
    );
    flaky.commit(&key, 0, "m1").await;

    // 再断一次：本地并发上限 1 必须完整可用。
    proxy.down();
    assert!(
        flaky.acquire(&key, &limits, "m2").await.is_ok(),
        "the slot m1 took while degraded was never released locally"
    );
}

/// 非流式的 token（`commit`）同样要记一份进本地备份，与流式的 `add_tokens_all`
/// 一致。只记流式的话，Redis 断线时本地降级看到的 token 窗口里没有非流式花掉的
/// 那部分 —— 已经打满的窗口在断线期间又放行了。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_tokens_are_enforced_by_the_local_fallback() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let proxy = FlakyProxy::start(&url).await;
    let flaky = store(&proxy.url).await;
    let direct = store(&url).await;
    let key = unique_key("unary-tokens");
    let limits = RateLimit {
        concurrency: Some(1),
        tpd: Some(1_000),
        ..rl()
    };

    // 连着 Redis。直连的副本拿不到并发位，说明这次 acquire 确实走的是 Redis ——
    // 降级到本地的话本地 commit 自己就会记账，这条用例什么都证明不了。
    flaky
        .acquire(&key, &limits, "m1")
        .await
        .expect("inside the window");
    assert!(
        direct.acquire(&key, &limits, "d").await.is_err(),
        "the first acquire did not go through Redis"
    );
    // 非流式请求结束：token 越过当天上限，经 commit 记账（同时还掉并发位）。
    flaky.commit(&key, 1_500, "m1").await;
    // commit 也走的是 Redis：直连的副本这时看到的是打满的窗口。
    let seen = direct.acquire(&key, &limits, "d").await;
    assert!(
        matches!(seen, Err(RateLimitError::Tokens { .. })),
        "the commit did not reach Redis: {seen:?}"
    );

    // 断线：降级到本地，这笔非流式的 token 必须也在本地那份里。
    proxy.down();
    let r = flaky.acquire(&key, &limits, "m2").await;
    assert!(
        matches!(r, Err(RateLimitError::Tokens { .. })),
        "the local fallback never saw the unary tokens: {r:?}"
    );
}

/// `claim_once` 是给「会重放、但记账不能重复」的副作用用的：批处理跑完之后的
/// 用量归因就是这种 —— 它的用量事件带确定性 id，消费端能去重，而
/// Prometheus 计数器没有那层保护，重放一次就是多记一遍。
///
/// 用两个各自 `connect` 出来的存储，模拟「重启后的同一个网关」与「另一个实例」
/// —— 它们不共享任何进程内状态，只共享 Redis。标记要在这种情况下仍然只发一次。
#[tokio::test]
async fn a_one_shot_claim_is_granted_once_across_instances() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let ttl = Duration::from_secs(600);
    let first = unique_key("batch-spend-1");
    let second = unique_key("batch-spend-2");

    assert!(a.claim_once(&first, ttl).await, "第一次该拿到标记");
    assert!(
        !a.claim_once(&first, ttl).await,
        "同一个存储重来一次不该再拿到"
    );
    assert!(
        !b.claim_once(&first, ttl).await,
        "另一个实例（也等价于重启后的自己）不该再拿到 —— 这正是本地内存那份\
         做不到、必须放进共享存储的原因"
    );
    // 另一个批次互不影响。
    assert!(
        b.claim_once(&second, ttl).await,
        "不同批次的标记被互相挡住了"
    );

    // TTL 真的落在键上：没有过期时间的标记会永久占着 Redis。
    let client = redis::Client::open(url.as_str()).expect("redis client");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("redis connection");
    let marker = format!("{DEFAULT_PREFIX}:{{claim:{first}}}");
    let secs: i64 = redis::cmd("TTL")
        .arg(&marker)
        .query_async(&mut conn)
        .await
        .expect("TTL 可用");
    assert!(
        secs > 0 && secs <= 600,
        "标记键的 TTL 不对（{secs}）—— -2 表示键没建出来，-1 表示永不过期，那是泄漏"
    );
}

/// A TCP relay in front of Redis that a test can break, in either of the
/// two shapes a real outage takes.
///
/// - [`cut`](RedisCutoff::cut) — the relay answers each request with a
///   Redis error instead of forwarding it. Redis is *reachable* and
///   refusing, which is the cheap failure: the client learns immediately.
/// - [`blackhole`](RedisCutoff::blackhole) — the socket stays open and
///   nothing is ever forwarded or answered, which is what a stopped
///   container, a downed host or a partitioned network looks like from
///   the client end. Nothing arrives, nothing is refused, and without a
///   command budget the caller waits on TCP retransmission for minutes.
/// - [`fail_next_reply_after_apply`](RedisCutoff::fail_next_reply_after_apply)
///   — forward one command to Redis, replace its successful reply with a
///   local Redis error, then keep the TCP session usable. This models the
///   post-dispatch ambiguity without opening the client's connectivity
///   breaker, so the next refresh can prove ownership deterministically.
struct RedisCutoff {
    port: u16,
    cut: std::sync::Arc<std::sync::atomic::AtomicBool>,
    hole: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_reply_after_apply: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RedisCutoff {
    async fn start(upstream: &str) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let upstream = upstream
            .trim_start_matches("redis://")
            .trim_end_matches('/')
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cut = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hole = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fail_reply_after_apply = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cut.clone();
        let hole_flag = hole.clone();
        let fail_reply_after_apply_flag = fail_reply_after_apply.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut server) = tokio::net::TcpStream::connect(&upstream).await else {
                    continue;
                };
                let flag = flag.clone();
                let hole_flag = hole_flag.clone();
                let fail_reply_after_apply_flag = fail_reply_after_apply_flag.clone();
                tokio::spawn(async move {
                    let mut from_client = [0u8; 8192];
                    let mut from_server = [0u8; 8192];
                    loop {
                        tokio::select! {
                            n = client.read(&mut from_client) => {
                                let Ok(n) = n else { return };
                                if n == 0 {
                                    return;
                                }
                                if hole_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                    // Swallowed: no forward, no reply, no
                                    // close. The peer has simply gone quiet.
                                } else if flag.load(std::sync::atomic::Ordering::Relaxed) {
                                    if client
                                        .write_all(b"-ERR simulated redis outage\r\n")
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                } else if server.write_all(&from_client[..n]).await.is_err() {
                                    return;
                                }
                            }
                            n = server.read(&mut from_server) => {
                                let Ok(n) = n else { return };
                                // EOF first: a closed upstream read stays
                                // ready forever, so `continue`-ing on it
                                // would spin the relay task.
                                if n == 0 {
                                    return;
                                }
                                if hole_flag.load(std::sync::atomic::Ordering::Relaxed) {
                                    continue;
                                }
                                if fail_reply_after_apply_flag
                                    .swap(false, std::sync::atomic::Ordering::Relaxed)
                                {
                                    // Redis applied this command, but the
                                    // client receives an error instead of
                                    // its success reply. Keeping the socket
                                    // alive avoids tripping the 30s network
                                    // breaker and leaves a deterministic
                                    // post-dispatch ambiguity to resolve.
                                    if client
                                        .write_all(b"-ERR simulated lost redis reply\r\n")
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    continue;
                                }
                                if client.write_all(&from_server[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        Self {
            port,
            cut,
            hole,
            fail_reply_after_apply,
        }
    }

    fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
    }

    fn cut(&self) {
        self.cut.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn blackhole(&self) {
        self.hole.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn fail_next_reply_after_apply(&self) {
        self.fail_reply_after_apply
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Forward again. Connections opened from now on work; the ones the
    /// blackhole swallowed mid-handshake were abandoned by the client
    /// when its own budget expired, which is what a Redis coming back up
    /// looks like from the caller's end.
    fn heal(&self) {
        self.cut.store(false, std::sync::atomic::Ordering::Relaxed);
        self.hole.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

fn counter_value(rendered: &str, operation: &str) -> f64 {
    let needle = format!("aisix_redis_failures_total{{operation=\"{operation}\"}} ");
    rendered
        .lines()
        .find_map(|l| l.trim().strip_prefix(&needle))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.0)
}

/// #1060: the shared limiter fails OPEN — an unreachable Redis silently
/// degrades every replica to its own in-memory counters, so the cluster
/// stops enforcing one global window and nothing about the answer to a
/// caller changes. The warning logs once per outage, which cannot say how
/// long or how hard it is failing. `aisix_redis_failures_total` is the
/// only signal an operator can scrape, and before this it was never
/// emitted at all.
#[tokio::test]
async fn a_redis_outage_counts_every_failed_operation() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let metrics = Metrics::new(false);
    let relay = RedisCutoff::start(&url).await;
    let store = RedisStore::connect(&single(&relay.url()))
        .await
        .expect("redis connect through the relay")
        .with_metrics(Arc::new(metrics.clone()));
    let key = unique_key("outage");
    let limits = RateLimit {
        rpm: Some(10),
        ..rl()
    };

    // Healthy: the operation succeeds and nothing is counted.
    store
        .acquire(&key, &limits, "m-1")
        .await
        .expect("allowed while Redis is reachable");
    assert_eq!(counter_value(&metrics.render(), "ratelimit_acquire"), 0.0);

    relay.cut();

    // Still admitted — that is the fail-open contract, and exactly why
    // the outage is invisible without the counter.
    store
        .acquire(&key, &limits, "m-2")
        .await
        .expect("fail-open keeps admitting");
    let first = counter_value(&metrics.render(), "ratelimit_acquire");
    assert!(first >= 1.0, "the failed acquire must be counted: {first}");

    // Every subsequent failure counts, not just the first: the log line
    // is one-shot per outage and cannot report a rate.
    store
        .acquire(&key, &limits, "m-3")
        .await
        .expect("fail-open keeps admitting");
    assert!(
        counter_value(&metrics.render(), "ratelimit_acquire") > first,
        "each failed operation counts, not only the one that logged",
    );
}

/// A stream admitted by the local fail-open fallback never had a Redis ZSET
/// member. When Redis comes back, that absence must remain `Unavailable`, not
/// become a terminal `Missing` signal that truncates the still-live stream.
#[tokio::test]
async fn a_fallback_stream_lease_stays_open_after_redis_recovers() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let relay = RedisCutoff::start(&url).await;
    let store = Arc::new(
        RedisStore::connect(&single(&relay.url()))
            .await
            .expect("connect through relay")
            .with_conc_ttl(1),
    );
    let limiter = Limiter::with_store(store.clone());
    let key = unique_key("fallback-stream-recovery");
    let fallback_key = unique_key("fallback-stream-recovery-direct");
    let fallback_member = "fallback-stream-member";
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    relay.cut();
    store
        .acquire(&fallback_key, &limits, fallback_member)
        .await
        .expect("the local fallback admits while Redis is down");
    let hold = MultiReservation::new(vec![limiter.pre_commit(&key, &limits).await.unwrap()])
        .into_stream_hold();
    let mut lost = hold.lease_loss_receiver();

    // A fresh connection through the healed relay proves Redis is serving
    // again. The first stream still has only its local fallback reservation.
    relay.heal();
    let peer = RedisStore::connect(&single(&relay.url()))
        .await
        .expect("Redis recovers through the relay");
    let probe_key = unique_key("fallback-stream-recovery-probe");
    peer.acquire(&probe_key, &limits, "recovered-peer")
        .await
        .expect("the recovered Redis accepts a shared reservation");
    peer.release(&probe_key, "recovered-peer");

    assert_eq!(
        store
            .refresh_stream_lease_outcome(&fallback_key, fallback_member)
            .await,
        StreamLeaseRefresh::Unavailable,
        "a local-only member is never a definitive Redis lease loss"
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !*lost.borrow_and_update(),
        "Redis recovery must not truncate a fallback-admitted stream"
    );
    assert!(
        !lost
            .has_changed()
            .expect("the stream hold retains its lease-loss sender"),
        "fallback refreshes remain fail-open after recovery"
    );

    drop(hold);
    store.release(&fallback_key, fallback_member);
}

/// A lost Redis reply leaves acquire provenance ambiguous: the server may
/// have added the member even though the client fell back locally. Refresh
/// must probe and promote that member when Redis confirms it, not let it age
/// out as if the fallback were certainly local-only.
#[tokio::test]
async fn an_ambiguous_acquire_promotes_when_redis_confirms_the_member() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let relay = RedisCutoff::start(&url).await;
    let store = RedisStore::connect(&single(&relay.url()))
        .await
        .expect("connect through relay")
        .with_conc_ttl(1);
    let limits = RateLimit {
        concurrency: Some(1),
        ..rl()
    };

    // `Script::invoke_async` can load this Lua script with a NOSCRIPT retry
    // before its first real execution. Warm it before replacing a reply so
    // the injected error follows a Redis-applied ZADD on every clean CI DB.
    let warmup_key = unique_key("ambiguous-stream-acquire-warmup");
    store
        .acquire(&warmup_key, &limits, "warmup")
        .await
        .expect("warm acquire loads the Lua script");
    store.commit(&warmup_key, 0, "warmup").await;

    let key = unique_key("ambiguous-stream-acquire");
    let member = "ambiguous-stream-member";

    relay.fail_next_reply_after_apply();
    store
        .acquire(&key, &limits, member)
        .await
        .expect("a lost acquire reply falls back locally");

    // The reply disappeared only after Redis processed it, so a different
    // replica sees the shared slot and proves the member exists remotely.
    let peer = RedisStore::connect(&single(&relay.url()))
        .await
        .expect("peer connects through relay");
    assert!(
        matches!(
            peer.acquire(&key, &limits, "peer-member").await,
            Err(aisix_ratelimit::RateLimitError::Concurrency { .. })
        ),
        "the ambiguous member must be present in Redis"
    );

    let refreshed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match store.refresh_stream_lease_outcome(&key, member).await {
                StreamLeaseRefresh::Renewed => return StreamLeaseRefresh::Renewed,
                StreamLeaseRefresh::Unavailable => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                StreamLeaseRefresh::Missing => return StreamLeaseRefresh::Missing,
            }
        }
    })
    .await
    .expect("the recovered connection must confirm the ambiguous member");
    assert_eq!(refreshed, StreamLeaseRefresh::Renewed);

    store.release(&key, member);
}

/// A Redis that stops answering without closing the socket must degrade
/// the request, not hold it.
///
/// The store has always failed open on `Err`, but with no command budget
/// the `Err` never arrived: the command sat on TCP retransmission and the
/// caller — a live proxy request — hung with it. Reported against 1.2.0-rc.1
/// and reproduced identically on v1.1.0, where a `docker stop` of Redis
/// left every rate-limited request unanswered past three minutes.
///
/// Two properties, because each fails without the other: the first
/// command must return inside the budget, and the ones behind it must not
/// each pay that budget again for as long as the outage lasts.
#[tokio::test]
async fn a_silent_redis_fails_open_within_the_command_budget() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let relay = RedisCutoff::start(&url).await;
    // Below the 5s default on purpose: a bound of 6s would pass whether or
    // not the per-block field reached the connection, so it would not be a
    // check at all.
    const BUDGET_SECS: u64 = 2;
    let cfg = RedisConnConfig {
        timeout_secs: BUDGET_SECS,
        ..single(&relay.url())
    };
    let store = RedisStore::connect(&cfg)
        .await
        .expect("redis connect through the relay");
    let key = unique_key("blackhole");
    let limits = RateLimit {
        rpm: Some(10),
        ..rl()
    };

    store
        .acquire(&key, &limits, "m-1")
        .await
        .expect("allowed while Redis answers");

    relay.blackhole();

    let started = std::time::Instant::now();
    store
        .acquire(&key, &limits, "m-2")
        .await
        .expect("fail-open still admits");
    let first = started.elapsed();
    assert!(
        first >= Duration::from_secs(BUDGET_SECS),
        "the budget is what makes it give up; a faster return means the \
         blackhole was not reached, took {first:?}",
    );
    assert!(
        first < Duration::from_millis(BUDGET_SECS * 1000 + 1_500),
        "the first silent command must give up on its own budget, not the \
         default, took {first:?}",
    );

    // Behind it the breaker is open, so this one costs nothing at all —
    // otherwise every request for the length of the outage carries the
    // full budget as added latency.
    let started = std::time::Instant::now();
    store
        .acquire(&key, &limits, "m-3")
        .await
        .expect("fail-open still admits");
    let second = started.elapsed();
    assert!(
        second < Duration::from_millis(500),
        "the command behind a failure must short-circuit, took {second:?}",
    );
}

/// A store whose connection never landed must BE the store an outage
/// produces — every operation failing open to the per-replica counters —
/// and it must take the shared backend over on its own once Redis
/// answers.
///
/// `connect_or_attach_later` is the only production constructor
/// (`RedisStore::connect` is now test-only), and the e2e that covers the
/// boot exercises `acquire` and `commit` alone. The other three
/// operations reach the same empty slot and are what a `Drop`, a
/// post-stream token add and the `x-ratelimit-*` headers run through.
#[tokio::test]
async fn a_store_that_never_connected_fails_open_and_attaches_later() {
    let Some(url) = redis_url() else { return };
    let relay = RedisCutoff::start(&url).await;
    // Silent from the first SYN: the store's own connect is what meets it.
    relay.blackhole();

    let mut cfg = single(&relay.url());
    cfg.timeout_secs = 1;
    let key = unique_key("boot-degraded");
    // rpm=1 is what the fallback must refuse on; the concurrency cap is
    // deliberately ABOVE the two requests below, or the concurrency gate
    // would refuse the second one first and the rpm window would never
    // be the thing under test.
    let limits = RateLimit {
        rpm: Some(1),
        concurrency: Some(2),
        ..Default::default()
    };

    let metrics = Metrics::new(false);
    let started = std::time::Instant::now();
    let (store, unreachable) = RedisStore::connect_or_attach_later(&cfg)
        .await
        .expect("an unreachable Redis is a diagnostic, not a fatal config error");
    let elapsed = started.elapsed();
    let store = store.with_metrics(Arc::new(metrics.clone()));

    // The whole point: it returned, on the configured budget, with the
    // failure as a diagnostic rather than as an error to propagate.
    assert!(
        unreachable.is_some(),
        "an unreachable Redis must be reported"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "connect_or_attach_later spent {elapsed:?}, not the configured 1s budget"
    );

    // Fails OPEN, and the fallback still ENFORCES: the second request in
    // the same minute is refused by the local window, not admitted.
    store
        .acquire(&key, &limits, "boot-1")
        .await
        .expect("the local fallback admits the first request");
    let refused = store
        .acquire(&key, &limits, "boot-2")
        .await
        .expect_err("the local fallback still enforces rpm=1");
    assert!(matches!(
        refused,
        aisix_ratelimit::RateLimitError::Requests { .. }
    ));

    // The remaining three operations run against the same empty slot.
    // `peek` reports the local window rather than nothing at all.
    let status = store
        .peek(&key, &limits)
        .await
        .expect("peek falls back to the local window");
    assert_eq!(status.rpm_limit, Some(1));
    assert_eq!(status.rpm_used, 1);
    store.add_tokens(&key, 7);
    store.release(&key, "boot-1");

    // The degradation is counted, which is the only signal it produces
    // while no log level is raised.
    assert!(
        counter_value(&metrics.render(), "ratelimit_acquire") > 0.0,
        "a degraded acquire must be counted"
    );
    // 记账走后台 worker，它在 `with_metrics` 之前就起来了：失败同样要数到这个 sink 上。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while counter_value(&metrics.render(), "ratelimit_add_tokens") == 0.0 {
        assert!(
            std::time::Instant::now() < deadline,
            "post-stream tokens that cannot reach Redis must be counted"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Redis comes up. The background task attaches it with no restart.
    relay.heal();
    let peer = RedisStore::connect(&single(&relay.url()))
        .await
        .expect("a healed Redis connects");
    wait_for_attach(&store, &peer).await;

    // Discriminating: a SECOND store, connected normally to the same
    // Redis, is already over rpm=1 because of the request the first one
    // just made. While the first store was counting locally this could
    // not hold — the peer's window would be its own and empty.
    let shared_key = unique_key("boot-attached");
    store
        .acquire(&shared_key, &limits, "attached-1")
        .await
        .expect("the attached store admits the first request");
    peer.acquire(&shared_key, &limits, "peer-1")
        .await
        .expect_err("the attached store wrote to the SHARED counter, not to its local one");
}

/// Block until `store` has attached its shared backend, judged by an
/// effect only the shared backend can produce: a token add that `peer` —
/// a second store connected normally to the same Redis — can read back.
///
/// Not `peek`, and not a successful `acquire`: both of those answer from
/// the local fallback too, so a gate built on either falls through
/// immediately and leaves whatever follows it asserting against a store
/// that may still be degraded. (Both gates here were written that way
/// first; a mutation check caught it.)
async fn wait_for_attach(store: &RedisStore, peer: &RedisStore) {
    let probe = unique_key("attach-probe");
    let limits = RateLimit {
        tpm: Some(1_000_000),
        ..Default::default()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        store.add_tokens(&probe, 1);
        tokio::time::sleep(Duration::from_millis(200)).await;
        if peer
            .peek(&probe, &limits)
            .await
            .is_some_and(|s| s.tpm_used > 0)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the shared backend never attached"
        );
    }
}

/// A reservation taken on the local fallback and committed after the
/// shared backend attached must give its local concurrency slot back.
///
/// `LocalStore` has no ttl on `in_flight`, so a slot left behind is left
/// behind for the life of the process: the local baseline for that bucket
/// is permanently raised, and a LATER Redis outage then refuses traffic
/// on a path whose entire contract is to fail open. The boot-degraded
/// state makes this reachable on every cold start that precedes its
/// Redis, where before it needed the exact instant a breaker closed.
#[tokio::test]
async fn a_commit_after_the_backend_attaches_returns_the_local_slot() {
    let Some(url) = redis_url() else { return };
    let relay = RedisCutoff::start(&url).await;
    relay.blackhole();

    let mut cfg = single(&relay.url());
    cfg.timeout_secs = 1;
    let key = unique_key("boot-handover");
    // concurrency=1 and nothing else, so the only thing that can refuse
    // the probe at the end is a leaked slot.
    let limits = RateLimit {
        concurrency: Some(1),
        ..Default::default()
    };

    let (store, unreachable) = RedisStore::connect_or_attach_later(&cfg)
        .await
        .expect("an unreachable Redis is a diagnostic, not a fatal config error");
    assert!(unreachable.is_some());

    // Slot taken locally, while there is no connection.
    store
        .acquire(&key, &limits, "handover-1")
        .await
        .expect("the local fallback admits it");

    relay.heal();
    let peer = RedisStore::connect(&single(&relay.url()))
        .await
        .expect("a healed Redis connects");
    wait_for_attach(&store, &peer).await;

    // Committed against the shared backend, which never held the member.
    store.commit(&key, 0, "handover-1").await;

    // Now force the store back onto the local counters and check the slot
    // came back. Without the release in `commit` this refuses.
    relay.blackhole();
    store
        .acquire(&key, &limits, "handover-2")
        .await
        .expect("the local concurrency slot was returned on commit");
}

/// A config the process rejects BY ITSELF must still end the boot.
///
/// Everything that depends on a server's answer is degraded around and
/// retried in the background, because a server can come back and a
/// credential can be corrected on it. A `url` the driver cannot parse
/// can do neither: nothing outside this process will ever change it, so
/// a gateway that started would be quietly never going to enforce a
/// shared limit, which is strictly worse than the boot failure.
#[tokio::test]
async fn a_config_the_driver_cannot_use_is_still_fatal() {
    let cfg = RedisConnConfig {
        mode: RedisMode::Single,
        url: Some("not-a-redis-url".into()),
        ..Default::default()
    };
    let err = RedisStore::connect_or_attach_later(&cfg)
        .await
        .expect_err("a malformed url must not be degraded around");
    assert!(aisix_redis::is_boot_fatal(&err), "{err:?}");
}

/// A refused credential is NOT fatal — and it is not an outage either.
///
/// The server answered, so the gateway serves degraded and keeps
/// retrying (the credential may be fixed on the server side), but the
/// operator is told a refusal rather than sent to look at the network.
/// Those are two separate claims and this pins both.
///
/// Synthetic errors, so this pins the CLASSIFIER and nothing else. It
/// does not prove a real Redis refusal ever reaches it, and for a while
/// none did — every driver reported a refusal as a connectivity failure,
/// so the refused branch was unreachable in production while a test like
/// this stayed green. `crates/aisix-redis/tests/auth_connect.rs` is what
/// answers that question, against a live server.
#[test]
fn a_refused_credential_is_told_apart_from_an_outage() {
    let refused = redis::RedisError::from((
        redis::ErrorKind::AuthenticationFailed,
        "WRONGPASS invalid username-password pair",
    ));
    assert!(!aisix_redis::is_boot_fatal(&refused));
    assert_eq!(aisix_redis::failure_reason(&refused), "refused");

    let unreachable = redis::RedisError::from((
        redis::ErrorKind::IoError,
        "redis connect timed out",
        "no connection within 5s".to_string(),
    ));
    assert!(!aisix_redis::is_boot_fatal(&unreachable));
    assert_eq!(aisix_redis::failure_reason(&unreachable), "unreachable");
}

/// The whole point of degrading rather than exiting: the credential is
/// corrected ON THE SERVER, and the running gateway picks it up.
///
/// An ACL user rather than `requirepass`, because `requirepass` is
/// server-wide and every other test in this file shares the server. The
/// user is created with one password, the store is configured with
/// another — so the first connect is refused, exactly as a typo'd
/// `AISIX_RATELIMIT__REDIS__PASSWORD` would be — and then the user's
/// password is changed to the configured one with the gateway still
/// running.
#[tokio::test]
async fn a_refused_credential_corrected_on_the_server_is_adopted_without_a_restart() {
    let Some(url) = redis_url() else { return };
    let user = format!("aisix-heal-{}", unique_key("u").replace(':', "-"));
    let wanted = "the-password-the-gateway-holds";

    let admin = aisix_redis::connect(&single(&url))
        .await
        .expect("the admin connection must come up");
    let set_user = |password: &str| {
        let (user, password) = (user.clone(), password.to_string());
        let admin = &admin;
        async move {
            let mut handle = admin.acquire().await.expect("an admin handle");
            redis::cmd("ACL")
                .arg("SETUSER")
                .arg(&user)
                .arg("on")
                .arg(format!(">{password}"))
                .arg("~*")
                .arg("+@all")
                .query_async::<()>(&mut handle)
                .await
                .expect("the ACL user must be settable");
        }
    };
    set_user("not-the-password-the-gateway-holds").await;

    let cfg = RedisConnConfig {
        username: Some(user.clone()),
        password: Some(wanted.into()),
        ..single(&url)
    };
    let (store, degraded) = RedisStore::connect_or_attach_later(&cfg)
        .await
        .expect("a refused credential must NOT end the boot");
    let degraded = degraded.expect("a refused credential must degrade, not connect");
    assert_eq!(
        aisix_redis::failure_reason(&degraded),
        "refused",
        "the operator must be told the server refused, not that it is unreachable: {degraded}"
    );

    // Still enforcing, per replica: a degraded limiter that had stopped
    // counting would serve both of these.
    let key = unique_key("heal");
    let limits = RateLimit {
        rpm: Some(1),
        ..Default::default()
    };
    assert!(store.acquire(&key, &limits, "heal-1").await.is_ok());
    assert!(
        store.acquire(&key, &limits, "heal-2").await.is_err(),
        "per-replica enforcement must hold while the shared backend is refused"
    );

    // The operator fixes it on the server. Nothing restarts.
    set_user(wanted).await;
    let peer = RedisStore::connect(&cfg)
        .await
        .expect("the corrected credential must connect");
    wait_for_attach(&store, &peer).await;

    // The Redis is shared with every other test here and outlives the
    // run, so the user this test invented does not get to accumulate on
    // it one password at a time.
    let mut handle = admin.acquire().await.expect("an admin handle");
    let _ = redis::cmd("ACL")
        .arg("DELUSER")
        .arg(&user)
        .query_async::<()>(&mut handle)
        .await;
}
