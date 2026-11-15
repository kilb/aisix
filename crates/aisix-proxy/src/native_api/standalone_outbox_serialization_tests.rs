// 真实文件 SQLite 组件回归；只计序列化调用，不把计时或本地用量当提供商 E2E。
use super::*;
use sqlx::Row;
use std::{any::type_name, cell::Cell, time::Instant};

thread_local! {
    static SUBMISSION_ENCODINGS: Cell<Option<usize>> = const { Cell::new(None) };
}

pub(crate) fn record_json<T>() {
    if type_name::<T>() == type_name::<BeginRequest>() {
        SUBMISSION_ENCODINGS.with(|counter| {
            if let Some(calls) = counter.get() {
                counter.set(Some(calls + 1));
            }
        });
    }
}

struct SerializationScope;
impl SerializationScope {
    fn start() -> Self {
        SUBMISSION_ENCODINGS.with(|counter| assert_eq!(counter.replace(Some(0)), None));
        Self
    }

    fn finish(self) -> usize {
        let calls = SUBMISSION_ENCODINGS.with(|counter| counter.get().unwrap());
        drop(self);
        calls
    }
}
impl Drop for SerializationScope {
    fn drop(&mut self) {
        SUBMISSION_ENCODINGS.with(|counter| counter.set(None));
    }
}

async fn saved_rows(
    store: &StandaloneNativeStore,
    record_id: Uuid,
) -> Vec<(String, String, String, String, String)> {
    sqlx::query("SELECT entry_id,event_id,submission,usage,event FROM native_terminal_outbox WHERE record_id=? ORDER BY entry_id")
        .bind(record_id.to_string()).fetch_all(&store.pool).await.unwrap()
        .into_iter().map(|row| (
            row.get("entry_id"), row.get("event_id"), row.get("submission"),
            row.get("usage"), row.get("event"),
        )).collect()
}

