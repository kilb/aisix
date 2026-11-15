//! 通过合法短 deadline 自然到期；已接受尾量不变成新工作或新租约。

type Fence = (String, i64, i64, String, Option<i64>, String, String);
type JournalRow = (String, Option<String>, String, Option<String>, String, i64);

async fn short_session(f: &Fixture) -> (BeginRequest, LiveSessionRecord) {
    let source = accepted_live(f, &f.store).await;
    let deadline = Utc::now() + chrono::Duration::seconds(4);
    let reply = f
        .store
        .create_live_session(CreateLiveSessionRequest {
            operation_id: source.operation_id,
            api_key_id: source.api_key_id,
            purpose: RealtimePurpose::Realtime,
            transport: RealtimeTransport::WebSocket,
            credential_id: None,
            provider_session_id: Some("sess.expiry-tail".into()),
            deadline_at: deadline,
        })
        .await
        .unwrap();
    let live = reply.live_session.unwrap();
    assert_eq!(live.state, "live");
    assert_eq!(live.operation_id, source.operation_id);
    assert_eq!(live.api_key_id, source.api_key_id);
    assert_eq!(live.deadline_at, Some(deadline));
    let expiry = live.lease_expires_at.unwrap();
    assert!(expiry > Utc::now());
    assert!(expiry <= deadline);
    assert_eq!(live.lease.as_ref().unwrap().generation, 1);
    (source, live)
}

async fn naturally_expire(live: &LiveSessionRecord) {
    let expiry = live.lease_expires_at.unwrap();
    let remaining = (expiry - Utc::now()).to_std().unwrap_or_default();
    assert!(remaining <= std::time::Duration::from_secs(4));
    tokio::time::sleep(remaining + std::time::Duration::from_millis(25)).await;
    assert!(Utc::now() > expiry);
}

async fn fence(store: &StandaloneNativeStore, operation: Uuid) -> Fence {
    sqlx::query_as(
        "SELECT lease_worker,lease_generation,lease_expires_at,state,closed_at,deadline_at,owner \
         FROM native_live_sessions WHERE operation_id=?",
    )
    .bind(operation.to_string())
    .fetch_one(&store.pool)
    .await
    .unwrap()
}

async fn journal(store: &StandaloneNativeStore, operation: Uuid) -> Vec<JournalRow> {
    sqlx::query_as(
        "SELECT event_id,response_id,usage,raw_usage,accepted,created_at \
         FROM native_live_journal WHERE operation_id=? ORDER BY event_id",
    )
    .bind(operation.to_string())
    .fetch_all(&store.pool)
    .await
    .unwrap()
}

fn reported_tail(source: &BeginRequest, live: &LiveSessionRecord) -> AppendLiveUsageRequest {
    let mut request = append_request(source, live, "event.expired.accepted", Some("resp.expired"));
    request.usage = vec![observation("", "test-model", 13, 5)];
    request.raw_usage = Some(json!({"provider_event":"response.done","reported":true}));
    request
}

