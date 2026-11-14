//! IO 终态只补充已结算行的非计费元数据，不产生第二条用量或账本记录。
mod support;

use serde_json::{json, Value};
use sqlx::Connection;
use support::world::{deliver, price, seed_model, tokens_680, usage, world, World};
use uuid::Uuid;

fn audit(bytes: u64) -> Value {
    json!({"version":1,"boundary":"client_exchange","request_body_bytes":91,
        "response_body_bytes":bytes,"response_complete":true,
        "input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true},
        "output_text":{"utf8_bytes":4,"unicode_scalars":1,"complete":true}})
}

fn update(event_id: &str, measurement: Value) -> Value {
    json!({"kind":"io_audit_update","event_id":event_id,"io_audit":measurement})
}

fn original(w: &World, event_id: &str, model: Uuid) -> Value {
    let mut event = usage(w, event_id, model, tokens_680());
    event["is_final_attempt"] = json!(true);
    event
}

fn upstream_audit() -> Value {
    json!({"version":2,"boundary":"upstream_attempt","transport":"mixed","request_body_bytes":123,
        "response_body_bytes":79,"request_complete":true,"response_complete":false,
        "exchange_count":2,
        "input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true},
        "output_text":{"utf8_bytes":5,"unicode_scalars":2,"complete":false}})
}

fn upstream_original(w: &World, event_id: &str, model: Uuid) -> Value {
    let mut event = original(w, event_id, model);
    event["io_audit"] = upstream_audit();
    event
}

async fn stored(w: &World, event_id: &str) -> Value {
    sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("读取原始用量行")
}

async fn assert_one_charge(w: &World, event_id: &str) {
    let charge: (i64, i64) = sqlx::query_as(
        "SELECT charge_micro_cny, (SELECT count(*) FROM billing_ledger_cny WHERE event_id = $1) \
         FROM pricing_charges_cny WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("读取唯一收费和账本记录");
    assert_eq!(charge, (680, 1));
    let ledger: i64 =
        sqlx::query_scalar("SELECT amount_micro_cny FROM billing_ledger_cny WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("读取原始账本金额");
    assert_eq!(ledger, -680);
}

#[tokio::test]
async fn an_io_update_after_original_only_amends_metadata_and_replays_without_billing() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let base = original(&w, "io-update-after", model);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body, json!({"accepted":1,"rejected":0,"duplicates":0}));
    assert_one_charge(&w, "io-update-after").await;
    let batch = Uuid::new_v4();
    let amendment = update("io-update-after", audit(47));
    for batch in [batch, batch, Uuid::new_v4()] {
        let sent = deliver(&w.dp, batch, vec![amendment.clone()]).await;
        assert_eq!(sent.status, 200, "{sent:?}");
        assert_eq!(sent.body, json!({"accepted":0,"rejected":0,"duplicates":0}));
        assert_one_charge(&w, "io-update-after").await;
    }
    let mut expected = base;
    expected["io_audit"] = audit(47);
    assert_eq!(stored(&w, "io-update-after").await, expected);
    assert_eq!(w.cp.count("usage_events").await, 1);
    assert_eq!(w.cp.count("pricing_charges_cny").await, 1);
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
}

#[tokio::test]
async fn an_io_update_before_original_is_durable_and_bound_to_its_authenticated_dp() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let early = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![update("io-update-before", audit(47))],
    )
    .await;
    assert_eq!(early.status, 200, "{early:?}");
    assert_eq!(
        early.body,
        json!({"accepted":0,"rejected":0,"duplicates":0})
    );
    assert_eq!(w.cp.count("usage_events").await, 0);
    assert_eq!(w.cp.count("pricing_charges_cny").await, 0);
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 1);
    let base = original(&w, "io-update-before", model);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body, json!({"accepted":1,"rejected":0,"duplicates":0}));
    let mut expected = base;
    expected["io_audit"] = audit(47);
    assert_eq!(stored(&w, "io-update-before").await, expected);
    assert_one_charge(&w, "io-update-before").await;
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);

    let other = w.cp.issue_dp_client("io-amendment-other").await;
    let foreign = deliver(
        &other,
        Uuid::new_v4(),
        vec![update("io-update-before", audit(999))],
    )
    .await;
    assert_eq!(foreign.status, 200, "{foreign:?}");
    assert_eq!(stored(&w, "io-update-before").await, expected);
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
    assert_one_charge(&w, "io-update-before").await;

    let unknown = deliver(
        &other,
        Uuid::new_v4(),
        vec![update("io-unknown-parent", audit(999))],
    )
    .await;
    assert_eq!(unknown.status, 200, "{unknown:?}");
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 1);
    let base = original(&w, "io-unknown-parent", model);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(
        stored(&w, "io-unknown-parent").await,
        base,
        "另一网关预先提交的元数据不得附到本行"
    );
    assert_one_charge(&w, "io-unknown-parent").await;
}

