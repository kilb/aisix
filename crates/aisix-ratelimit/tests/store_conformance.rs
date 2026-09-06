//! The two `RateStore` implementations must answer the same way.
//!
//! `LocalStore` counts per replica, `RedisStore` counts across them, and the
//! gateway swaps one for the other on a config flag — so a caller's
//! experience is supposed to differ only in SCOPE, never in semantics. There
//! was no test holding them to that. The trait has ten methods and two
//! implementations, each with its own test module, and each module asserts
//! what its own author expected.
//!
//! That gap has already cost something: `claim_once` shipped with the local
//! implementation refreshing the deadline on a DENIED claim, which `SET NX`
//! does not do. Both sides had tests. Neither compared them.
//!
//! Skips when no Redis is reachable — a plain `redis-server` on PATH is
//! enough, no modules needed — and says so rather than passing silently.

use aisix_core::config::{RedisConnConfig, RedisMode};
use aisix_core::RateLimit;
use aisix_ratelimit::store::RateStore;
use aisix_ratelimit::{LocalStore, RedisStore};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static NS: AtomicU32 = AtomicU32::new(0);

struct Redis {
    owned: Option<(Child, std::path::PathBuf)>,
    url: String,
    ns: String,
}

impl Redis {
    fn start() -> Option<Self> {
        let ns = format!(
            "conf{}-{}",
            std::process::id(),
            NS.fetch_add(1, Ordering::Relaxed)
        );
        if let Ok(url) = std::env::var("RATELIMIT_TEST_REDIS_URL") {
            if !url.trim().is_empty() {
                return Some(Self {
                    owned: None,
                    url: url.trim().to_string(),
                    ns,
                });
            }
        }
        Command::new("redis-server")
            .arg("--version")
            .output()
            .ok()?;
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            l.local_addr().ok()?.port()
        };
        let dir = std::env::temp_dir().join(format!("aisix-rl-conformance-{port}"));
        std::fs::create_dir_all(&dir).ok()?;
        let child = Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--save",
                "",
                "--appendonly",
                "no",
                "--dir",
                &dir.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(Self {
            owned: Some((child, dir)),
            url: format!("redis://127.0.0.1:{port}"),
            ns,
        })
    }

    async fn wait_ready(&self) -> bool {
        for _ in 0..100 {
            if let Ok(client) = redis::Client::open(self.url.clone()) {
                if let Ok(mut c) = client.get_multiplexed_async_connection().await {
                    if redis::cmd("PING")
                        .query_async::<String>(&mut c)
                        .await
                        .is_ok()
                    {
                        return true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }
}

impl Drop for Redis {
    fn drop(&mut self) {
        if let Some((child, dir)) = self.owned.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Run `scenario` against both stores and require the same answer.
///
/// The scenario returns a comparable transcript rather than asserting inside,
/// so a divergence names WHAT differed instead of just which store failed.
async fn both<F, Fut>(label: &str, scenario: F)
where
    F: Fn(std::sync::Arc<dyn RateStore>) -> Fut,
    Fut: std::future::Future<Output = Vec<String>>,
{
    let Some(redis) = Redis::start() else {
        eprintln!("skipping {label}: no RATELIMIT_TEST_REDIS_URL and no redis-server");
        return;
    };
    if !redis.wait_ready().await {
        panic!("redis started but never answered PING — environment, not a skip");
    }
    let cfg = RedisConnConfig {
        mode: RedisMode::Single,
        url: Some(redis.url.clone()),
        ..RedisConnConfig::default()
    };
    let shared: std::sync::Arc<dyn RateStore> = std::sync::Arc::new(
        RedisStore::connect(&cfg)
            .await
            .expect("connect")
            .with_env_namespace(&redis.ns),
    );
    let local: std::sync::Arc<dyn RateStore> = std::sync::Arc::new(LocalStore::new());

    let from_local = scenario(local).await;
    let from_redis = scenario(shared).await;
    assert_eq!(
        from_local, from_redis,
        "{label}: the two stores disagree — a caller's experience should differ \
         between them only in SCOPE (per-replica vs shared), never in semantics"
    );
}

fn rpm(n: u64) -> RateLimit {
    RateLimit {
        rpm: Some(n),
        ..RateLimit::default()
    }
}

#[tokio::test]
async fn a_request_ceiling_admits_and_refuses_identically() {
    both("rpm ceiling", |store| async move {
        let limits = rpm(2);
        let mut out = Vec::new();
        for i in 0..4 {
            let member = format!("m{i}");
            let verdict = store.acquire("k-ceiling", &limits, &member).await;
            out.push(format!("acquire{i}={}", verdict.is_ok()));
        }
        out
    })
    .await;
}

#[tokio::test]
async fn peek_reports_the_same_remaining_after_the_same_traffic() {
    both("peek", |store| async move {
        let limits = rpm(5);
        let mut out = Vec::new();
        for i in 0..3 {
            let _ = store.acquire("k-peek", &limits, &format!("m{i}")).await;
        }
        let status = store.peek("k-peek", &limits).await;
        out.push(format!(
            "rpm_remaining={:?}",
            status.and_then(|s| s.rpm_remaining())
        ));
        out
    })
    .await;
}

#[tokio::test]
async fn a_one_shot_claim_is_granted_exactly_once() {
    both("claim_once", |store| async move {
        let mut out = Vec::new();
        for i in 0..3 {
            out.push(format!(
                "claim{i}={}",
                store.claim_once("k-claim", Duration::from_secs(60)).await
            ));
        }
        out.push(format!(
            "other={}",
            store
                .claim_once("k-claim-other", Duration::from_secs(60))
                .await
        ));

        // Expiry TIMING is deliberately not compared here. The local
        // deadline is floored unix SECONDS while `EX` is real time, so the
        // local window can lapse up to a second early — a genuine granularity
        // difference, not a semantic one, and pinning it with real sleeps
        // produces a test that fails under load rather than one that finds
        // bugs. The property that actually went wrong once — a DENIED claim
        // extending the deadline — is pinned exactly, on the injectable
        // clock, in `a_denied_claim_never_extends_the_deadline` below.
        out
    })
    .await;
}

/// A denied claim must not extend the marker's life.
///
/// This is the divergence that actually shipped: the local store rewrote the
/// deadline on every call, so a caller that kept asking kept the marker alive
/// forever, while `SET NX` leaves an existing key's TTL alone. It guards a
/// non-idempotent side effect — a second charge — so "held forever" and "held
/// for the TTL" are different amounts of money.
///
/// Local-only and on the injectable clock, because that is what makes it
/// exact: no sleeps, no jitter, and it runs with no Redis at all.
#[tokio::test]
async fn a_denied_claim_never_extends_the_deadline() {
    use aisix_ratelimit::TestClock;

    let clock = TestClock::new(1_000);
    let store = LocalStore::with_clock(clock.clone());
    let ttl = Duration::from_secs(10);

    assert!(store.claim_once("k", ttl).await, "first claim is granted");
    // Ask repeatedly through the window. Every answer must be a refusal, and
    // none of them may push the deadline out.
    for step in 1..=9 {
        clock.advance(1);
        assert!(
            !store.claim_once("k", ttl).await,
            "claim at +{step}s is inside the window and must be refused"
        );
    }
    clock.advance(2);
    assert!(
        store.claim_once("k", ttl).await,
        "the window lapsed at +10s; the nine refusals in between must not \
         have extended it"
    );
}

/// A sub-second ttl must still hold the marker for at least one tick.
///
/// `as_secs()` TRUNCATES, so any ttl under a second became a deadline of
/// `now + 0` — already lapsed, swept by the `retain` at the top of the next
/// call, and the guard granted every single time. Redis never had this: it
/// clamps, because `EX 0` is not a valid expiry. The local side did not, so
/// the two backends answered differently for the same argument and the
/// no-double-charge guard silently became a no-op on one of them.
///
/// On the injectable clock with no advance at all, so it is the truncation
/// being tested and not a race.
#[tokio::test]
async fn a_sub_second_ttl_still_holds_the_marker() {
    use aisix_ratelimit::TestClock;

    let store = LocalStore::with_clock(TestClock::new(1_000));
    let ttl = Duration::from_millis(600);
    assert!(store.claim_once("k", ttl).await, "first claim is granted");
    assert!(
        !store.claim_once("k", ttl).await,
        "a ttl under a second must not round down to an already-lapsed \
         deadline — that turns the one-shot guard into no guard"
    );
}

/// A cumulative allowance survives bucket reclamation on BOTH stores.
///
/// `granted_micro_usd` has no window and never rolls, so anything that drops
/// it hands the caller the whole allowance back. Redis says so in the Lua
/// that writes the counter — no EXPIRE, because an expiry "degrades it into a
/// cap" — while the local store reclaimed the whole bucket on idle, which is
/// that expiry by another name. Nothing compared the two.
#[tokio::test]
async fn a_cumulative_allowance_survives_reaping_on_both_stores() {
    both("cumulative allowance vs reap", |store| async move {
        let limits = RateLimit {
            granted_micro_usd: Some(1_000),
            ..RateLimit::default()
        };
        let mut out = Vec::new();
        store
            .acquire("k-granted", &limits, "m")
            .await
            .expect("first call is inside the allowance");
        // Spend past it. `commit`'s number is what the layer counts, and for
        // the spend layer that is micro-USD — committing 0 would consume
        // nothing and leave this whole case proving nothing.
        store.commit("k-granted", 1_500, "m").await;
        out.push(format!(
            "after_spend={}",
            store.acquire("k-granted", &limits, "m").await.is_ok()
        ));
        // Reclaim idle buckets, as the gateway does every five minutes.
        //
        // The sleep is load-bearing and is NOT a tight timing assertion:
        // `reap` compares `last_touched >= now - idle_for`, and the commit
        // above set `last_touched` to the current second — so reaping in the
        // same second reclaims nothing and this case would pass no matter
        // what the store does. Waiting past a second-boundary makes the
        // bucket genuinely idle. A slower machine only makes it more idle,
        // so there is nothing here to flake.
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        store.reap(Duration::from_secs(0));
        out.push(format!(
            "after_reap={}",
            store.acquire("k-granted", &limits, "m").await.is_ok()
        ));
        out
    })
    .await;
}

/// Concurrency admits up to the limit, and a release frees exactly one slot.
///
/// The two implementations are structurally different here — local keeps an
/// in-flight counter, Redis keeps a ZSET of leased members — which is exactly
/// the shape where they can drift apart while each side's own tests pass.
#[tokio::test]
async fn concurrency_admits_and_releases_identically() {
    both("concurrency", |store| async move {
        let limits = RateLimit {
            concurrency: Some(2),
            ..RateLimit::default()
        };
        let mut out = Vec::new();
        for i in 0..3 {
            out.push(format!(
                "acquire{i}={}",
                store
                    .acquire("k-conc", &limits, &format!("m{i}"))
                    .await
                    .is_ok()
            ));
        }
        // Freeing one slot admits exactly one more.
        //
        // `RedisStore::release` is a SYNC trait method that spawns its `ZREM`,
        // because releasing a shared slot needs I/O it cannot await here — so
        // the freed slot becomes visible a moment later. That is a deliberate
        // difference in TIMING, not in semantics, and asserting immediately
        // just races it. Yielding until the spawned task has run keeps this
        // case about what it is for: a release frees exactly one slot, not
        // zero and not two.
        store.release("k-conc", "m0");
        tokio::time::sleep(Duration::from_millis(200)).await;
        out.push(format!(
            "after_release={}",
            store.acquire("k-conc", &limits, "m-new").await.is_ok()
        ));
        out.push(format!(
            "and_no_more={}",
            store.acquire("k-conc", &limits, "m-extra").await.is_ok()
        ));
        out
    })
    .await;
}

/// Releasing a member that never held a slot must not free someone else's.
///
/// An implementation that decrements a bare counter on any release lets a
/// caller hand back slots it never took, opening the limit indefinitely; one
/// that removes a named member from a set cannot. Both must behave like the
/// second.
#[tokio::test]
async fn releasing_an_unknown_member_frees_nothing() {
    both("release unknown", |store| async move {
        let limits = RateLimit {
            concurrency: Some(1),
            ..RateLimit::default()
        };
        let mut out = Vec::new();
        out.push(format!(
            "held={}",
            store.acquire("k-rel", &limits, "holder").await.is_ok()
        ));
        for i in 0..3 {
            store.release("k-rel", &format!("never-held-{i}"));
        }
        out.push(format!(
            "admitted_another={}",
            store.acquire("k-rel", &limits, "other").await.is_ok()
        ));
        out
    })
    .await;
}
