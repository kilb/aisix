use super::*;

fn detached_store() -> RedisStore {
    RedisStore::with_slot(ConnSlot::empty(), Arc::new(AtomicBool::new(false))).unwrap()
}

// current_thread 中两次采样之间没有 await；多出的 release spawn 不可能先自行退出。
#[tokio::test(flavor = "current_thread")]
async fn release_without_redis_ownership_cleans_the_local_member_without_spawning() {
    let store = detached_store();
    let limits = RateLimit {
        rpm: Some(10),
        ..Default::default()
    };
    store.acquire("window", &limits, "member").await.unwrap();
    assert!(store.lease_members.is_empty());
    assert_eq!(
        store.local.peek("window", &limits).await.unwrap().in_flight,
        1
    );

    let runtime = tokio::runtime::Handle::current().metrics();
    let tasks = runtime.num_alive_tasks();
    store.release("window", "member");
    assert_eq!(
        runtime.num_alive_tasks(),
        tasks,
        "没有 Redis 租约不能派发清理任务"
    );
    let status = store.local.peek("window", &limits).await.unwrap();
    assert_eq!(status.in_flight, 0);
    assert_eq!(status.rpm_used, 1, "release 不得回退请求窗口");
}

#[tokio::test(flavor = "current_thread")]
async fn local_only_release_cleans_the_fallback_slot_without_spawning() {
    let store = detached_store();
    let limits = RateLimit {
        concurrency: Some(1),
        ..Default::default()
    };
    store
        .acquire("local-only", &limits, "member")
        .await
        .unwrap();
    assert_eq!(
        *store
            .lease_members
            .get(&("local-only".into(), "member".into()))
            .unwrap(),
        LeaseOwnership::LocalOnly
    );
    assert!(store
        .local
        .acquire("local-only", &limits, "blocked")
        .await
        .is_err());

    let runtime = tokio::runtime::Handle::current().metrics();
    let tasks = runtime.num_alive_tasks();
    store.release("local-only", "member");
    assert_eq!(
        runtime.num_alive_tasks(),
        tasks,
        "本地取得的槽不需要远程 ZREM"
    );
    assert!(store.lease_members.is_empty());
    assert_eq!(
        store
            .local
            .peek("local-only", &limits)
            .await
            .unwrap()
            .in_flight,
        0
    );
    assert!(store
        .local
        .acquire("local-only", &limits, "next")
        .await
        .is_ok());
    store.local.release("local-only", "next");
}
