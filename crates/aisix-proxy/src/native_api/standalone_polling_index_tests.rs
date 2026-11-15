//! 当前受支持文件的轮询索引、租约与去重；计时是组件基准，不代表转发吞吐。
use super::*;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::time::Instant;

const CLAIM_SQL: &str = "SELECT record_id FROM native_objects WHERE pending=1 AND poll_after<=? AND (lease_until IS NULL OR lease_until<=?) AND (? IS NULL OR record_id=?) AND (? IS NULL OR protocol=?) AND (? IS NULL OR kind=?) ORDER BY poll_after,record_id LIMIT ?";
const DELIVERY_SQL: &str = "SELECT event_id,record_id,submission,owner,usage,event,delivery_generation FROM native_terminal_outbox WHERE delivered=0 AND (delivery_until IS NULL OR delivery_until<=?) ORDER BY event_id LIMIT ?";
const INDEXES: &str = "CREATE INDEX IF NOT EXISTS native_objects_poll_pending ON native_objects(poll_after,record_id) WHERE pending=1; CREATE INDEX IF NOT EXISTS native_terminal_delivery_pending ON native_terminal_outbox(event_id) WHERE delivered=0;";
const DROP_INDEXES: &str = "DROP INDEX IF EXISTS native_objects_poll_pending; DROP INDEX IF EXISTS native_terminal_delivery_pending;";

fn response_claim() -> ClaimRequest {
    ClaimRequest {
        limit: 8,
        record_id: None,
        protocol: Some(NativeProtocol::Openai),
        kind: Some(NativeObjectKind::Response),
    }
}

async fn completed(f: &Fixture, raw: &str) -> NativeRecord {
    let record = f
        .create("key-a", NativeObjectKind::Response, raw, true)
        .await;
    let lease = f
        .store
        .claim(ClaimRequest {
            record_id: Some(record.record_id),
            ..response_claim()
        })
        .await
        .unwrap()
        .records
        .remove(0);
    f.store
        .complete(CompleteRequest {
            lease: LeaseToken {
                record_id: record.record_id,
                generation: lease.lease_generation.unwrap(),
            },
            terminal_status: "completed".into(),
            usage: vec![terminal_usage("")],
            output_file_ids: vec![],
        })
        .await
        .unwrap();
    record
}

async fn index_names(store: &StandaloneNativeStore) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type='index' AND name IN ('native_objects_poll_pending','native_terminal_delivery_pending') ORDER BY name")
        .fetch_all(&store.pool).await.unwrap()
}

async fn plans(store: &StandaloneNativeStore) -> (Vec<String>, Vec<String>) {
    let query = format!("EXPLAIN QUERY PLAN {CLAIM_SQL}");
    let claim = sqlx::query(&query)
        .bind(now())
        .bind(now())
        .bind(None::<String>)
        .bind(None::<String>)
        .bind("openai")
        .bind("openai")
        .bind("response")
        .bind("response")
        .bind(8)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    let query = format!("EXPLAIN QUERY PLAN {DELIVERY_SQL}");
    let delivery = sqlx::query(&query)
        .bind(now())
        .bind(16)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    let details = |rows: Vec<sqlx::sqlite::SqliteRow>| {
        rows.into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect()
    };
    (details(claim), details(delivery))
}

fn uses_index(plan: &[String], name: &str) -> bool {
    plan.iter().any(|detail| detail.contains(name))
}

async fn facts_digest(store: &StandaloneNativeStore) -> Vec<u8> {
    let mut digest = Sha256::new();
    for query in [
        "SELECT json_array(name,value) FROM native_meta ORDER BY name",
        "SELECT json_array(operation_id,owner,caller,submission,state,related,root_id,worker_id,telemetry) FROM native_operations ORDER BY operation_id",
        "SELECT json_array(record_id,operation_id,protocol,kind,boundary,raw_id,owner,record,pending,poll_after,lease_worker,lease_until,lease_generation,terminal) FROM native_objects ORDER BY record_id",
        "SELECT json_array(event_id,record_id,entry_id,submission,owner,usage,delivered,delivery_worker,delivery_until,delivery_generation,event) FROM native_terminal_outbox ORDER BY event_id",
    ] {
        let rows: Vec<String> = sqlx::query_scalar(query).fetch_all(&store.pool).await.unwrap();
        for row in rows {
            digest.update((row.len() as u64).to_be_bytes());
            digest.update(row.as_bytes());
        }
    }
    digest.finalize().to_vec()
}

