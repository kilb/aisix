use super::*;
use crate::TestClock;

// 本地没有续租源；无意义的 forwarder 会让这个保留 sender 多出一个 receiver。
#[tokio::test]
async fn local_concurrency_hold_does_not_start_a_lease_loss_watcher() {
    let limiter = Limiter::local_with_clock(TestClock::new(100));
    let limits = RateLimit {
        concurrency: Some(1),
        ..Default::default()
    };
    let reservation = limiter.pre_commit("local", &limits).await.unwrap();
    let source = reservation.lease_loss.clone();
    let hold = MultiReservation::new(vec![reservation]).into_stream_hold();

    assert_eq!(source.receiver_count(), 0, "本地并发槽没有远程租约可监听");
    assert!(matches!(
        limiter.pre_commit("local", &limits).await,
        Err(RateLimitError::Concurrency { .. })
    ));
    drop(hold);
    assert_eq!(limiter.peek("local", &limits).await.unwrap().in_flight, 0);
    assert!(limiter.pre_commit("local", &limits).await.is_ok());
}

// 只限窗口仍登记本地 member；省 watcher 不能顺便省掉它的 release 或 token 结算。
#[tokio::test]
async fn window_only_hold_does_not_watch_a_lease_and_still_cleans_its_local_member() {
    let limiter = Limiter::local_with_clock(TestClock::new(100));
    let limits = RateLimit {
        rpm: Some(10),
        tpm: Some(100),
        ..Default::default()
    };
    let reservation = limiter.pre_commit("window", &limits).await.unwrap();
    let source = reservation.lease_loss.clone();
    let reservation = MultiReservation::new(vec![reservation]);
    let keys = reservation.keys();
    let hold = reservation.into_stream_hold();

    assert_eq!(source.receiver_count(), 0, "窗口预留没有续租源");
    assert_eq!(limiter.peek("window", &limits).await.unwrap().in_flight, 1);
    limiter.add_tokens_post_stream_all(&keys, 7);
    drop(hold);
    let status = limiter.peek("window", &limits).await.unwrap();
    assert_eq!(
        status.in_flight, 0,
        "没有 concurrency 也必须清理本地 member"
    );
    assert_eq!(status.rpm_used, 1);
    assert_eq!(status.tpm_used, 7, "清理成员不能重复扣 token");
}

// 任意一层的真实 Redis 租约丢失都必须传到流持有者，不能只监听第一层。
#[tokio::test]
async fn redis_multilayer_hold_keeps_each_real_lease_loss_forwarder() {
    let url = match std::env::var("RATELIMIT_TEST_REDIS_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            assert!(std::env::var("CI").is_err(), "CI 必须配置真实 Redis");
            return;
        }
    };
    let cfg = aisix_core::RedisConnConfig {
        url: Some(url.clone()),
        ..Default::default()
    };
    let store = Arc::new(
        crate::RedisStore::connect(&cfg)
            .await
            .unwrap()
            .with_conc_ttl(1),
    );
    let limiter = Limiter::with_store(store);
    let limits = RateLimit {
        concurrency: Some(1),
        ..Default::default()
    };
    let first_key = format!("lease-first:{}", uuid::Uuid::new_v4());
    let second_key = format!("lease-second:{}", uuid::Uuid::new_v4());
    let first = limiter.pre_commit(&first_key, &limits).await.unwrap();
    let second = limiter.pre_commit(&second_key, &limits).await.unwrap();
    let first_source = first.lease_loss.clone();
    let second_source = second.lease_loss.clone();
    let hold = MultiReservation::new(vec![first, second]).into_stream_hold();
    assert_eq!(first_source.receiver_count(), 1);
    assert_eq!(second_source.receiver_count(), 1);
    let mut lost = hold.lease_loss_receiver();

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let removed: u64 = redis::cmd("DEL")
        .arg(format!(
            "{}:{{{second_key}}}:conc",
            crate::store::redis::DEFAULT_PREFIX
        ))
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(removed, 1, "先证明被删除的是实际取得的共享租约");
    tokio::time::timeout(Duration::from_secs(3), lost.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(*lost.borrow_and_update());
    assert!(!*first_source.borrow(), "第二层丢失不能被误记在第一层");

    drop(hold);
    tokio::time::timeout(Duration::from_secs(1), async {
        while first_source.receiver_count() != 0 || second_source.receiver_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("流结束后必须撤销所有租约监听");
}