#[tokio::test]
async fn accepted_usage_after_natural_expiry_persists_once_without_renewing_or_allowing_new_work() {
    let f = Fixture::new().await;
    let (source, live) = short_session(&f).await;
    let initial = append_request(&source, &live, "event.before-expiry", Some("resp.before"));
    assert_eq!(
        f.store.append_live_usage(initial).await.unwrap().appended,
        Some(true)
    );
    let original = journal(&f.store, source.operation_id).await;
    assert_eq!(original.len(), 1);
    let before = fence(&f.store, source.operation_id).await;
    naturally_expire(&live).await;
    assert_eq!(fence(&f.store, source.operation_id).await, before);
    assert_eq!(
        f.store
            .checkpoint_live_session(checkpoint(&live, false))
            .await
            .unwrap_err(),
        Error::NotFound,
        "到期后的新工作/续租仍须拒绝"
    );

    let tail = reported_tail(&source, &live);
    let appended = f.store.append_live_usage(tail.clone()).await.unwrap();
    assert_eq!(appended.appended, Some(true));
    assert_eq!(fence(&f.store, source.operation_id).await, before);
    assert_eq!(
        f.store
            .append_live_usage(tail.clone())
            .await
            .unwrap()
            .appended,
        Some(false)
    );
    let mut same_response = tail.clone();
    same_response.event_id = "event.expired.alias".into();
    assert_eq!(
        f.store
            .append_live_usage(same_response)
            .await
            .unwrap()
            .appended,
        Some(false)
    );
    let accepted = journal(&f.store, source.operation_id).await;
    assert_eq!(accepted.len(), 2);
    assert!(accepted.contains(&original[0]));
    let saved = accepted.iter().find(|row| row.0 == tail.event_id).unwrap();
    let usage: serde_json::Value = serde_json::from_str(&saved.2).unwrap();
    assert_eq!(
        usage.pointer("/0/meter/usage/prompt_tokens"),
        Some(&json!(13))
    );
    assert_eq!(
        usage.pointer("/0/meter/usage/completion_tokens"),
        Some(&json!(5))
    );
    assert_eq!(
        usage.pointer("/0/meter/usage/total_tokens"),
        Some(&json!(18))
    );
    let captured: BeginRequest = serde_json::from_str(&saved.4).unwrap();
    assert_eq!(captured.api_key_id, source.api_key_id);
    assert_eq!(captured.origin, source.origin);
    assert_eq!(captured.upstream_model, source.upstream_model);
    let mut conflicting = tail;
    conflicting.usage = vec![observation("", "test-model", 14, 5)];
    assert_eq!(
        f.store.append_live_usage(conflicting).await.unwrap_err(),
        Error::Conflict
    );
    assert_eq!(journal(&f.store, source.operation_id).await, accepted);
    assert_eq!(fence(&f.store, source.operation_id).await, before);
    assert_eq!(
        f.store
            .checkpoint_live_session(checkpoint(&live, false))
            .await
            .unwrap_err(),
        Error::NotFound
    );
}

#[tokio::test]
async fn an_expired_accepted_terminal_tail_survives_key_revocation_and_closes_without_lease_revival(
) {
    let f = Fixture::new().await;
    let (source, live) = short_session(&f).await;
    let initial = append_request(
        &source,
        &live,
        "event.before-terminal",
        Some("resp.initial"),
    );
    assert_eq!(
        f.store.append_live_usage(initial).await.unwrap().appended,
        Some(true)
    );
    let before = fence(&f.store, source.operation_id).await;
    naturally_expire(&live).await;
    let revoked = (*f.snapshot.load()).clone();
    revoked.apikeys.remove("key-a");
    f.snapshot.store(revoked);
    let mut tail = reported_tail(&source, &live);
    tail.terminal = true;
    let reply = f.store.append_live_usage(tail.clone()).await.unwrap();
    assert_eq!(reply.appended, Some(true));
    assert_eq!(reply.live_session.unwrap().state, "closed");
    let closed = fence(&f.store, source.operation_id).await;
    assert_eq!(closed.0, before.0);
    assert_eq!(closed.1, before.1);
    assert_eq!(closed.2, before.2);
    assert_eq!(closed.3, "closed");
    assert!(closed.4.is_some());
    assert_eq!(closed.5, before.5);
    assert_eq!(closed.6, before.6);
    assert_eq!(
        f.store.append_live_usage(tail).await.unwrap().appended,
        Some(false)
    );
    assert_eq!(journal(&f.store, source.operation_id).await.len(), 2);
    assert_eq!(fence(&f.store, source.operation_id).await, closed);
}

#[tokio::test]
async fn recovery_generation_still_fences_the_expired_original_observer() {
    let f = Fixture::new().await;
    let (source, live) = short_session(&f).await;
    assert_eq!(
        f.store
            .append_live_usage(append_request(&source, &live, "event.old-owner", None))
            .await
            .unwrap()
            .appended,
        Some(true)
    );
    naturally_expire(&live).await;
    let recovered = f.reopen().await;
    let claimed = recovered
        .claim_live_sessions(ClaimLiveSessionsRequest { limit: 1 })
        .await
        .unwrap();
    assert_eq!(claimed.live_sessions.len(), 1);
    let current = &claimed.live_sessions[0];
    assert_eq!(current.operation_id, source.operation_id);
    assert!(current.requires_stop);
    assert!(current.lease.as_ref().unwrap().generation > live.lease.as_ref().unwrap().generation);
    let before = journal(&f.store, source.operation_id).await;
    let current_fence = fence(&f.store, source.operation_id).await;
    assert_eq!(
        f.store
            .append_live_usage(reported_tail(&source, &live))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    assert_eq!(journal(&f.store, source.operation_id).await, before);
    assert_eq!(fence(&f.store, source.operation_id).await, current_fence);
}