// 合成行只用于缩放读写成本，模板来自真实 public begin/bind/claim/complete；不是已执行的上游请求。
async fn seed_history(
    store: &StandaloneNativeStore,
    template: &NativeRecord,
    first: i64,
    last: i64,
) {
    let operation = template.operation_id.unwrap().to_string();
    let record = template.record_id.to_string();
    let mut tx = store.write_tx().await.unwrap();
    sqlx::query("WITH RECURSIVE history(i) AS (SELECT ? UNION ALL SELECT i+1 FROM history WHERE i<?) INSERT INTO native_operations(operation_id,owner,caller,submission,state,related,root_id,worker_id,telemetry) SELECT printf('00000000-0000-4000-8001-%012x',i),p.owner,p.caller,json_set(p.submission,'$.operation_id',printf('00000000-0000-4000-8001-%012x',i)),'bound',json_set(p.related,'$.submission.operation_id',printf('00000000-0000-4000-8001-%012x',i)),printf('00000000-0000-4000-8002-%012x',i),p.worker_id,p.telemetry FROM history CROSS JOIN native_operations p WHERE p.operation_id=?")
        .bind(first).bind(last).bind(&operation).execute(&mut *tx).await.unwrap();
    sqlx::query("WITH RECURSIVE history(i) AS (SELECT ? UNION ALL SELECT i+1 FROM history WHERE i<?) INSERT INTO native_objects(record_id,operation_id,protocol,kind,boundary,raw_id,owner,record,pending,poll_after,lease_worker,lease_until,lease_generation,terminal) SELECT printf('00000000-0000-4000-8002-%012x',i),printf('00000000-0000-4000-8001-%012x',i),o.protocol,o.kind,o.boundary,printf('resp-synthetic-poll-history-%d',i),o.owner,json_set(o.record,'$.record_id',printf('00000000-0000-4000-8002-%012x',i),'$.operation_id',printf('00000000-0000-4000-8001-%012x',i),'$.raw_id',printf('resp-synthetic-poll-history-%d',i)),0,o.poll_after,o.lease_worker,o.lease_until,o.lease_generation,json_set(o.terminal,'$.lease.record_id',printf('00000000-0000-4000-8002-%012x',i)) FROM history CROSS JOIN native_objects o WHERE o.record_id=?")
        .bind(first).bind(last).bind(&record).execute(&mut *tx).await.unwrap();
    sqlx::query("WITH RECURSIVE history(i) AS (SELECT ? UNION ALL SELECT i+1 FROM history WHERE i<?) INSERT INTO native_terminal_outbox(event_id,record_id,entry_id,submission,owner,usage,delivered,delivery_worker,delivery_until,delivery_generation,event) SELECT printf('00000000-0000-4000-8003-%012x',i),printf('00000000-0000-4000-8002-%012x',i),t.entry_id,json_set(t.submission,'$.operation_id',printf('00000000-0000-4000-8001-%012x',i)),t.owner,t.usage,1,NULL,NULL,0,json_set(t.event,'$.event_id',printf('00000000-0000-4000-8003-%012x',i)) FROM history CROSS JOIN native_terminal_outbox t WHERE t.record_id=?")
        .bind(first).bind(last).bind(&record).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

async fn drop_indexes(store: &StandaloneNativeStore) {
    sqlx::raw_sql(DROP_INDEXES)
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(index_names(store).await.is_empty());
}

async fn build_indexes(store: &StandaloneNativeStore) -> f64 {
    let started = Instant::now();
    let mut tx = store.write_tx().await.unwrap();
    sqlx::raw_sql(INDEXES).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    started.elapsed().as_secs_f64() * 1000.0
}

#[tokio::test]
async fn reopening_current_file_restores_polling_indexes_and_preserves_lease_and_terminal_semantics(
) {
    let f = Fixture::new().await;
    let historical = completed(&f, "resp-index-history").await;
    let delivered = f.store.pending_terminals(1).await.unwrap().remove(0);
    f.store
        .acknowledge_terminal_claim(&delivered)
        .await
        .unwrap();
    seed_history(&f.store, &historical, 1, 128).await;
    let ready = f
        .create(
            "key-a",
            NativeObjectKind::Response,
            "resp-index-ready",
            true,
        )
        .await;
    let held = f
        .create("key-a", NativeObjectKind::Response, "resp-index-held", true)
        .await;
    let held = f
        .store
        .claim(ClaimRequest {
            record_id: Some(held.record_id),
            ..response_claim()
        })
        .await
        .unwrap()
        .records
        .remove(0);
    assert!(held.lease_expires_at.unwrap() > Utc::now());
    f.create(
        "key-a",
        NativeObjectKind::MessageBatch,
        "batch-index-filter",
        true,
    )
    .await;
    let held_terminal = completed(&f, "resp-index-delivery-held").await;
    let held_delivery = f.store.pending_terminals(1).await.unwrap().remove(0);
    assert_eq!(held_delivery.record_id, held_terminal.record_id);
    let ready_terminal = completed(&f, "resp-index-delivery-ready").await;
    let canonical: String =
        sqlx::query_scalar("SELECT event FROM native_terminal_outbox WHERE record_id=?")
            .bind(ready_terminal.record_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    let before = facts_digest(&f.store).await;
    let format_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    assert_eq!(format_version, 1);
    drop_indexes(&f.store).await;
    let missing = plans(&f.store).await;
    assert!(!uses_index(&missing.0, "native_objects_poll_pending"));
    assert!(!uses_index(&missing.1, "native_terminal_delivery_pending"));
    f.store.close().await;
    let started = Instant::now();
    let reopened = f.reopen().await;
    let reopen_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(facts_digest(&reopened).await == before);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&reopened.pool)
            .await
            .unwrap(),
        format_version
    );
    let restored = plans(&reopened).await;
    eprintln!(
        "SQLITE_POLL_REOPEN {}",
        json!({"reopen_ms":reopen_ms,"missing_plans":missing,"reopened_plans":restored,"indexes":index_names(&reopened).await})
    );

    let leased = reopened.claim(response_claim()).await.unwrap().records;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].record_id, ready.record_id);
    assert!(reopened
        .claim(response_claim())
        .await
        .unwrap()
        .records
        .is_empty());
    let second = f.reopen().await;
    let (one, two) = tokio::join!(reopened.pending_terminals(16), second.pending_terminals(16));
    let one = one.unwrap();
    let two = two.unwrap();
    let mut counts = vec![one.len(), two.len()];
    counts.sort_unstable();
    assert_eq!(counts, vec![0, 1]);
    let (delivery_store, terminal) = if one.is_empty() {
        (&second, &two[0])
    } else {
        (&reopened, &one[0])
    };
    assert_eq!(terminal.record_id, ready_terminal.record_id);
    assert_eq!(terminal.canonical_event, canonical);
    assert!(reopened.pending_terminals(16).await.unwrap().is_empty());
    assert!(second.pending_terminals(16).await.unwrap().is_empty());
    delivery_store
        .acknowledge_terminal_claim(terminal)
        .await
        .unwrap();
    assert_eq!(
        delivery_store
            .acknowledge_terminal_claim(terminal)
            .await
            .unwrap_err(),
        Error::Conflict
    );

    reopened
        .reschedule(LeaseRequest {
            lease: LeaseToken {
                record_id: ready.record_id,
                generation: leased[0].lease_generation.unwrap(),
            },
            poll_after_ms: 0,
        })
        .await
        .unwrap();
    let (one, two) = tokio::join!(
        reopened.claim(response_claim()),
        second.claim(response_claim())
    );
    let one = one.unwrap().records;
    let two = two.unwrap().records;
    let mut counts = vec![one.len(), two.len()];
    counts.sort_unstable();
    assert_eq!(counts, vec![0, 1]);
    let claimed = one.iter().chain(two.iter()).next().unwrap();
    assert_eq!(claimed.record_id, ready.record_id);
    assert_eq!(
        claimed.lease_generation,
        Some(leased[0].lease_generation.unwrap() + 1)
    );
    assert!(second.pending_terminals(16).await.unwrap().is_empty());

    // 语义正控先执行，再以真实计划判定缺索引，不能把入口失败称为租约/去重证明。
    assert!(
        uses_index(&restored.0, "native_objects_poll_pending"),
        "{restored:?}"
    );
    assert!(
        uses_index(&restored.1, "native_terminal_delivery_pending"),
        "{restored:?}"
    );
    assert!(restored
        .0
        .iter()
        .all(|detail| !detail.contains("TEMP B-TREE")));
}

