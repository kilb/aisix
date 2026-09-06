//! The two `Cache` implementations must answer the same way.
//!
//! `MemoryCache` caches per replica, `RedisCache` caches across them, and the
//! gateway swaps one for the other on a config flag — so a caller's
//! experience is supposed to differ only in SCOPE, never in semantics. There
//! was no test holding them to that: the trait has five methods and two
//! implementations, each with its own test module asserting what its own
//! author expected.
//!
//! This is the same suite shape that found a real divergence in `RateStore`'s
//! two backends on its first run.
//!
//! Needs only a plain `redis-server` — the exact cache is GET/SET, no modules
//! — and says so rather than passing silently when there is none.

#![cfg(feature = "redis")]

use aisix_cache::{Cache, CachedBody, MemoryCache, RedisCache};
use aisix_core::config::{RedisConnConfig, RedisMode};
use aisix_gateway::{ChatMessage, ChatResponse, FinishReason, UsageStats};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static NS: AtomicU32 = AtomicU32::new(0);

struct Redis {
    owned: Option<(Child, std::path::PathBuf)>,
    url: String,
}

impl Redis {
    fn start() -> Option<Self> {
        if let Ok(url) = std::env::var("CACHE_TEST_REDIS_URL") {
            if !url.trim().is_empty() {
                return Some(Self {
                    owned: None,
                    url: url.trim().to_string(),
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
        let dir = std::env::temp_dir().join(format!("aisix-cache-conformance-{port}"));
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

fn response(text: &str) -> ChatResponse {
    ChatResponse {
        id: "resp-1".into(),
        model: "m".into(),
        message: ChatMessage::assistant(text.to_string()),
        finish_reason: FinishReason::Stop,
        usage: UsageStats::default(),
    }
}

/// Run `scenario` against both caches and require the same transcript, so a
/// divergence names WHAT differed rather than just which backend failed.
async fn both<F, Fut>(label: &str, scenario: F)
where
    F: Fn(std::sync::Arc<dyn Cache>, String) -> Fut,
    Fut: std::future::Future<Output = Vec<String>>,
{
    let Some(redis) = Redis::start() else {
        eprintln!("skipping {label}: no CACHE_TEST_REDIS_URL and no redis-server");
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
    let shared: std::sync::Arc<dyn Cache> =
        std::sync::Arc::new(RedisCache::connect(&cfg).await.expect("connect"));
    let local: std::sync::Arc<dyn Cache> =
        std::sync::Arc::new(MemoryCache::new(Duration::from_secs(60), 128));

    let ns = NS.fetch_add(1, Ordering::Relaxed);
    let from_local = scenario(local, format!("conf-{}-{ns}-mem", std::process::id())).await;
    let from_redis = scenario(shared, format!("conf-{}-{ns}-red", std::process::id())).await;
    assert_eq!(
        from_local, from_redis,
        "{label}: the two caches disagree — a caller's experience should differ \
         between them only in SCOPE (per-replica vs shared), never in semantics"
    );
}

#[tokio::test]
async fn a_miss_a_put_and_a_hit_read_the_same() {
    both("chat round trip", |cache, k| async move {
        let mut out = Vec::new();
        out.push(format!(
            "before={:?}",
            cache.get(&k).await.map(|v| v.is_some())
        ));
        cache.put(&k, response("hello")).await.expect("put");
        let got = cache.get(&k).await.expect("get");
        out.push(format!(
            "after={:?}",
            got.map(|r| r.message.content_str().to_string())
        ));
        out.push(format!(
            "other_key={:?}",
            cache.get(&format!("{k}-absent")).await.map(|v| v.is_some())
        ));
        out
    })
    .await;
}

#[tokio::test]
async fn a_second_put_replaces_the_first() {
    both("overwrite", |cache, k| async move {
        cache.put(&k, response("first")).await.expect("put");
        cache.put(&k, response("second")).await.expect("put again");
        let got = cache.get(&k).await.expect("get");
        vec![format!(
            "{:?}",
            got.map(|r| r.message.content_str().to_string())
        )]
    })
    .await;
}

#[tokio::test]
async fn a_body_round_trips_with_its_content_type() {
    both("body round trip", |cache, k| async move {
        let mut out = Vec::new();
        out.push(format!(
            "before={:?}",
            cache.get_body(&k).await.map(|v| v.is_some())
        ));
        cache
            .put_body_with_ttl(
                &k,
                CachedBody {
                    content_type: "application/json".into(),
                    body: b"{\"ok\":true}".to_vec(),
                    prompt_tokens: 7,
                    completion_tokens: 3,
                },
                Duration::from_secs(60),
            )
            .await
            .expect("put body");
        let got = cache.get_body(&k).await.expect("get body");
        out.push(format!(
            "after={:?}",
            got.map(|b| (
                b.content_type,
                String::from_utf8_lossy(&b.body).into_owned(),
                b.prompt_tokens,
                b.completion_tokens,
            ))
        ));
        out
    })
    .await;
}

/// A chat entry and a body entry under the SAME key must not read each
/// other: the two families are stored by different methods and a backend
/// that shared one slot would hand a caller the wrong shape entirely.
#[tokio::test]
async fn the_chat_and_body_families_do_not_read_each_other() {
    both("family isolation", |cache, k| async move {
        let mut out = Vec::new();
        cache.put(&k, response("chat shaped")).await.expect("put");
        out.push(format!(
            "body_after_chat_put={:?}",
            cache.get_body(&k).await.map(|v| v.is_some())
        ));
        cache
            .put_body_with_ttl(
                &format!("{k}-b"),
                CachedBody {
                    content_type: "text/plain".into(),
                    body: b"body shaped".to_vec(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                },
                Duration::from_secs(60),
            )
            .await
            .expect("put body");
        out.push(format!(
            "chat_after_body_put={:?}",
            cache.get(&format!("{k}-b")).await.map(|v| v.is_some())
        ));
        out
    })
    .await;
}
