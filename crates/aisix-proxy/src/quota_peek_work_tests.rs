use super::*;
use aisix_core::{AisixSnapshot, ApiKey, Model, ResourceEntry};
use aisix_ratelimit::{LocalStore, RateLimitError, RateLimitStatus, RateStore, TestClock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// 只在真实 LocalStore 的 peek 调用处计数，其余限流状态变化完整委托给原后端。
struct PeekCountStore {
    inner: LocalStore<TestClock>,
    peeks: AtomicUsize,
}

#[axum::async_trait]
impl RateStore for PeekCountStore {
    async fn acquire(
        &self,
        key: &str,
        limits: &RateLimit,
        member: &str,
    ) -> Result<(), RateLimitError> {
        self.inner.acquire(key, limits, member).await
    }

    async fn commit(&self, key: &str, tokens: u64, member: &str) {
        self.inner.commit(key, tokens, member).await;
    }

    fn release(&self, key: &str, member: &str) {
        self.inner.release(key, member);
    }

    fn add_tokens(&self, key: &str, tokens: u64) {
        self.inner.add_tokens(key, tokens);
    }

    async fn peek(&self, key: &str, limits: &RateLimit) -> Option<RateLimitStatus> {
        let status = self.inner.peek(key, limits).await;
        self.peeks.fetch_add(1, Ordering::Relaxed);
        status
    }

    async fn claim_once(&self, key: &str, ttl: Duration) -> bool {
        self.inner.claim_once(key, ttl).await
    }
}

struct Fixture {
    state: ProxyState,
    snapshot: AisixSnapshot,
    auth: AuthenticatedKey,
    model: ModelRateLimit,
    store: Arc<PeekCountStore>,
    clock: TestClock,
}

fn fixture(limits: RateLimit) -> Fixture {
    let snapshot = AisixSnapshot::new();
    let model: Model = serde_json::from_value(serde_json::json!({
        "display_name": "test-model", "provider": "openai",
        "model_name": "upstream-model", "provider_key_id": "provider-key",
    }))
    .unwrap();
    let model_limits = ModelRateLimit::from_model("test-model", "model-id", &model);
    snapshot
        .models
        .insert(ResourceEntry::new("model-id", model, 1));
    let key: ApiKey = serde_json::from_value(serde_json::json!({
        "key_hash": ApiKey::hash_bearer("test-token"),
        "display_name": "test-key", "allowed_models": ["test-model"],
        "rate_limit": limits,
    }))
    .unwrap();
    let entry = Arc::new(ResourceEntry::new("key-id", key, 1));
    snapshot.apikeys.insert_arc(Arc::clone(&entry));
    let auth = AuthenticatedKey::new(&snapshot, entry, None, false);
    let clock = TestClock::new(1_000);
    let store = Arc::new(PeekCountStore {
        inner: LocalStore::with_clock(clock.clone()),
        peeks: AtomicUsize::new(0),
    });
    let state = ProxyState::with_limiter(
        aisix_core::snapshot::SnapshotHandle::new(snapshot.clone()),
        Arc::new(aisix_gateway::Hub::new()),
        Arc::new(aisix_ratelimit::Limiter::with_store(store.clone())),
        &serde_json::from_value(serde_json::json!({"addr": "127.0.0.1:0"})).unwrap(),
    );
    Fixture {
        state,
        snapshot,
        auth,
        model: model_limits,
        store,
        clock,
    }
}

async fn drain_spawned_tasks(before: usize) {
    let runtime = tokio::runtime::Handle::current().metrics();
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.num_alive_tasks() > before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("本地准入的遥测任务必须实际完成，不能只看到尚未 poll 的零计数");
}

async fn publish(f: &Fixture) -> axum::response::Response {
    let mut response = axum::response::Response::new(axum::body::Body::empty());
    crate::request_metrics::publish_rate_limit_window(
        &f.state.metrics,
        &f.state.limiter,
        &f.snapshot,
        &f.auth.entry.id,
        f.auth.key().rate_limit.as_ref().unwrap(),
        "test-model",
        "upstream-model",
        &mut response,
    )
    .await;
    response
}

fn unpublished_limits() -> [(&'static str, RateLimit); 5] {
    [
        (
            "rps",
            RateLimit {
                rps: Some(1),
                ..Default::default()
            },
        ),
        (
            "rph",
            RateLimit {
                rph: Some(1),
                ..Default::default()
            },
        ),
        (
            "rpd",
            RateLimit {
                rpd: Some(1),
                ..Default::default()
            },
        ),
        (
            "tph",
            RateLimit {
                tph: Some(1),
                ..Default::default()
            },
        ),
        (
            "tpd",
            RateLimit {
                tpd: Some(1),
                ..Default::default()
            },
        ),
    ]
}

fn gauge_values(f: &Fixture, family: &str) -> Vec<f64> {
    f.state
        .metrics
        .render()
        .lines()
        .filter(|line| line.starts_with(&format!("{family}{{")))
        .map(|line| line.rsplit_once(' ').unwrap().1.parse().unwrap())
        .collect()
}

// A 只发布 RPM/TPM gauges；普通和 WS 的 split 准入都不该为其它维度空读。
#[tokio::test(flavor = "current_thread")]
async fn admission_without_minute_gauge_consumers_does_not_peek_or_spawn() {
    let mut cases = unpublished_limits().to_vec();
    cases.push((
        "concurrency",
        RateLimit {
            concurrency: Some(1),
            ..Default::default()
        },
    ));
    for (dimension, limits) in cases {
        for split in [false, true] {
            let f = fixture(limits.clone());
            let runtime = tokio::runtime::Handle::current().metrics();
            let before = runtime.num_alive_tasks();
            let reservation = if split {
                let (mut request, model) =
                    super::enforce_split(&f.state, &f.snapshot, &f.auth, &f.model)
                        .await
                        .unwrap();
                request.merge(model);
                request
            } else {
                super::enforce(&f.state, &f.snapshot, &f.auth, Some(&f.model))
                    .await
                    .unwrap()
            };
            let spawned = runtime.num_alive_tasks() - before;
            drain_spawned_tasks(before).await;
            let peeks = f.store.peeks.load(Ordering::Relaxed);
            drop(reservation);
            assert_eq!(peeks, 0, "{dimension}/split={split}: 没有准入 gauge 消费者");
            assert_eq!(spawned, 0, "{dimension}/split={split}: 不应派发空读任务");
            assert!(gauge_values(&f, "aisix_ratelimit_remaining_requests").is_empty());
            assert!(gauge_values(&f, "aisix_ratelimit_remaining_tokens").is_empty());
        }
    }
}

// B 只发布 RPM/TPM/concurrency；其它窗口不产生任何头或 gauge，不能为它读后端。
#[tokio::test(flavor = "current_thread")]
async fn response_without_any_published_window_does_not_peek() {
    for (dimension, limits) in unpublished_limits() {
        let f = fixture(limits.clone());
        let reservation = f
            .state
            .limiter
            .pre_commit(&f.auth.entry.id, &limits)
            .await
            .unwrap();
        let response = publish(&f).await;
        assert_eq!(
            f.store.peeks.load(Ordering::Relaxed),
            0,
            "{dimension}: 空读应消失"
        );
        assert!(!response
            .headers()
            .keys()
            .any(|name| name.as_str().starts_with("x-ratelimit-")));
        assert!(gauge_values(&f, "aisix_ratelimit_remaining_requests").is_empty());
        assert!(gauge_values(&f, "aisix_ratelimit_remaining_tokens").is_empty());
        drop(reservation);
    }
}

// 省掉的是观测空读，不是这些窗口真正的准入检查；token 窗口仍在 commit 后拒绝。
#[tokio::test(flavor = "current_thread")]
async fn unpublished_window_dimensions_still_reject_after_their_limit_is_spent() {
    for (dimension, limits) in unpublished_limits() {
        let f = fixture(limits);
        let reservation = super::enforce(&f.state, &f.snapshot, &f.auth, Some(&f.model))
            .await
            .unwrap();
        reservation
            .commit_tokens(if dimension.starts_with('t') { 1 } else { 0 })
            .await;
        let error = super::enforce(&f.state, &f.snapshot, &f.auth, Some(&f.model))
            .await
            .unwrap_err();
        let ProxyError::RateLimit(error) = error else {
            panic!("{dimension}: 应是 key 的限流拒绝")
        };
        assert_eq!(error.detail().dimension, dimension);
        assert_eq!(error.detail().limit, 1);
        assert_eq!(error.detail().remaining, 0);
    }
}

// 有消费者的两次读不能合并：提交 token、别的请求以及并发槽变化必须到达新的响应头。
#[tokio::test(flavor = "current_thread")]
async fn response_window_stays_fresh_after_commit_and_another_request() {
    let limits = RateLimit {
        rpm: Some(10),
        tpm: Some(100),
        concurrency: Some(3),
        ..Default::default()
    };
    let f = fixture(limits.clone());
    let first = f
        .state
        .limiter
        .pre_commit(&f.auth.entry.id, &limits)
        .await
        .unwrap();
    let response = publish(&f).await;
    assert_eq!(response.headers()["x-ratelimit-remaining-requests"], "9");
    assert_eq!(response.headers()["x-ratelimit-remaining-tokens"], "100");
    assert_eq!(response.headers()["x-ratelimit-remaining-concurrent"], "2");
    first.commit_tokens(7).await;
    f.state
        .limiter
        .pre_commit(&f.auth.entry.id, &limits)
        .await
        .unwrap()
        .commit_tokens(5)
        .await;
    let response = publish(&f).await;
    assert_eq!(response.headers()["x-ratelimit-remaining-requests"], "8");
    assert_eq!(response.headers()["x-ratelimit-remaining-tokens"], "88");
    assert_eq!(response.headers()["x-ratelimit-remaining-concurrent"], "3");
    assert_eq!(
        gauge_values(&f, "aisix_ratelimit_remaining_requests"),
        [8.0]
    );
    assert_eq!(gauge_values(&f, "aisix_ratelimit_remaining_tokens"), [88.0]);
    assert_eq!(f.store.peeks.load(Ordering::Relaxed), 2);

    f.clock.advance(60);
    let response = publish(&f).await;
    assert_eq!(response.headers()["x-ratelimit-remaining-requests"], "10");
    assert_eq!(response.headers()["x-ratelimit-remaining-tokens"], "100");
    assert_eq!(
        f.store.peeks.load(Ordering::Relaxed),
        3,
        "新分钟必须重新读，不能复用旧状态"
    );
}

// WS 没有普通 handler 的尾部 publisher；流持有期间的分钟 gauges 仍要在准入后可见。
#[tokio::test(flavor = "current_thread")]
async fn visible_admission_gauges_remain_available_during_a_stream_or_websocket_hold() {
    let f = fixture(RateLimit {
        rpm: Some(10),
        tpm: Some(100),
        ..Default::default()
    });
    let before = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    let (mut reservation, model) = super::enforce_split(&f.state, &f.snapshot, &f.auth, &f.model)
        .await
        .unwrap();
    reservation.merge(model);
    let hold = reservation.into_stream_hold();
    drain_spawned_tasks(before).await;
    assert_eq!(f.store.peeks.load(Ordering::Relaxed), 1);
    assert_eq!(
        gauge_values(&f, "aisix_ratelimit_remaining_requests"),
        [9.0]
    );
    assert_eq!(
        gauge_values(&f, "aisix_ratelimit_remaining_tokens"),
        [100.0]
    );
    drop(hold);
}

// Some(0) 是实际配置，不是缺失：拒绝后的 A gauge 与 B 响应头必须仍然读取并发布零。
#[tokio::test(flavor = "current_thread")]
async fn zero_valued_published_limits_are_not_treated_as_absent() {
    for (dimension, limits, header) in [
        (
            "rpm",
            RateLimit {
                rpm: Some(0),
                ..Default::default()
            },
            "requests",
        ),
        (
            "tpm",
            RateLimit {
                tpm: Some(0),
                ..Default::default()
            },
            "tokens",
        ),
        (
            "concurrency",
            RateLimit {
                concurrency: Some(0),
                ..Default::default()
            },
            "concurrent",
        ),
    ] {
        let f = fixture(limits);
        let before = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        let result = super::enforce(&f.state, &f.snapshot, &f.auth, Some(&f.model)).await;
        assert!(
            matches!(result, Err(ProxyError::RateLimit(_))),
            "{dimension}: 零上限必须拒绝"
        );
        drain_spawned_tasks(before).await;
        if dimension != "concurrency" {
            assert_eq!(f.store.peeks.load(Ordering::Relaxed), 1);
            assert_eq!(
                gauge_values(&f, &format!("aisix_ratelimit_remaining_{header}")),
                [0.0]
            );
        }
        let count = f.store.peeks.load(Ordering::Relaxed);
        let response = publish(&f).await;
        assert_eq!(f.store.peeks.load(Ordering::Relaxed), count + 1);
        assert_eq!(
            response.headers()[format!("x-ratelimit-limit-{header}")],
            "0"
        );
        assert_eq!(
            response.headers()[format!("x-ratelimit-remaining-{header}")],
            "0"
        );
    }
}