async fn batch_completion_serialization(entry_count: usize) {
    let f = Fixture::new().await;
    let mut begin = f.begin("key-a", NativeObjectKind::MessageBatch, true);
    begin.entries = (0..entry_count)
        .map(|index| NativeEntry {
            usage_context: None,
            upstream_input_text: None,
            endpoint: "/v1/responses".into(),
            entry_id: format!("original-entry-{index:04}"),
            requested_model: begin.requested_model.clone(),
            requested_service_tier: None,
            requested_model_id: begin.requested_model_id,
            model_id: begin.origin.model_id,
            upstream_model: begin.upstream_model.clone(),
            upstream_provider: begin.upstream_provider.clone(),
        })
        .collect();
    let expected_submission = serde_json::to_string(&begin).unwrap();
    // 计数器移除或未接上真实序列化入口时，校准必须先失败，不能把恒零当优化。
    let calibration = SerializationScope::start();
    assert_eq!(super::super::json(&begin).unwrap(), expected_submission);
    assert_eq!(calibration.finish(), 1);

    assert_eq!(
        f.store.begin(begin.clone()).await.unwrap().state,
        "pending_submission"
    );
    let raw_id = format!("batch-nio2-{entry_count}");
    let record = f
        .store
        .bind(BindRequest {
            existing_record_id: None,
            objects: vec![],
            operation_id: begin.operation_id,
            api_key_id: begin.api_key_id,
            raw_id: raw_id.clone(),
            provider_created_at: None,
            provider_expires_at: None,
        })
        .await
        .unwrap()
        .record
        .unwrap();
    let leased = f
        .store
        .claim(ClaimRequest {
            limit: 1,
            record_id: Some(record.record_id),
            protocol: None,
            kind: None,
        })
        .await
        .unwrap()
        .records
        .remove(0);
    let request = CompleteRequest {
        lease: LeaseToken {
            record_id: record.record_id,
            generation: leased.lease_generation.unwrap(),
        },
        terminal_status: "completed".into(),
        usage: begin
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let mut usage = terminal_usage(&entry.entry_id);
                usage.prompt_tokens += index as u32;
                usage.total_tokens += index as u32;
                usage
            })
            .collect(),
        output_file_ids: vec![],
    };
    // 在任何 terminal receipt 存在前验证代次；否则 payload 差异也会产生 Conflict。
    let mut stale_fresh = request.clone();
    stale_fresh.lease.generation += 1;
    let rejected_scope = SerializationScope::start();
    assert_eq!(
        f.store.complete(stale_fresh).await.unwrap_err(),
        Error::Conflict
    );
    assert_eq!(rejected_scope.finish(), 0);
    assert!(saved_rows(&f.store, record.record_id).await.is_empty());
    let untouched = f
        .store
        .lookup(lookup("key-a", NativeObjectKind::MessageBatch, &raw_id))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(untouched.version, leased.version);
    assert!(!untouched.completed);

    let fresh = request.clone();
    let scope = SerializationScope::start();
    let started = Instant::now();
    let reply = f.store.complete(fresh).await.unwrap();
    let elapsed = started.elapsed();
    let encodings = scope.finish();
    assert_eq!(reply.state, "completed");
    let rows = saved_rows(&f.store, record.record_id).await;
    assert_eq!(rows.len(), entry_count);
    for (index, (entry_id, event_id, submission, usage, event)) in rows.iter().enumerate() {
        assert_eq!(entry_id, &begin.entries[index].entry_id);
        assert_eq!(
            event_id,
            &Uuid::new_v5(&record.record_id, entry_id.as_bytes()).to_string()
        );
        assert_eq!(submission.as_bytes(), expected_submission.as_bytes());
        assert_eq!(
            serde_json::from_str::<BeginRequest>(submission).unwrap(),
            begin
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(usage).unwrap(),
            serde_json::to_value(&request.usage[index]).unwrap()
        );
        let event: serde_json::Value = serde_json::from_str(event).unwrap();
        assert_eq!(event["event_id"], event_id.as_str());
        assert_eq!(event["prompt_tokens"], request.usage[index].prompt_tokens);
        assert_eq!(
            event["completion_tokens"],
            request.usage[index].completion_tokens
        );
        assert_eq!(
            event["cached_prompt_tokens"],
            request.usage[index].cached_prompt_tokens
        );
        assert_eq!(event["total_tokens"], request.usage[index].total_tokens);
    }
    let before = f
        .store
        .lookup(lookup("key-a", NativeObjectKind::MessageBatch, &raw_id))
        .await
        .unwrap()
        .record
        .unwrap();
    let replay_scope = SerializationScope::start();
    assert_eq!(
        f.store.complete(request.clone()).await.unwrap().state,
        "completed"
    );
    assert_eq!(replay_scope.finish(), 0);
    let mut stale = request;
    stale.lease.generation += 1;
    assert_eq!(f.store.complete(stale).await.unwrap_err(), Error::Conflict);
    let after = f
        .store
        .lookup(lookup("key-a", NativeObjectKind::MessageBatch, &raw_id))
        .await
        .unwrap()
        .record
        .unwrap();
    assert_eq!(after.version, before.version);
    assert_eq!(saved_rows(&f.store, record.record_id).await, rows);

    f.store.close().await;
    let reopened = f.reopen().await;
    assert_eq!(saved_rows(&reopened, record.record_id).await, rows);
    assert_eq!(
        reopened
            .lookup(lookup("key-b", NativeObjectKind::MessageBatch, &raw_id))
            .await
            .unwrap_err(),
        Error::NotFound
    );
    let expected_events: BTreeMap<_, _> = rows.iter().map(|row| (row.1.as_str(), row)).collect();
    let mut delivered = BTreeSet::new();
    loop {
        let claims = reopened.pending_terminals(100).await.unwrap();
        if claims.is_empty() {
            break;
        }
        for claim in claims {
            assert!(delivered.insert(claim.event_id));
            let expected = expected_events
                .get(claim.event_id.to_string().as_str())
                .unwrap();
            assert_eq!(claim.submission, begin);
            assert_eq!(claim.api_key_id, "key-a");
            assert_eq!(claim.team_id.as_deref(), Some("team-a"));
            assert_eq!(claim.user_id.as_deref(), Some("user-a"));
            assert_eq!(claim.canonical_event.as_bytes(), expected.4.as_bytes());
            assert_eq!(serde_json::to_string(&claim.usage).unwrap(), expected.3);
            reopened.acknowledge_terminal_claim(&claim).await.unwrap();
            assert_eq!(
                reopened
                    .acknowledge_terminal_claim(&claim)
                    .await
                    .unwrap_err(),
                Error::Conflict
            );
        }
    }
    assert_eq!(delivered.len(), entry_count);
    assert_eq!(saved_rows(&reopened, record.record_id).await, rows);
    let acknowledged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM native_terminal_outbox WHERE record_id=? AND delivered=1",
    )
    .bind(record.record_id.to_string())
    .fetch_one(&reopened.pool)
    .await
    .unwrap();
    assert_eq!(acknowledged, entry_count as i64);
    println!(
        "NIO2_SERIALIZATION_WORK {}",
        json!({
            "entries": entry_count, "full_submission_bytes": expected_submission.len(),
            "begin_request_encode_calls": encodings,
            "serialized_submission_bytes": encodings * expected_submission.len(),
            "persisted_submission_bytes": entry_count * expected_submission.len(),
            "complete_elapsed_ns": elapsed.as_nanos(), "durable_rows": rows.len(),
            "acknowledged_rows": acknowledged, "business_checks_before_budget": true,
        })
    );
    assert_eq!(
        encodings, 1,
        "one immutable full submission serialization per fresh complete"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn batch32_keeps_full_outbox_facts_with_one_submission_serialization() {
    batch_completion_serialization(32).await;
}

#[tokio::test(flavor = "current_thread")]
async fn batch256_keeps_full_outbox_facts_with_one_submission_serialization() {
    batch_completion_serialization(256).await;
}