#[tokio::test]
async fn invalid_io_updates_never_change_tokens_or_price_and_unknown_parents_expire() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let base = original(&w, "io-update-invalid", model);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    let mut invalid = Vec::new();
    for (field, value) in [("version", json!(2)), ("boundary", json!("upstream"))] {
        let mut measurement = audit(47);
        measurement[field] = value;
        invalid.push(update("io-update-invalid", measurement));
    }
    let mut bad_scalars = audit(47);
    bad_scalars["input_text"]["unicode_scalars"] = json!(9);
    invalid.push(update("io-update-invalid", bad_scalars));
    let mut tokens = update("io-update-invalid", audit(47));
    tokens["prompt_tokens"] = json!(9_999_999);
    invalid.push(tokens);
    let sent = deliver(&w.dp, Uuid::new_v4(), invalid).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body, json!({"accepted":0,"rejected":0,"duplicates":0}));
    assert_eq!(stored(&w, "io-update-invalid").await, base);
    assert_one_charge(&w, "io-update-invalid").await;
    assert_eq!(w.cp.count("usage_events").await, 1);
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
    let sent = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![update("io-never-arrives", audit(47))],
    )
    .await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 1);
    sqlx::query("UPDATE usage_io_audit_pending SET received_at = now() - interval '25 hours'")
        .execute(w.cp.db().pool())
        .await
        .expect("让孤立元数据超过保留期");
    aisix_control_plane::housekeeping::run_once(w.cp.db())
        .await
        .expect("运行现有清理流程");
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
    assert_eq!(stored(&w, "io-update-invalid").await, base);
    assert_one_charge(&w, "io-update-invalid").await;
}

#[tokio::test]
async fn concurrent_originals_and_updates_always_attach_to_the_one_priced_row() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    for index in 0..16 {
        let event_id = format!("io-concurrent-{index}");
        let base = original(&w, &event_id, model);
        let (normal, amendment) = tokio::join!(
            deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]),
            deliver(&w.dp, Uuid::new_v4(), vec![update(&event_id, audit(47))]),
        );
        assert_eq!(normal.status, 200, "{normal:?}");
        assert_eq!(amendment.status, 200, "{amendment:?}");
        let mut expected = base;
        expected["io_audit"] = audit(47);
        assert_eq!(stored(&w, &event_id).await, expected);
        assert_one_charge(&w, &event_id).await;
    }
    assert_eq!(w.cp.count("usage_events").await, 16);
    assert_eq!(w.cp.count("pricing_charges_cny").await, 16);
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
}

