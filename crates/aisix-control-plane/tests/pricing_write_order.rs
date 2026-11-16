//! 价格写入等待版本锁后，必须按真实数据库时钟重新校验生效时间。

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::pricing::baselines::{self, BaselineSource, NewBaseline};
use aisix_control_plane::pricing::cost_rates::{self, NewCostRate};
use aisix_control_plane::pricing::rates::{self, NewGroupRate};
use aisix_control_plane::pricing::store::{self, NewDiscount};
use aisix_pricing::CnyPerUsd;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use sqlx::{Connection, PgConnection};
use support::world::{
    card, db_now, deliver, group_id, price, seed_model, seed_provider_key, tokens_680, usage, world,
};
use uuid::Uuid;

async fn write_revision(
    conn: &mut PgConnection,
    kind: usize,
    model: Uuid,
    group: Uuid,
    account: Uuid,
    tenant: Uuid,
    at: DateTime<Utc>,
) -> Result<(), ApiError> {
    match kind {
        0 => baselines::insert_baseline(
            conn,
            &NewBaseline {
                model_id: model,
                effective_from: at,
                card: card(),
                source: BaselineSource::Manual {
                    note: "锁等待回归".into(),
                },
            },
            Actor::System,
        )
        .await
        .map(|_| ()),
        1 => rates::insert_group_rate(
            conn,
            &NewGroupRate {
                group_id: group,
                model_id: None,
                effective_from: at,
                cny_per_usd: Some(CnyPerUsd(20_000)),
                note: String::new(),
            },
            Actor::System,
        )
        .await
        .map(|_| ()),
        2 => cost_rates::insert_cost_rate(
            conn,
            &NewCostRate {
                provider_key_id: account,
                model_id: None,
                effective_from: at,
                cny_per_usd: Some(CnyPerUsd(20_000)),
                note: String::new(),
            },
            Actor::System,
        )
        .await
        .map(|_| ()),
        3 => store::insert_discount(
            conn,
            &NewDiscount {
                tenant_id: tenant,
                effective_from: at,
                discount_bps: 100,
            },
            Actor::System,
        )
        .await
        .map(|_| ()),
        _ => unreachable!("four price revision families"),
    }
}