fn summary(mut samples: Vec<f64>) -> serde_json::Value {
    samples.sort_by(f64::total_cmp);
    json!({"samples":samples.len(),"median_us":samples[samples.len()/2],"min_us":samples[0],"max_us":samples[samples.len()-1]})
}

async fn empty_polls(store: &StandaloneNativeStore) -> serde_json::Value {
    let mut claims = Vec::new();
    let mut terminals = Vec::new();
    for iteration in 0..24 {
        let started = Instant::now();
        assert!(store
            .claim(response_claim())
            .await
            .unwrap()
            .records
            .is_empty());
        let claim = started.elapsed().as_secs_f64() * 1_000_000.0;
        let started = Instant::now();
        assert!(store.pending_terminals(16).await.unwrap().is_empty());
        if iteration >= 3 {
            claims.push(claim);
            terminals.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }
    }
    json!({"claim":summary(claims),"pending_terminals":summary(terminals)})
}

async fn selected_ids(store: &StandaloneNativeStore) -> (Vec<String>, Vec<String>) {
    let mut tx = store.write_tx().await.unwrap();
    let claims = sqlx::query_scalar(CLAIM_SQL)
        .bind(now())
        .bind(now())
        .bind(None::<String>)
        .bind(None::<String>)
        .bind("openai")
        .bind("openai")
        .bind("response")
        .bind("response")
        .bind(8)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let rows = sqlx::query(DELIVERY_SQL)
        .bind(now())
        .bind(16)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let delivery = rows
        .into_iter()
        .map(|row| row.get::<String, _>("event_id"))
        .collect();
    tx.rollback().await.unwrap();
    (claims, delivery)
}

