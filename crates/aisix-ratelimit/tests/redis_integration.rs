//! Shared-counter tests for `RedisStore` against a live Redis.
//!
//! Runs only when `RATELIMIT_TEST_REDIS_URL` is set (CI spins
//! `redis:7-alpine` as a service; absence is a no-op so local unit runs
//! stay hermetic). Two `RedisStore` instances stand in for two DP
//! replicas pointed at one Redis — the exact api7/#798 shape:
//! a limit hit on one replica must already be hit on the other.

use std::sync::Arc;
use std::time::Duration;

use aisix_core::{RateLimit, RateLimitScope, RedisConnConfig, RedisMode};
use aisix_ratelimit::{Limiter, MultiReservation, RateStore, RedisStore};

fn redis_url() -> Option<String> {
    std::env::var("RATELIMIT_TEST_REDIS_URL").ok()
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
    let nodes = std::env::var("RATELIMIT_TEST_REDIS_CLUSTER_NODES").ok()?;
    Some(RedisConnConfig {
        mode: RedisMode::Cluster,
        nodes: nodes.split(',').map(|s| s.trim().to_string()).collect(),
        ..Default::default()
    })
}

/// Sentinel topology, e.g. `RATELIMIT_TEST_REDIS_SENTINELS=redis://127.0.0.1:26379`
/// plus `RATELIMIT_TEST_REDIS_MASTER=mymaster`.
fn sentinel_cfg() -> Option<RedisConnConfig> {
    let sentinels = std::env::var("RATELIMIT_TEST_REDIS_SENTINELS").ok()?;
    let master = std::env::var("RATELIMIT_TEST_REDIS_MASTER").ok()?;
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

#[tokio::test]
async fn post_stream_token_usage_is_ordered_before_the_next_replica_acquire() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let a = Limiter::with_store(Arc::new(store(&url).await));
    let b = Limiter::with_store(Arc::new(store(&url).await));
    let key = unique_key("tpm-stream-exact");
    let limits = RateLimit {
        tpm: Some(1_000),
        ..rl()
    };

    a.add_tokens_post_stream(&key, 1_000);

    assert!(matches!(
        b.pre_commit(&key, &limits).await,
        Err(aisix_ratelimit::RateLimitError::Tokens { .. }),
    ));
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
            Err(aisix_ratelimit::RateLimitError::Concurrency)
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
    // 1s slot lifetime: a never-released slot (crashed replica) is pruned.
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

    tokio::time::sleep(Duration::from_millis(1_300)).await;
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
        Err(aisix_ratelimit::RateLimitError::Concurrency),
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
    a.renew_concurrency_lease(&key, "released-member").await;
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
        a.renew_concurrency_lease(&key, "committed-member"),
        a.commit(&key, 0, "committed-member"),
    );

    assert_eq!(a.peek(&key, &limits).await.unwrap().in_flight, 0);
    a.renew_concurrency_lease(&key, "committed-member").await;
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
        Err(aisix_ratelimit::RateLimitError::Concurrency),
    ));

    drop(stream_guard);
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