#[tokio::test]
async fn an_update_committing_while_the_original_insert_is_uncommitted_is_not_lost() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let mut gate = sqlx::PgConnection::connect(w.cp.database_url())
        .await
        .expect("建立竞态屏障连接");
    sqlx::query("SELECT pg_advisory_lock(481725601)")
        .execute(&mut gate)
        .await
        .expect("持有提交屏障");
    sqlx::raw_sql("CREATE FUNCTION io_update_race_gate() RETURNS trigger LANGUAGE plpgsql AS $$ \
        BEGIN IF NEW.event_id = 'io-race-gated' THEN PERFORM pg_advisory_xact_lock(481725601); END IF; RETURN NEW; END $$; \
        CREATE TRIGGER io_update_race_gate AFTER INSERT ON usage_events FOR EACH ROW EXECUTE FUNCTION io_update_race_gate();")
        .execute(w.cp.db().pool()).await.expect("安装仅此测试库的插入屏障");
    let base = original(&w, "io-race-gated", model);
    let controller = async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING(pid) \
                WHERE NOT l.granted AND l.locktype = 'advisory' AND a.datname = current_database()",
            )
            .fetch_one(w.cp.db().pool())
            .await
            .expect("观察原始行尚未提交");
            if waiting > 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "原始行未进入插入屏障"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let amendment = deliver(
            &w.dp,
            Uuid::new_v4(),
            vec![update("io-race-gated", audit(47))],
        )
        .await;
        assert_eq!(amendment.status, 200, "{amendment:?}");
        assert_eq!(
            amendment.body,
            json!({"accepted":0,"rejected":0,"duplicates":0})
        );
        assert_eq!(
            w.cp.count("usage_events").await,
            0,
            "原始行尚未提交，补充记录不能提前造出用量"
        );
        assert_eq!(
            w.cp.count("usage_io_audit_pending").await,
            1,
            "补充元数据已独立持久化"
        );
        sqlx::query("SELECT pg_advisory_unlock(481725601)")
            .execute(&mut gate)
            .await
            .expect("释放提交屏障");
    };
    let (normal, ()) = tokio::join!(
        deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]),
        controller
    );
    assert_eq!(normal.status, 200, "{normal:?}");
    let mut expected = base;
    expected["io_audit"] = audit(47);
    assert_eq!(stored(&w, "io-race-gated").await, expected);
    assert_one_charge(&w, "io-race-gated").await;
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
}

#[tokio::test]
async fn legacy_amendments_never_replace_original_upstream_snapshots_in_any_arrival_order() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    for order in ["before", "after", "same-batch-before", "same-batch-after"] {
        let event_id = format!("upstream-io-{order}");
        let base = upstream_original(&w, &event_id, model);
        let amendment = update(&event_id, audit(999));
        if order == "before" {
            let sent = deliver(&w.dp, Uuid::new_v4(), vec![amendment.clone()]).await;
            assert_eq!(sent.status, 200, "{sent:?}");
            assert_eq!(sent.body, json!({"accepted":0,"rejected":0,"duplicates":0}));
            assert_eq!(w.cp.count("usage_io_audit_pending").await, 1);
        }
        let events = match order {
            "same-batch-before" => vec![amendment.clone(), base.clone()],
            "same-batch-after" => vec![base.clone(), amendment.clone()],
            _ => vec![base.clone()],
        };
        let sent = deliver(&w.dp, Uuid::new_v4(), events).await;
        assert_eq!(sent.status, 200, "{sent:?}");
        assert_eq!(sent.body, json!({"accepted":1,"rejected":0,"duplicates":0}));
        assert_eq!(stored(&w, &event_id).await, base, "到达顺序: {order}");
        assert_one_charge(&w, &event_id).await;
        assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
        let batch = Uuid::new_v4();
        for batch in [batch, batch, Uuid::new_v4()] {
            let sent = deliver(&w.dp, batch, vec![amendment.clone()]).await;
            assert_eq!(sent.status, 200, "{sent:?}");
            assert_eq!(sent.body, json!({"accepted":0,"rejected":0,"duplicates":0}));
            assert_eq!(stored(&w, &event_id).await, base);
            assert_one_charge(&w, &event_id).await;
            assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
        }
    }
    for index in 0..16 {
        let event_id = format!("upstream-io-concurrent-{index}");
        let base = upstream_original(&w, &event_id, model);
        let (normal, amendment) = tokio::join!(
            deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]),
            deliver(&w.dp, Uuid::new_v4(), vec![update(&event_id, audit(999))]),
        );
        assert_eq!(normal.status, 200, "{normal:?}");
        assert_eq!(amendment.status, 200, "{amendment:?}");
        assert_eq!(stored(&w, &event_id).await, base);
        assert_one_charge(&w, &event_id).await;
    }
    assert_eq!(w.cp.count("usage_events").await, 20);
    assert_eq!(w.cp.count("pricing_charges_cny").await, 20);
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
}

