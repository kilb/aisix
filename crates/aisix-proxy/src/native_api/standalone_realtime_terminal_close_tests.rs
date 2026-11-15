// 关闭只收束已接受观察，不生成零量事件或为已关闭会话刷新租约。

#[tokio::test]
async fn terminal_empty_close_after_expiry_is_unknown_without_creating_usage_or_renewing() {
    let f = Fixture::new().await;
    let (source, live) = short_session(&f).await;
    assert!(journal(&f.store, source.operation_id).await.is_empty());
    let before = fence(&f.store, source.operation_id).await;
    naturally_expire(&live).await;
    let revoked = (*f.snapshot.load()).clone();
    revoked.apikeys.remove("key-a");
    f.snapshot.store(revoked);
    let mut close = checkpoint(&live, true);
    close.coverage = UsageCoverage::Unknown;
    assert!(close.entries.is_empty());
    let reply = f
        .store
        .checkpoint_live_session(close.clone())
        .await
        .unwrap();
    let closed = reply.live_session.unwrap();
    assert_eq!(closed.state, "closed");
    assert_eq!(closed.coverage, UsageCoverage::Unknown);
    assert_eq!(closed.accounting_state, "awaiting_reconciliation");
    assert_eq!(closed.lease, live.lease);
    assert_eq!(closed.lease_expires_at, live.lease_expires_at);
    assert_eq!(closed.deadline_at, live.deadline_at);
    assert!(journal(&f.store, source.operation_id).await.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM native_terminal_outbox")
            .fetch_one(&f.store.pool)
            .await
            .unwrap(),
        0
    );
    let after = fence(&f.store, source.operation_id).await;
    assert_eq!(after.0, before.0);
    assert_eq!(after.1, before.1);
    assert_eq!(after.2, before.2);
    assert_eq!(after.3, "closed");
    assert!(after.4.is_some());
    assert_eq!(after.5, before.5);
    assert_eq!(after.6, before.6);
    let replay = f.store.checkpoint_live_session(close).await.unwrap();
    assert_eq!(replay.live_session.unwrap().state, "closed");
    assert_eq!(fence(&f.store, source.operation_id).await, after);
    assert!(journal(&f.store, source.operation_id).await.is_empty());
}

#[tokio::test]
async fn terminal_empty_close_preserves_existing_nonzero_facts_without_extending_a_live_lease() {
    let f = Fixture::new().await;
    let (source, live) = created(&f, &f.store, None).await;
    assert!(live.deadline_at.unwrap() > live.lease_expires_at.unwrap());
    let known = reported_tail(&source, &live);
    assert_eq!(
        f.store.append_live_usage(known).await.unwrap().appended,
        Some(true)
    );
    let accepted = journal(&f.store, source.operation_id).await;
    assert_eq!(accepted.len(), 1);
    let usage: serde_json::Value = serde_json::from_str(&accepted[0].2).unwrap();
    assert_eq!(
        usage.pointer("/0/meter/usage/prompt_tokens"),
        Some(&json!(13))
    );
    assert_eq!(
        usage.pointer("/0/meter/usage/completion_tokens"),
        Some(&json!(5))
    );
    let before = fence(&f.store, source.operation_id).await;
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(Utc::now() < live.lease_expires_at.unwrap());
    let mut close = checkpoint(&live, true);
    close.coverage = UsageCoverage::Unknown;
    let reply = f
        .store
        .checkpoint_live_session(close.clone())
        .await
        .unwrap();
    let closed = reply.live_session.unwrap();
    assert_eq!(closed.state, "closed");
    assert_eq!(closed.coverage, UsageCoverage::Unknown);
    assert_eq!(closed.accounting_state, "awaiting_reconciliation");
    assert_eq!(closed.lease, live.lease);
    assert_eq!(
        closed.lease_expires_at, live.lease_expires_at,
        "关闭确认不能把新心跳的 TTL 写回已关闭会话"
    );
    assert_eq!(journal(&f.store, source.operation_id).await, accepted);
    let after = fence(&f.store, source.operation_id).await;
    assert_eq!(after.0, before.0);
    assert_eq!(after.1, before.1);
    assert_eq!(after.2, before.2);
    assert_eq!(after.5, before.5);
    assert_eq!(after.6, before.6);
    assert_eq!(
        f.store
            .checkpoint_live_session(close)
            .await
            .unwrap()
            .live_session
            .unwrap()
            .state,
        "closed"
    );
    assert_eq!(fence(&f.store, source.operation_id).await, after);
    assert_eq!(journal(&f.store, source.operation_id).await, accepted);
}