#[tokio::test]
async fn every_price_writer_rechecks_wall_time_after_waiting_on_the_version() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let group = group_id(&w.cp, "shared").await;
    let tables = [
        "pricing_baselines",
        "pricing_group_rates",
        "pricing_cost_rates",
        "pricing_discount_revisions",
    ];
    let mut before = Vec::new();
    for table in tables {
        before.push(w.cp.count(table).await);
    }
    let audits = w.cp.count("audit_log").await;
    let version: serde_json::Value =
        sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = 'pricing_version'")
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read version");
    let mut held = PgConnection::connect(w.cp.database_url())
        .await
        .expect("connect lock holder");
    let mut lock = held.begin().await.expect("begin lock holder");
    sqlx::query("SELECT value FROM cp_settings WHERE key = 'pricing_version' FOR UPDATE")
        .fetch_one(&mut *lock)
        .await
        .expect("hold version row");
    let at = db_now(&w.cp).await - TimeDelta::seconds(3);
    let mut writes = Vec::new();
    let mut pids = Vec::new();
    for kind in 0..4 {
        let mut conn = PgConnection::connect(w.cp.database_url())
            .await
            .expect("connect writer");
        sqlx::query("BEGIN")
            .execute(&mut conn)
            .await
            .expect("begin writer");
        sqlx::query("SET LOCAL lock_timeout = '15s'")
            .execute(&mut conn)
            .await
            .expect("bound wait");
        pids.push(
            sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
                .fetch_one(&mut conn)
                .await
                .expect("writer pid"),
        );
        let tenant = w.tenant;
        writes.push(tokio::spawn(async move {
            let result = write_revision(&mut conn, kind, model, group, account, tenant, at).await;
            sqlx::query("ROLLBACK")
                .execute(&mut conn)
                .await
                .expect("rollback test writer");
            result
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let blocked: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = ANY($1) AND wait_event_type = 'Lock'")
                .bind(&pids).fetch_one(w.cp.db().pool()).await.expect("observe blocked writers");
            if blocked == 4 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("all four writers must actually wait on the version row");
    let response = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "while-price-waits", model, tokens_680())],
    )
    .await;
    assert_eq!(
        response.status, 200,
        "intake commits while price writers wait: {response:?}"
    );
    let charged: i64 = sqlx::query_scalar(
        "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = 'while-price-waits'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read committed charge");
    assert_eq!(charged, 680);
    // 按数据库条件等待跨过容差，避免测试依赖主机时钟。
    loop {
        let expired: bool =
            sqlx::query_scalar("SELECT clock_timestamp() > $1 + interval '5.1 seconds'")
                .bind(at)
                .fetch_one(w.cp.db().pool())
                .await
                .expect("read wall clock");
        if expired {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    lock.rollback().await.expect("release version row");
    let mut outcomes = Vec::new();
    for write in writes {
        outcomes.push(match write.await.expect("writer task") {
            Ok(()) => (200, Vec::new()),
            Err(err) => (
                err.status.as_u16(),
                err.details.into_iter().map(|d| d.path).collect(),
            ),
        });
    }
    assert_eq!(
        outcomes,
        vec![(422, vec!["/effective_from".to_string()]); 4],
        "every family must reject a revision that expired during its version wait"
    );
    for (table, count) in tables.into_iter().zip(before) {
        assert_eq!(w.cp.count(table).await, count, "{table}");
    }
    assert_eq!(w.cp.count("audit_log").await, audits);
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = 'pricing_version'")
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read version after refusal");
    assert_eq!(after, version);
    assert_eq!(
        response.body,
        json!({"accepted": 1, "rejected": 0, "duplicates": 0})
    );
}

#[tokio::test]
async fn catalogue_and_uploaded_imports_recheck_time_after_the_version_wait() {
    let Some(w) = world().await else { return };
    let catalogue_model = seed_model(&w.cp, &w.admin).await;
    let imported_model = seed_model(&w.cp, &w.admin).await;
    let snapshot = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/e2e/fixtures/pricing-import/genai-prices-v2-7de757d.json"
    ))
    .expect("read the committed dataset snapshot");
    let body = format!("{{\"dataset_ref\":\"version-lock-regression\",\"data\":{snapshot}}}");
    let uploaded =
        w.cp.send_raw(
            "POST",
            "/admin/pricing/imports",
            Some(w.admin.cookie()),
            Some("application/json"),
            Some(body.into_bytes()),
        )
        .await;
    assert_eq!(uploaded.status, 201, "{uploaded:?}");
    let batch = uploaded.body["id"].as_str().expect("an import id");
    let proposal: Uuid = sqlx::query_scalar(
        "SELECT id FROM pricing_import_proposals WHERE batch_id = $1 \
         AND provider = 'anthropic' AND model = 'claude-opus-5-5' AND effective_from IS NULL",
    )
    .bind(Uuid::parse_str(batch).expect("batch UUID"))
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the snapshot's current proposal");
    let audits = w.cp.count("audit_log").await;
    let mut held = w.cp.db().pool().begin().await.expect("hold price version");
    sqlx::query("SELECT value FROM cp_settings WHERE key = 'pricing_version' FOR UPDATE")
        .fetch_one(&mut *held)
        .await
        .expect("lock version row");
    let at = db_now(&w.cp).await - TimeDelta::seconds(3);
    let requests = [
        (
            w.cp.url("/admin/pricing/baselines"),
            json!({
                "model_id": catalogue_model, "effective_from": at,
                "catalogue_key": "openai/gpt-5.4-mini"
            }),
        ),
        (
            w.cp.url(&format!(
                "/admin/pricing/imports/{batch}/proposals/{proposal}/confirm"
            )),
            json!({
                "model_id": imported_model, "effective_from": at, "attested": true
            }),
        ),
    ];
    let mut writes = Vec::new();
    for (url, body) in requests {
        let cookie = w.admin.cookie().to_string();
        writes.push(tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("HTTP client")
                .post(url)
                .header(reqwest::header::COOKIE, cookie)
                .json(&body)
                .send()
                .await
                .expect("send a real import request")
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                WHERE datname = current_database() AND wait_event_type = 'Lock' \
                AND query LIKE '%pricing_version%'",
            )
            .fetch_one(w.cp.db().pool())
            .await
            .expect("observe both imports waiting");
            if blocked == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both imports must actually wait on the price-version row");
    loop {
        let expired: bool =
            sqlx::query_scalar("SELECT clock_timestamp() > $1 + interval '5.1 seconds'")
                .bind(at)
                .fetch_one(w.cp.db().pool())
                .await
                .expect("wait beyond the tolerated start");
        if expired {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    held.rollback().await.expect("release the version row");
    let mut outcomes = Vec::new();
    for write in writes {
        let response = write.await.expect("import request task");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.expect("JSON refusal");
        outcomes.push((
            status,
            body["details"][0]["path"].as_str().map(str::to_string),
        ));
    }
    assert_eq!(
        outcomes,
        vec![(422, Some("/effective_from".to_string())); 2],
        "both catalogue and uploaded imports must refuse the expired start"
    );
    assert_eq!(w.cp.count("pricing_baselines").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);
    let status: String =
        sqlx::query_scalar("SELECT status FROM pricing_import_proposals WHERE id = $1")
            .bind(proposal)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("the proposal remains undecided");
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn a_discount_waits_for_its_tenant_before_taking_the_price_version() {
    let Some(w) = world().await else { return };
    let mut tenant = w.cp.db().pool().begin().await.expect("hold tenant");
    sqlx::query("SELECT id FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(w.tenant)
        .fetch_one(&mut *tenant)
        .await
        .expect("lock tenant row");
    let mut writer = PgConnection::connect(w.cp.database_url())
        .await
        .expect("discount writer");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut writer)
        .await
        .expect("writer pid");
    let tenant_id = w.tenant;
    let writing = tokio::spawn(async move {
        let mut tx = writer.begin().await.expect("begin discount");
        store::insert_discount(
            &mut tx,
            &NewDiscount {
                tenant_id,
                effective_from: Utc::now() + TimeDelta::minutes(1),
                discount_bps: 0,
            },
            Actor::System,
        )
        .await
        .expect("write discount after tenant unlocks");
        tx.rollback().await.expect("rollback discount");
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')")
                .bind(pid).fetch_one(w.cp.db().pool()).await.expect("observe tenant wait");
            if blocked { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("discount must actually wait for its tenant");
    let version_free = sqlx::query(
        "SELECT value FROM cp_settings WHERE key = 'pricing_version' FOR UPDATE NOWAIT",
    )
    .fetch_one(w.cp.db().pool())
    .await;
    tenant.rollback().await.expect("release tenant");
    writing.await.expect("discount finishes");
    assert!(version_free.is_ok(), "a waiting discount must leave the version free for a tenant-locked repricing apply: {version_free:?}");
}