#[tokio::test]
async fn upstream_snapshots_are_rejected_on_the_legacy_scalar_update_channel() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let base = original(&w, "upstream-sidecar-existing", model);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    let batch = Uuid::new_v4();
    let events = vec![
        update("upstream-sidecar-existing", upstream_audit()),
        update("upstream-sidecar-before", upstream_audit()),
    ];
    let sent = deliver(&w.dp, batch, events.clone()).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body, json!({"accepted":0,"rejected":0,"duplicates":0}));
    let rejected: Vec<(String, Value)> =
        sqlx::query_as("SELECT reason, raw FROM usage_rejects WHERE batch_id = $1 ORDER BY seq")
            .bind(batch)
            .fetch_all(w.cp.db().pool())
            .await
            .expect("读取旧通道的拒绝记录");
    assert_eq!(rejected.len(), 2);
    for ((reason, raw), expected) in rejected.iter().zip(&events) {
        assert_eq!(reason, "io_audit_update_invalid");
        assert_eq!(raw, expected);
    }
    assert_eq!(stored(&w, "upstream-sidecar-existing").await, base);
    assert_one_charge(&w, "upstream-sidecar-existing").await;
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
    let later = original(&w, "upstream-sidecar-before", model);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![later.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(stored(&w, "upstream-sidecar-before").await, later);
    assert_one_charge(&w, "upstream-sidecar-before").await;
    assert_eq!(w.cp.count("usage_events").await, 2);
    assert_eq!(w.cp.count("pricing_charges_cny").await, 2);
}

#[tokio::test]
async fn a_legacy_update_committed_during_an_upstream_original_insert_cannot_overwrite_it() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let mut gate = sqlx::PgConnection::connect(w.cp.database_url())
        .await
        .expect("建立上游快照竞态屏障连接");
    sqlx::query("SELECT pg_advisory_lock(481725602)")
        .execute(&mut gate)
        .await
        .expect("持有提交屏障");
    sqlx::raw_sql("CREATE FUNCTION upstream_io_race_gate() RETURNS trigger LANGUAGE plpgsql AS $$ \
        BEGIN IF NEW.event_id = 'upstream-io-race-gated' THEN PERFORM pg_advisory_xact_lock(481725602); END IF; RETURN NEW; END $$; \
        CREATE TRIGGER upstream_io_race_gate AFTER INSERT ON usage_events FOR EACH ROW EXECUTE FUNCTION upstream_io_race_gate();")
        .execute(w.cp.db().pool()).await.expect("安装仅此测试库的插入屏障");
    let base = upstream_original(&w, "upstream-io-race-gated", model);
    let controller = async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING(pid) \
                WHERE NOT l.granted AND l.locktype = 'advisory' AND a.datname = current_database()",
            )
            .fetch_one(w.cp.db().pool())
            .await
            .expect("观察上游原始行尚未提交");
            if waiting > 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "原始行未进入插入屏障"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let sent = deliver(
            &w.dp,
            Uuid::new_v4(),
            vec![update("upstream-io-race-gated", audit(999))],
        )
        .await;
        assert_eq!(sent.status, 200, "{sent:?}");
        assert_eq!(sent.body, json!({"accepted":0,"rejected":0,"duplicates":0}));
        assert_eq!(w.cp.count("usage_events").await, 0);
        assert_eq!(w.cp.count("usage_io_audit_pending").await, 1);
        sqlx::query("SELECT pg_advisory_unlock(481725602)")
            .execute(&mut gate)
            .await
            .expect("释放上游快照提交屏障");
    };
    let (normal, ()) = tokio::join!(
        deliver(&w.dp, Uuid::new_v4(), vec![base.clone()]),
        controller
    );
    assert_eq!(normal.status, 200, "{normal:?}");
    assert_eq!(stored(&w, "upstream-io-race-gated").await, base);
    assert_one_charge(&w, "upstream-io-race-gated").await;
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
}