async fn sparse_selection(
    store: &StandaloneNativeStore,
) -> (serde_json::Value, (Vec<String>, Vec<String>)) {
    let expected = selected_ids(store).await;
    let mut elapsed = Vec::new();
    for iteration in 0..10 {
        let started = Instant::now();
        let ids = selected_ids(store).await;
        if iteration >= 3 {
            elapsed.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        }
        assert_eq!(ids, expected);
    }
    (summary(elapsed), expected)
}

async fn write_pending(f: &Fixture, prefix: &str) -> serde_json::Value {
    let mut elapsed = Vec::new();
    for i in 0..8 {
        let started = Instant::now();
        f.create(
            "key-a",
            NativeObjectKind::Response,
            &format!("resp-index-write-{prefix}-{i}"),
            true,
        )
        .await;
        elapsed.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    summary(elapsed)
}

// 单独计时 complete/ack 调用；不把其中读取、事件构建和 fsync 当成纯索引维护成本。
async fn write_terminals(f: &Fixture, prefix: &str) -> serde_json::Value {
    let mut complete = Vec::new();
    let mut acknowledge = Vec::new();
    for i in 0..8 {
        let record = f
            .create(
                "key-a",
                NativeObjectKind::Response,
                &format!("resp-index-terminal-{prefix}-{i}"),
                true,
            )
            .await;
        let lease = f
            .store
            .claim(ClaimRequest {
                record_id: Some(record.record_id),
                ..response_claim()
            })
            .await
            .unwrap()
            .records
            .remove(0);
        let started = Instant::now();
        f.store
            .complete(CompleteRequest {
                lease: LeaseToken {
                    record_id: record.record_id,
                    generation: lease.lease_generation.unwrap(),
                },
                terminal_status: "completed".into(),
                usage: vec![terminal_usage("")],
                output_file_ids: vec![],
            })
            .await
            .unwrap();
        complete.push(started.elapsed().as_secs_f64() * 1_000_000.0);
        let terminal = f.store.pending_terminals(1).await.unwrap().remove(0);
        assert_eq!(terminal.record_id, record.record_id);
        let started = Instant::now();
        f.store.acknowledge_terminal_claim(&terminal).await.unwrap();
        acknowledge.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    json!({"complete":summary(complete),"acknowledge":summary(acknowledge)})
}

#[tokio::test]
async fn rust_sqlx_polling_baseline_reports_history_scaling_build_reopen_and_write_costs() {
    let f = Fixture::new().await;
    let template = completed(&f, "resp-index-benchmark-template").await;
    let terminal = f.store.pending_terminals(1).await.unwrap().remove(0);
    f.store.acknowledge_terminal_claim(&terminal).await.unwrap();
    let sqlite: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&f.store.pool)
        .await
        .unwrap();
    let mut prior = 0;
    for history in [1_000, 10_000, 100_000] {
        drop_indexes(&f.store).await;
        let started = Instant::now();
        seed_history(&f.store, &template, prior + 1, history).await;
        let synthetic_seed_ms = started.elapsed().as_secs_f64() * 1000.0;
        prior = history;
        let started = Instant::now();
        let reopening = f.reopen().await;
        let reopen_missing_ms = started.elapsed().as_secs_f64() * 1000.0;
        let startup_indexes = index_names(&reopening).await;
        reopening.close().await;
        // 两组都在同一文件和同一逻辑数据上测；产品升级后也保留无索引负控。
        drop_indexes(&f.store).await;
        let before_plans = plans(&f.store).await;
        let before = empty_polls(&f.store).await;
        let index_build_ms = build_indexes(&f.store).await;
        let after_plans = plans(&f.store).await;
        let after = empty_polls(&f.store).await;
        assert!(uses_index(&after_plans.0, "native_objects_poll_pending"));
        assert!(uses_index(
            &after_plans.1,
            "native_terminal_delivery_pending"
        ));
        assert!(!uses_index(&before_plans.0, "native_objects_poll_pending"));
        assert!(!uses_index(
            &before_plans.1,
            "native_terminal_delivery_pending"
        ));
        let started = Instant::now();
        let reopening = f.reopen().await;
        let reopen_existing_ms = started.elapsed().as_secs_f64() * 1000.0;
        reopening.close().await;
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM native_objects")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(rows, history + 1);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM native_terminal_outbox")
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
        assert_eq!(events, history + 1);
        eprintln!(
            "SQLITE_POLL_BENCH {}",
            json!({"sqlite":sqlite,"synthetic_history_rows_per_table":history,"synthetic_seed_ms":synthetic_seed_ms,"startup_missing_index_ms":reopen_missing_ms,"startup_created_indexes":startup_indexes,"explicit_index_build_ms":index_build_ms,"startup_existing_index_ms":reopen_existing_ms,"before_plans":before_plans,"after_plans":after_plans,"full_store_empty_poll_before":before,"full_store_empty_poll_after":after,"scope":"local file SQLx component; warm OS cache; synthetic history, not upstream requests or gateway throughput"})
        );
    }

    drop_indexes(&f.store).await;
    let unindexed_writes = write_pending(&f, "without-index").await;
    let unindexed_terminal_writes = write_terminals(&f, "without-index").await;
    let write_index_build_ms = build_indexes(&f.store).await;
    let indexed_writes = write_pending(&f, "with-index").await;
    let indexed_terminal_writes = write_terminals(&f, "with-index").await;
    let held = f
        .create(
            "key-a",
            NativeObjectKind::Response,
            "resp-index-bench-held",
            true,
        )
        .await;
    let held = f
        .store
        .claim(ClaimRequest {
            record_id: Some(held.record_id),
            ..response_claim()
        })
        .await
        .unwrap()
        .records
        .remove(0);
    assert!(held.lease_expires_at.unwrap() > Utc::now());
    let pending = completed(&f, "resp-index-bench-undelivered").await;
    let expected_event: String =
        sqlx::query_scalar("SELECT event_id FROM native_terminal_outbox WHERE record_id=?")
            .bind(pending.record_id.to_string())
            .fetch_one(&f.store.pool)
            .await
            .unwrap();
    drop_indexes(&f.store).await;
    let before = sparse_selection(&f.store).await;
    let sparse_build_ms = build_indexes(&f.store).await;
    let after = sparse_selection(&f.store).await;
    assert_eq!(before.1, after.1);
    assert_eq!(after.1 .0.len(), 8);
    assert!(!after.1 .0.contains(&held.record_id.to_string()));
    assert_eq!(after.1 .1, vec![expected_event]);
    eprintln!(
        "SQLITE_POLL_WRITES_AND_SPARSE {}",
        json!({"unindexed_public_begin_bind":unindexed_writes,"indexed_public_begin_bind":indexed_writes,"unindexed_terminal_writes":unindexed_terminal_writes,"indexed_terminal_writes":indexed_terminal_writes,"write_phase_index_build_ms":write_index_build_ms,"sparse_index_build_ms":sparse_build_ms,"selection_before":before.0,"selection_after":after.0,"equal_result_ids":after.1,"scope":"write samples include UUID/B-tree placement/fsync variation; no zero-overhead or causal throughput claim"})
    );
}
