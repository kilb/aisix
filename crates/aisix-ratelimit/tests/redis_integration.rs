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
//! (tokens, spend) are not: their totals are only known once the upstream
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
use aisix_ratelimit::{Limiter, MultiReservation, RateLimitError, RateStore, RedisStore};

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

/// Shedding a post-stream update means a caller's tokens — and, since spend
/// ceilings became a local mechanism, their money — never reach the shared
/// counters, so they can spend past a `max_spend_micro_usd` ceiling by the
/// dropped amount.
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
// 多线程：`RedisStore::connect` 同步等后台 worker 建连，转发器得在另一个线程上跑。
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
    for i in 0..100 {
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

/// 流式花费在 Redis 模式下也记一份进本地备份（`add_tokens_all`），供断线时的本地
/// 降级接着用。累计额度的那份不能因为空闲被回收：桶的 acquire 走的是 Redis，本地
/// 从没见过它带的限额，回收认不出这是一份额度，空闲一天后整个收走 —— 下一次断线时
/// 本地降级从 0 起算，用完的额度又放行了。
// 多线程：`RedisStore::connect` 同步等后台 worker 建连，转发器得在另一个线程上跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_spend_on_an_allowance_survives_reaping_into_the_next_outage() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let proxy = FlakyProxy::start(&url).await;
    let flaky = store(&proxy.url).await;
    let direct = store(&url).await;
    let key = unique_key("stream-allowance");
    let limits = RateLimit {
        concurrency: Some(1),
        granted_micro_usd: Some(1_000),
        ..rl()
    };

    // 连着 Redis。直连的副本拿不到并发位，说明这次 acquire 确实走的是 Redis ——
    // 降级到本地的话本地 acquire 自己就会记下额度，这条用例什么都证明不了。
    flaky
        .acquire(&key, &limits, "m1")
        .await
        .expect("inside the allowance");
    assert!(
        direct.acquire(&key, &limits, "d").await.is_err(),
        "the first acquire did not go through Redis"
    );
    flaky.release(&key, "m1");
    // 流结束：花费越过额度，经 add_tokens 记账。
    flaky.add_tokens(&key, 1_500);

    // 例行回收。睡过一个整秒边界，桶才算真的空闲（`reap` 按整秒比较）。
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    flaky.reap(Duration::from_secs(0));

    // 断线：降级到本地，那份累计额度必须还在。
    proxy.down();
    let r = flaky.acquire(&key, &limits, "m2").await;
    assert!(
        matches!(r, Err(RateLimitError::AllowanceExhausted)),
        "the local fallback started the allowance over after reaping: {r:?}"
    );
}

/// 非流式的花费（`commit`）同样要记一份进本地备份，与流式的 `add_tokens_all`
/// 一致。只记流式的话，Redis 断线时本地降级看到的累计额度里没有非流式花掉的
/// 那部分 —— 已经用完的额度在断线期间又放行了。
// 多线程：`RedisStore::connect` 同步等后台 worker 建连，转发器得在另一个线程上跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_spend_on_an_allowance_is_enforced_by_the_local_fallback() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: RATELIMIT_TEST_REDIS_URL not set");
        return;
    };
    let proxy = FlakyProxy::start(&url).await;
    let flaky = store(&proxy.url).await;
    let direct = store(&url).await;
    let key = unique_key("unary-allowance");
    let limits = RateLimit {
        concurrency: Some(1),
        granted_micro_usd: Some(1_000),
        ..rl()
    };

    // 连着 Redis。直连的副本拿不到并发位，说明这次 acquire 确实走的是 Redis ——
    // 降级到本地的话本地 commit 自己就会记账，这条用例什么都证明不了。
    flaky
        .acquire(&key, &limits, "m1")
        .await
        .expect("inside the allowance");
    assert!(
        direct.acquire(&key, &limits, "d").await.is_err(),
        "the first acquire did not go through Redis"
    );
    // 非流式请求结束：花费越过额度，经 commit 记账（同时还掉并发位）。
    flaky.commit(&key, 1_500, "m1").await;
    // commit 也走的是 Redis：直连的副本这时看到的是用完的额度。
    let seen = direct.acquire(&key, &limits, "d").await;
    assert!(
        matches!(seen, Err(RateLimitError::AllowanceExhausted)),
        "the commit did not reach Redis: {seen:?}"
    );

    // 断线：降级到本地，这笔非流式花费必须也在本地那份里。
    proxy.down();
    let r = flaky.acquire(&key, &limits, "m2").await;
    assert!(
        matches!(r, Err(RateLimitError::AllowanceExhausted)),
        "the local fallback never saw the unary spend: {r:?}"
    );
}
