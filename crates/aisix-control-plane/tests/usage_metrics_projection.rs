//! 指标投影复用真实摄入、HTTP 和独立 PostgreSQL；期望来自公开计数及费用合同。
mod support;

use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::db::Db;
use aisix_control_plane::usage_metrics_projection::backfill_once;
use chrono::{SecondsFormat, TimeDelta, Utc};
use serde_json::{json, Value};
use support::world::{
    deliver, price, seed_key, seed_model, seed_tenant, tokens_680, usage, world, World,
};
use support::{BareDatabase, Resp, TestCp};
use uuid::Uuid;

const COUNTERS: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "reasoning_tokens",
    "total_tokens",
    "cached_prompt_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "cache_write_tokens",
    "status_code",
    "first_token_latency_ms",
    "downstream_latency_ms",
    "upstream_ttft_ms",
    "upstream_latency_ms",
];

const PRESENCE: &[&str] = &[
    "reasoning_tokens",
    "cached_prompt_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "cache_write_tokens",
    "cache_usage_reported",
];

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .get(name)
        .unwrap_or_else(|| panic!("缺少 {name}: {value}"))
}

async fn input(cp: &TestCp, id: &str) -> Value {
    database_input(cp.db(), id).await
}
async fn database_input(db: &Db, id: &str) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(p) FROM usage_metric_inputs p WHERE event_id = $1")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("读取持久投影")
}

async fn input_stamp(cp: &TestCp, id: &str) -> String {
    database_input_stamp(cp.db(), id).await
}
async fn database_input_stamp(db: &Db, id: &str) -> String {
    sqlx::query_scalar("SELECT xmin::text FROM usage_metric_inputs WHERE event_id = $1")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("读取投影行版本")
}

async fn stored(cp: &TestCp, id: &str) -> Value {
    sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("读取已存原事件")
}

async fn finances(cp: &TestCp) -> Vec<Value> {
    let mut rows = Vec::new();
    for table in [
        "pricing_charges_cny",
        "pricing_adjustments_cny",
        "billing_ledger_cny",
    ] {
        let query = format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM {table} t"
        );
        rows.push(
            sqlx::query_scalar(&query)
                .fetch_one(cp.db().pool())
                .await
                .expect("冻结原账务"),
        );
    }
    rows
}

fn audit() -> Value {
    json!({"version":1,"boundary":"client_exchange","request_body_bytes":91,
        "response_body_bytes":47,"response_complete":true,
        "input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true},
        "output_text":{"utf8_bytes":4,"unicode_scalars":1,"complete":true}})
}

async fn assert_bill(cp: &TestCp, id: &str, amount: i64) {
    let actual: (Option<i64>, i64, i64) = sqlx::query_as(
        "SELECT charge_micro_cny, \
          (SELECT count(*) FROM billing_ledger_cny WHERE event_id = $1), \
          (SELECT COALESCE(sum(amount_micro_cny)::bigint, 0) FROM billing_ledger_cny WHERE event_id = $1) \
         FROM pricing_charges_cny WHERE event_id = $1",
    ).bind(id).fetch_one(cp.db().pool()).await.expect("读取唯一收费及账本");
    assert_eq!(actual, (Some(amount), 1, -amount));
}

// 去掉范围、类型或小数形态检查，或者让 JSON null 吞掉存在位，都必须失败。
#[tokio::test]
async fn pure_projection_preserves_u32_lexemes_tpot_and_all_presence_states() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let attributes: (bool, String, String, f64) = sqlx::query_as(
        "SELECT proisstrict, provolatile::text, proparallel::text, prorows::double precision FROM pg_proc \
         WHERE oid = 'usage_metric_input_v1(jsonb)'::regprocedure",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("读取函数执行约束");
    assert_eq!(attributes, (true, "i".into(), "s".into(), 1.0));
    let absent: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_metric_input_v1(NULL::jsonb)")
        .fetch_one(cp.db().pool())
        .await
        .expect("STRICT 不调用 NULL 输入");
    assert_eq!(absent, 0);

    let cases = [
        ("0", Some(0)),
        ("-0", Some(0)),
        ("1", Some(1)),
        ("1.0", None),
        ("-0.0", None),
        ("1e0", Some(1)),
        ("1.00e2", Some(100)),
        ("1.00e1", None),
        ("0e-1", None),
        ("4294967295", Some(4_294_967_295)),
        ("4294967296", None),
        ("4.294967295e9", Some(4_294_967_295)),
        ("42949672950e-1", None),
        ("1e100000", None),
        ("-1", None),
        ("1.5", None),
        ("\"1\"", None),
        ("null", None),
        ("true", None),
        ("[]", None),
        ("{}", None),
    ];
    let raw: Vec<String> = cases.iter().map(|(s, _)| (*s).to_owned()).collect();
    let expected: Vec<Option<i64>> = cases.iter().map(|(_, want)| *want).collect();
    for name in COUNTERS {
        let sql = format!(
            "SELECT p.u32_{name} FROM unnest($1::text[]) WITH ORDINALITY AS v(raw, n) \
             CROSS JOIN LATERAL usage_metric_input_v1(jsonb_build_object($2::text, v.raw::jsonb)) p ORDER BY n"
        );
        let actual: Vec<Option<i64>> = sqlx::query_scalar(&sql)
            .bind(&raw)
            .bind(*name)
            .fetch_all(cp.db().pool())
            .await
            .expect("验证独立数字边界");
        assert_eq!(actual, expected, "{name}");
    }
    for (raw, expected) in [
        ("0", None),
        ("-1", None),
        ("1e-101", None),
        ("1e-100", Some(1e-100)),
        ("1.00e-100", Some(1e-100)),
        ("1.5", Some(1.5)),
        ("1e100", Some(1e100)),
        ("1e101", None),
        ("\"1\"", None),
        ("null", None),
        ("[]", None),
    ] {
        let actual: Option<f64> = sqlx::query_scalar(
            "SELECT tpot FROM usage_metric_input_v1(jsonb_build_object('upstream_tpot_ms', $1::jsonb))",
        ).bind(raw).fetch_one(cp.db().pool()).await.expect("验证 TPOT 独立边界");
        assert_eq!(actual, expected, "TPOT {raw}");
    }
    for (raw, present) in [
        ("null", Vec::<&str>::new()),
        ("[]", vec![]),
        ("true", vec![]),
        ("17", vec![]),
        ("{}", vec![]),
        ("\"reasoning_tokens\"", vec!["reasoning_tokens"]),
        ("\"cache_usage_reported\"", vec!["cache_usage_reported"]),
        (
            "[\"reasoning_tokens\",\"cache_write_tokens\"]",
            vec!["reasoning_tokens", "cache_write_tokens"],
        ),
        ("[{\"reasoning_tokens\":1}]", vec![]),
    ] {
        let projected: Value =
            sqlx::query_scalar("SELECT to_jsonb(p) FROM usage_metric_input_v1($1::jsonb) p")
                .bind(raw)
                .fetch_one(cp.db().pool())
                .await
                .expect("旧根值仍产生一行投影");
        assert_eq!(projected.as_object().expect("完整投影对象").len(), 33);
        for name in COUNTERS {
            assert!(
                field(&projected, &format!("u32_{name}")).is_null(),
                "{raw}: {name}"
            );
        }
        for name in PRESENCE {
            assert_eq!(
                field(&projected, &format!("has_{name}")),
                &json!(present.contains(name)),
                "{raw}: {name}"
            );
        }
    }
    let raw = json!({"is_final_attempt":null,"cache_write_tokens":null,"reasoning_tokens":null,
        "cache_usage_reported":null,"upstream_dispatched":"true","model_id":"UPPER-Case-Raw",
        "provider_key_id":"invalid-raw-provider","error_class":17,"cache_status":["hit"]});
    let explicit: (bool, bool, bool, bool) = sqlx::query_as(
        "SELECT json_is_final_attempt IS NOT NULL, json_cache_write_tokens IS NOT NULL, \
          json_cache_usage_reported IS NOT NULL, has_reasoning_tokens FROM usage_metric_input_v1($1)",
    ).bind(&raw).fetch_one(cp.db().pool()).await.expect("JSON null 不能折成 SQL NULL");
    assert_eq!(explicit, (true, true, true, true));
    let projected: Value =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM usage_metric_input_v1($1) p")
            .bind(raw)
            .fetch_one(cp.db().pool())
            .await
            .expect("读取原始文本与类型");
    assert_eq!(field(&projected, "text_model_id"), "UPPER-Case-Raw");
    assert_eq!(
        field(&projected, "text_provider_key_id"),
        "invalid-raw-provider"
    );
    assert_eq!(field(&projected, "text_error_class"), "17");
    assert_eq!(field(&projected, "text_cache_status"), "[\"hit\"]");
    assert_eq!(field(&projected, "json_upstream_dispatched"), "true");
}

// 原 writer 的多行 INSERT/UPDATE、同版本刷新和回滚不能依赖新版应用额外写入。
#[tokio::test]
async fn legacy_sql_statements_project_all_rows_refresh_same_version_and_roll_back_together() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let raw = [
        "null",
        "[]",
        "\"reasoning_tokens\"",
        "{\"prompt_tokens\":1.0}",
        "{\"prompt_tokens\":4294967295}",
        "{\"prompt_tokens\":1.00e2}",
    ];
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO usage_events(event_id, dp_id, batch_id, occurred_at, event) \
         SELECT 'legacy-input-' || n, $1, $1, now(), v::jsonb \
         FROM unnest($2::text[]) WITH ORDINALITY AS x(v,n)",
    )
    .bind(id)
    .bind(raw.map(str::to_owned).to_vec())
    .execute(cp.db().pool())
    .await
    .expect("旧 SQL 批量写入");
    let actual: Vec<(i32, Option<i64>)> = sqlx::query_as(
        "SELECT projection_version, u32_prompt_tokens FROM usage_metric_inputs ORDER BY event_id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("全部旧行都有投影");
    assert_eq!(
        actual,
        vec![
            (1, None),
            (1, None),
            (1, None),
            (1, None),
            (1, Some(4_294_967_295)),
            (1, Some(100))
        ]
    );
    sqlx::query(
        "UPDATE usage_events SET event = '{\"prompt_tokens\":21,\"completion_tokens\":0}'::jsonb",
    )
    .execute(cp.db().pool())
    .await
    .expect("同版本批量替换原事件");
    let refreshed: Vec<(i32, Option<i64>, bool)> = sqlx::query_as(
        "SELECT projection_version, u32_prompt_tokens, has_reasoning_tokens FROM usage_metric_inputs ORDER BY event_id",
    ).fetch_all(cp.db().pool()).await.expect("同版本应刷新全部投影");
    assert_eq!(refreshed, vec![(1, Some(21), false); 6]);
    let before = input(&cp, "legacy-input-1").await;
    let mut tx = cp.db().pool().begin().await.expect("开始回滚场景");
    sqlx::query("UPDATE usage_events SET event = '{\"prompt_tokens\":99}'::jsonb WHERE event_id = 'legacy-input-1'")
        .execute(&mut *tx).await.expect("事务内改变原事件");
    let during: Option<i64> = sqlx::query_scalar(
        "SELECT u32_prompt_tokens FROM usage_metric_inputs WHERE event_id = 'legacy-input-1'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("投影在原事务内同步");
    assert_eq!(during, Some(99));
    sqlx::query("INSERT INTO usage_events(event_id,dp_id,batch_id,occurred_at,event) VALUES ('rolled-back-input',$1,$1,now(),'{}')")
        .bind(id).execute(&mut *tx).await.expect("事务内写入新原事件");
    let inserted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_metric_inputs WHERE event_id = 'rolled-back-input'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("事务内新投影可见");
    assert_eq!(inserted, 1);
    tx.rollback().await.expect("回滚原事件及投影");
    assert_eq!(input(&cp, "legacy-input-1").await, before);
    assert_eq!(stored(&cp, "legacy-input-1").await["prompt_tokens"], 21);
    for table in ["usage_events", "usage_metric_inputs"] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE event_id = 'rolled-back-input'"
        ))
        .fetch_one(cp.db().pool())
        .await
        .expect("回滚不能留下半条投影");
        assert_eq!(count, 0, "{table}");
    }
}

#[tokio::test]
async fn real_ingest_keeps_first_duplicate_and_does_not_rewrite_projection_for_io_only_updates() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let mut original = usage(&w, "input-first", model, tokens_680());
    original["is_final_attempt"] = json!(true);
    original["first_token_latency_ms"] = json!(0);
    let accepted = deliver(&w.dp, Uuid::new_v4(), vec![original.clone()]).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    let first = input(&w.cp, "input-first").await;
    assert_eq!(field(&first, "projection_version"), 1);
    assert_eq!(field(&first, "u32_prompt_tokens"), 1000);
    assert_eq!(field(&first, "u32_cached_prompt_tokens"), 800);
    assert_eq!(field(&first, "u32_completion_tokens"), 200);
    assert_eq!(field(&first, "u32_first_token_latency_ms"), 0);
    let stamp = input_stamp(&w.cp, "input-first").await;
    let mut conflicting = original.clone();
    conflicting["prompt_tokens"] = json!(999_999);
    conflicting["is_final_attempt"] = json!(false);
    let duplicate = deliver(&w.dp, Uuid::new_v4(), vec![conflicting]).await;
    assert_eq!(duplicate.status, 200, "{duplicate:?}");
    assert_eq!(
        duplicate.body,
        json!({"accepted":0,"rejected":0,"duplicates":1})
    );
    assert_eq!(stored(&w.cp, "input-first").await, original);
    assert_eq!(input(&w.cp, "input-first").await, first);
    assert_eq!(input_stamp(&w.cp, "input-first").await, stamp);
    let amendment = json!({"kind":"io_audit_update","event_id":"input-first","io_audit":audit()});
    let updated = deliver(&w.dp, Uuid::new_v4(), vec![amendment]).await;
    assert_eq!(updated.status, 200, "{updated:?}");
    assert_eq!(
        updated.body,
        json!({"accepted":0,"rejected":0,"duplicates":0})
    );
    original["io_audit"] = audit();
    assert_eq!(stored(&w.cp, "input-first").await, original);
    assert_eq!(input(&w.cp, "input-first").await, first);
    assert_eq!(input_stamp(&w.cp, "input-first").await, stamp);
    assert_bill(&w.cp, "input-first", 680).await;
}

#[tokio::test]
async fn a_failed_real_ingest_rolls_back_projection_and_can_replay_the_same_batch() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_projection_charge() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'projection fixture failure' USING ERRCODE = '53300'; END $$; \
         CREATE TRIGGER fail_projection_charge BEFORE INSERT ON pricing_charges_cny \
         FOR EACH ROW EXECUTE FUNCTION fail_projection_charge();",
    )
    .execute(w.cp.db().pool())
    .await
    .expect("在独立测试库注入计价失败");
    let batch = Uuid::new_v4();
    let row = usage(&w, "input-failed", model, tokens_680());
    let failed = deliver(&w.dp, batch, vec![row.clone()]).await;
    assert_eq!(failed.status, 503, "{failed:?}");
    assert_eq!(failed.body["code"], "unavailable");
    for table in [
        "usage_events",
        "usage_metric_inputs",
        "pricing_charges_cny",
        "billing_ledger_cny",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE event_id = 'input-failed'"
        ))
        .fetch_one(w.cp.db().pool())
        .await
        .expect("失败摄入不能留下部分数据");
        assert_eq!(count, 0, "{table}");
    }
    let batches: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_batches WHERE batch_id = $1")
        .bind(batch)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("失败批次也回滚");
    assert_eq!(batches, 0);
    sqlx::query("DROP TRIGGER fail_projection_charge ON pricing_charges_cny")
        .execute(w.cp.db().pool())
        .await
        .expect("解除测试失败条件");
    let replayed = deliver(&w.dp, batch, vec![row]).await;
    assert_eq!(replayed.status, 200, "{replayed:?}");
    assert_eq!(
        replayed.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    assert_eq!(
        field(&input(&w.cp, "input-failed").await, "u32_prompt_tokens"),
        1000
    );
    assert_bill(&w.cp, "input-failed", 680).await;
}

async fn adjustment(w: &World, event_id: &str, delta: i64) {
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id,status,filter,reason,pricing_version,engine_version,events,changed,still_unpriced, \
          skipped_unattributed,delta_micro_cny,by_tenant,created_by,applied_by,applied_at) \
         VALUES ($1,'applied','{}','投影账务对照',0,$2,1,1,0,0,$3,$4,$5,$5,now())",
    )
    .bind(run)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .bind(delta)
    .bind(json!([{"tenant_id":w.tenant,"events":1,"changed":1,"delta_micro_cny":delta}]))
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("记录真实修正批次");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny(run_id,event_id,delta_micro_cny,engine_version,reason,created_by) \
         VALUES ($1,$2,$3,$4,'投影账务对照',$5)",
    ).bind(run).bind(event_id).bind(delta).bind(i64::from(aisix_pricing::ENGINE_VERSION))
        .bind(w.admin.id).execute(w.cp.db().pool()).await.expect("记录追加式费用修正");
}

async fn snapshots(cp: &TestCp, requests: &[(String, String)]) -> Vec<Value> {
    let mut result = Vec::new();
    for (path, cookie) in requests {
        let response = cp.send("GET", path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{path}: {response:?}");
        assert!(
            response.body.get("totals").is_some_and(Value::is_object),
            "{path}"
        );
        result.push(response.body);
    }
    result
}

async fn pin_fixture_backfill_complete(cp: &TestCp) {
    // 本库尚无 UE；正常 writer 仍由触发器维护，人为旧行缺口不交给后台抢补。
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events")
        .fetch_one(cp.db().pool())
        .await
        .expect("固定空 fixture 的回填起点");
    assert_eq!(events, 0);
    let changed = sqlx::query("UPDATE usage_metric_input_backfill SET after_seq = 0, high_seq = 0, complete = true WHERE version = 1")
        .execute(cp.db().pool()).await.expect("独立测试库先完成空历史");
    assert_eq!(changed.rows_affected(), 1);
}

// marker=1 的坏值必须改变响应；仅改成未知版本后又必须整份恢复，防止永远走 fallback 的假通过。
#[tokio::test]
async fn all_metrics_scopes_keep_complete_responses_for_missing_and_poisoned_unknown_inputs() {
    let Some(w) = world().await else { return };
    pin_fixture_backfill_complete(&w.cp).await;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let provider: String =
        sqlx::query_scalar("SELECT doc->>'provider_key_id' FROM gw_resources WHERE id = $1")
            .bind(model)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("读取公开模型账号 ID");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let member_key = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let outside = seed_tenant(&w.cp, &w.admin).await;
    let outside_key = seed_key(&w.cp, &w.admin, outside, None).await;
    let from = Utc::now() - TimeDelta::minutes(1);
    let to = Utc::now() + TimeDelta::minutes(1);
    let mut events = Vec::new();
    for (index, (id, key, final_attempt)) in [
        ("input-api-success", w.key.as_str(), true),
        ("input-api-retry", w.key.as_str(), false),
        ("input-api-member", member_key.as_str(), true),
        ("input-api-foreign", outside_key.as_str(), true),
    ]
    .into_iter()
    .enumerate()
    {
        let mut event = usage(&w, id, model, tokens_680());
        for (key, value) in [
            ("api_key_id", json!(key)),
            ("model_id", json!(model)),
            ("provider_key_id", json!(provider)),
            ("is_final_attempt", json!(final_attempt)),
            ("upstream_dispatched", json!(true)),
            ("first_token_latency_ms", json!(index * 10)),
            ("downstream_latency_ms", json!(10 + index * 10)),
            ("upstream_latency_ms", json!(100 + index * 100)),
            ("upstream_ttft_ms", json!(20)),
            ("upstream_tpot_ms", json!(5)),
            ("cache_status", json!("miss")),
            ("status_code", json!(if final_attempt { 200 } else { 503 })),
            (
                "error_class",
                json!(if final_attempt { "" } else { "upstream_status" }),
            ),
        ] {
            event[key] = value;
        }
        events.push(event);
    }
    let delivered = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(
        delivered.body,
        json!({"accepted":4,"rejected":0,"duplicates":0})
    );
    adjustment(&w, "input-api-success", 7).await;
    adjustment(&w, "input-api-success", 11).await;
    let range = format!(
        "from={}&to={}&bucket=hour",
        from.to_rfc3339_opts(SecondsFormat::Secs, true),
        to.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let requests = vec![
        (format!("/api/usage/metrics?{range}"), owner.cookie().to_owned()),
        (format!("/api/usage/metrics?{range}"), member.cookie().to_owned()),
        (format!("/api/usage/metrics?{range}&member_id={}", w.member), member.cookie().to_owned()),
        (format!("/api/usage/metrics?{range}&api_key_id={}", w.key), owner.cookie().to_owned()),
        (format!("/admin/usage/metrics?{range}"), w.admin.cookie().to_owned()),
        (format!("/admin/usage/metrics?{range}&tenant_id={}", w.tenant), w.admin.cookie().to_owned()),
        (format!("/admin/tenants/{}/usage/metrics?{range}", w.tenant), w.admin.cookie().to_owned()),
        (format!("/admin/usage/direct-models?{range}"), w.admin.cookie().to_owned()),
        (format!("/admin/usage/direct-models?{range}&tenant_id={}", w.tenant), w.admin.cookie().to_owned()),
        (format!("/admin/usage/direct-models?{range}&model_id={model}&provider_key_id={provider}&api_key_id={}", w.key), w.admin.cookie().to_owned()),
    ];
    let baseline = snapshots(&w.cp, &requests).await;
    assert_eq!(baseline[0]["totals"]["requests"], 2);
    assert_eq!(baseline[0]["totals"]["billing_attempts"], 3);
    assert_eq!(baseline[0]["totals"]["tokens"]["prompt_tokens"], 3000);
    assert_eq!(baseline[0]["totals"]["charged_micro_cny"], "2058");
    assert_eq!(baseline[0]["totals"]["adjusted_micro_cny"], "18");
    assert_eq!(baseline[1]["totals"]["requests"], 1);
    assert_eq!(baseline[1]["totals"]["charged_micro_cny"], "680");
    assert_eq!(baseline[2]["totals"]["requests"], 0);
    assert_eq!(baseline[3]["totals"]["charged_micro_cny"], "1378");
    assert_eq!(baseline[4]["totals"]["requests"], 3);
    assert_eq!(baseline[4]["totals"]["charged_micro_cny"], "2738");
    assert_eq!(baseline[7]["totals"]["attempts"], 4);
    assert_eq!(baseline[8]["totals"]["attempts"], 3);
    assert_eq!(baseline[9]["totals"]["attempts"], 2);
    let forbidden =
        w.cp.send("GET", &requests[7].0, Some(owner.cookie()), None)
            .await;
    assert_eq!(forbidden.status, 403, "{forbidden:?}");
    let money = finances(&w.cp).await;
    sqlx::query("DELETE FROM usage_metric_inputs WHERE event_id = 'input-api-success'")
        .execute(w.cp.db().pool())
        .await
        .expect("制造一条真实历史缺投影");
    assert_eq!(snapshots(&w.cp, &requests).await, baseline);
    sqlx::query(
        "UPDATE usage_metric_inputs SET u32_prompt_tokens = 0 WHERE event_id LIKE 'input-api-%'",
    )
    .execute(w.cp.db().pool())
    .await
    .expect("已知版本坏值必须真的参与读取");
    let poisoned = snapshots(&w.cp, &requests).await;
    assert_ne!(poisoned[0], baseline[0], "请求指标不能永远绕过 ready 投影");
    assert_ne!(poisoned[7], baseline[7], "直连指标不能永远绕过 ready 投影");
    sqlx::query(
        "UPDATE usage_metric_inputs SET projection_version = 9 WHERE event_id LIKE 'input-api-%'",
    )
    .execute(w.cp.db().pool())
    .await
    .expect("未知版本不能相信其中的坏值");
    assert_eq!(snapshots(&w.cp, &requests).await, baseline);
    sqlx::query("DELETE FROM usage_metric_inputs WHERE event_id LIKE 'input-api-%'")
        .execute(w.cp.db().pool())
        .await
        .expect("全部旧行使用 fallback");
    assert_eq!(snapshots(&w.cp, &requests).await, baseline);
    sqlx::query("UPDATE usage_events SET event = event WHERE event_id LIKE 'input-api-%'")
        .execute(w.cp.db().pool())
        .await
        .expect("旧 UPDATE writer 同步补齐缺投影");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_metric_inputs WHERE event_id LIKE 'input-api-%' AND projection_version = 1")
        .fetch_one(w.cp.db().pool()).await.expect("重新得到四条 ready 投影");
    assert_eq!(count, 4);
    assert_eq!(snapshots(&w.cp, &requests).await, baseline);
    assert_eq!(finances(&w.cp).await, money, "投影或 fallback 不得改写账务");
}

async fn historical_rows(db: &Db, count: i64) {
    sqlx::query(
        "INSERT INTO usage_events(event_id,dp_id,batch_id,occurred_at,event) \
         SELECT 'input-history-' || n, $1, $1, now(), jsonb_build_object('prompt_tokens',n) \
         FROM generate_series(1,$2::bigint) n ORDER BY n",
    )
    .bind(Uuid::new_v4())
    .bind(count)
    .execute(db.pool())
    .await
    .expect("准备真实历史候选");
}

#[tokio::test]
async fn backfill_can_pause_resume_without_downgrading_future_inputs_or_extending_high_water() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    historical_rows(db, 258).await;
    sqlx::query("UPDATE usage_metric_inputs SET projection_version = 9, u32_prompt_tokens = 999 WHERE event_id = 'input-history-1'")
        .execute(db.pool()).await.expect("保留未来版本原行");
    let future = database_input(db, "input-history-1").await;
    let future_stamp = database_input_stamp(db, "input-history-1").await;
    sqlx::query("DELETE FROM usage_metric_inputs WHERE event_id <> 'input-history-1'")
        .execute(db.pool())
        .await
        .expect("模拟未回填的历史");
    let first = backfill_once(db).await.expect("第一批有限回填");
    assert_eq!(first.examined, 256);
    assert_eq!(first.written, 255);
    assert_eq!(first.deferred, 0);
    assert!(!first.complete);
    assert_eq!(database_input(db, "input-history-1").await, future);
    assert_eq!(
        database_input_stamp(db, "input-history-1").await,
        future_stamp
    );
    let high: i64 =
        sqlx::query_scalar("SELECT high_seq FROM usage_metric_input_backfill WHERE version = 1")
            .fetch_one(db.pool())
            .await
            .expect("冻结历史上界");
    let missing: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events ue LEFT JOIN usage_metric_inputs p USING(event_id) WHERE p.event_id IS NULL")
        .fetch_one(db.pool()).await.expect("暂停时仍保留两个待补洞");
    assert_eq!(missing, 2);
    sqlx::query("INSERT INTO usage_events(event_id,dp_id,batch_id,occurred_at,event) VALUES ('input-live-after-high',$1,$1,now(),'{\"prompt_tokens\":777}')")
        .bind(Uuid::new_v4()).execute(db.pool()).await.expect("暂停期间真实 writer 继续工作");
    assert_eq!(
        field(
            &database_input(db, "input-live-after-high").await,
            "u32_prompt_tokens"
        ),
        777
    );
    let resumed = backfill_once(db).await.expect("恢复第二批历史回填");
    assert_eq!(resumed.examined, 2);
    assert_eq!(resumed.written, 2);
    assert_eq!(resumed.deferred, 0);
    assert!(resumed.complete);
    let state: (i64, i64, bool) = sqlx::query_as(
        "SELECT after_seq, high_seq, complete FROM usage_metric_input_backfill WHERE version = 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("完成状态不追逐实时写入");
    assert_eq!(state, (high, high, true));
    assert_eq!(database_input(db, "input-history-1").await, future);
    assert_eq!(
        database_input_stamp(db, "input-history-1").await,
        future_stamp
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_metric_inputs")
        .fetch_one(db.pool())
        .await
        .expect("历史与实时均有投影");
    assert_eq!(count, 259);
}

#[tokio::test]
async fn backfill_does_not_jump_a_locked_first_row_and_completed_runs_never_scan_history() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    historical_rows(db, 3).await;
    sqlx::query("DELETE FROM usage_metric_inputs")
        .execute(db.pool())
        .await
        .expect("准备历史缺口");
    let mut held = db.pool().begin().await.expect("持有首个历史行");
    sqlx::query("SELECT event_id FROM usage_events ORDER BY seq LIMIT 1 FOR UPDATE")
        .fetch_one(&mut *held)
        .await
        .expect("真实行锁已获取");
    let attempt = tokio::time::timeout(Duration::from_secs(2), backfill_once(db)).await;
    held.rollback()
        .await
        .expect("先释放锁再报告超时，避免清理挂住");
    let blocked = attempt
        .expect("SKIP LOCKED 回填不能等待首行锁")
        .expect("返回延后结果");
    assert!(blocked.deferred > 0, "首洞必须记为 deferred");
    assert!(!blocked.complete, "跳锁不代表历史已完成");
    let cursor: i64 =
        sqlx::query_scalar("SELECT after_seq FROM usage_metric_input_backfill WHERE version = 1")
            .fetch_one(db.pool())
            .await
            .expect("检查连续前缀游标");
    assert_eq!(cursor, 0, "游标不得越过首洞");
    let resumed = backfill_once(db).await.expect("解锁后重新访问首洞");
    assert!(resumed.complete);
    for n in 1..=3 {
        assert_eq!(
            field(
                &database_input(db, &format!("input-history-{n}")).await,
                "u32_prompt_tokens"
            ),
            &json!(n)
        );
    }
    let mut exclusive = db.pool().begin().await.expect("完成后封住历史表");
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *exclusive)
        .await
        .expect("限制测试锁获取等待");
    sqlx::query("LOCK TABLE usage_events IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *exclusive)
        .await
        .expect("已获得历史表排他锁");
    let attempt = tokio::time::timeout(Duration::from_secs(2), backfill_once(db)).await;
    exclusive.rollback().await.expect("先释放排他锁");
    let done = attempt
        .expect("完成标志应使回填完全跳过历史表")
        .expect("完成状态可重复调用");
    assert!(done.complete);
    assert_eq!((done.examined, done.written, done.deferred), (0, 0, 0));
}

async fn worker_checkpoint(cp: &TestCp) -> Result<(i64, i64, Option<i64>, bool), sqlx::Error> {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM usage_metric_inputs), after_seq, high_seq, complete \
         FROM usage_metric_input_backfill WHERE version = 1",
    )
    .fetch_one(cp.db().pool())
    .await
}

// 删除 housekeeping 的回填分支必须超时失败；此场景从不手动调用 backfill_once。
#[tokio::test]
async fn real_background_worker_recovers_a_locked_historical_hole_across_control_plane_restarts() {
    let Some(mut cp) = TestCp::start().await else {
        return;
    };
    pin_fixture_backfill_complete(&cp).await;
    sqlx::query("ALTER TABLE usage_events DISABLE TRIGGER usage_metric_inputs_insert")
        .execute(cp.db().pool())
        .await
        .expect("仅关闭本测试库的统计 INSERT 触发器");
    let seeded = sqlx::query(
        "INSERT INTO usage_events(event_id,dp_id,batch_id,occurred_at,event) \
         SELECT 'input-worker-' || n, $1, $1, now(), jsonb_build_object('prompt_tokens',n) \
         FROM generate_series(1,3) n ORDER BY n",
    )
    .bind(Uuid::new_v4())
    .execute(cp.db().pool())
    .await;
    // 先恢复已提交的触发器状态，再处理造数错误。
    sqlx::query("ALTER TABLE usage_events ENABLE TRIGGER usage_metric_inputs_insert")
        .execute(cp.db().pool())
        .await
        .expect("恢复统计 INSERT 触发器");
    assert_eq!(
        seeded.expect("准备三条真实旧 writer 历史").rows_affected(),
        3
    );
    let missing: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_metric_inputs")
        .fetch_one(cp.db().pool())
        .await
        .expect("后台尚未恢复，必须确实没有投影");
    assert_eq!(missing, 0);
    let high: i64 = sqlx::query_scalar("SELECT max(seq) FROM usage_events")
        .fetch_one(cp.db().pool())
        .await
        .expect("已存历史的固定上界");
    let money = finances(&cp).await;
    let mut held = cp.db().pool().begin().await.expect("持有跨重启的首行锁");
    let first: String =
        sqlx::query_scalar("SELECT event_id FROM usage_events ORDER BY seq LIMIT 1 FOR UPDATE")
            .fetch_one(&mut *held)
            .await
            .expect("首行 FOR UPDATE 已真实获得");
    assert_eq!(first, "input-worker-1");
    let reset = sqlx::query(
        "UPDATE usage_metric_input_backfill SET after_seq = 0, high_seq = NULL, complete = false \
         WHERE version = 1",
    )
    .execute(cp.db().pool())
    .await;
    if let Err(error) = reset {
        held.rollback().await.expect("先释放首行锁");
        panic!("恢复后台回填状态失败: {error}");
    }
    let restarted = tokio::time::timeout(Duration::from_secs(10), cp.restart()).await;
    let next = match restarted {
        Ok(next) => next,
        Err(error) => {
            held.rollback().await.expect("重启超时前先释放首行锁");
            panic!("首洞锁不能阻止控制面重启: {error}");
        }
    };
    drop(std::mem::replace(&mut cp, next));
    let first_progress = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = worker_checkpoint(&cp).await?;
            if state.0 >= 2 || state.1 != 0 || state.3 {
                return Ok::<_, sqlx::Error>(state);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    let expected = (2, 0, Some(high), false);
    let checkpoint = match first_progress {
        Ok(Ok(state)) if state == expected => state,
        other => {
            held.rollback().await.expect("后台失败或超时后先释放首行锁");
            panic!("没有新 UE 流量时，后台应补后两行但保留首洞；expected={expected:?}, actual={other:?}");
        }
    };
    let restarted = tokio::time::timeout(Duration::from_secs(10), cp.restart()).await;
    let next = match restarted {
        Ok(next) => next,
        Err(error) => {
            held.rollback().await.expect("再次重启超时前先释放首行锁");
            panic!("保留 checkpoint 的重启失败: {error}");
        }
    };
    drop(std::mem::replace(&mut cp, next));
    let persisted = tokio::time::timeout(Duration::from_secs(10), worker_checkpoint(&cp)).await;
    let persisted = match persisted {
        Ok(Ok(state)) => state,
        other => {
            held.rollback()
                .await
                .expect("检查 checkpoint 失败先释放首行锁");
            panic!("重启后无法读取 checkpoint: {other:?}");
        }
    };
    held.rollback().await.expect("第二次重启后释放历史首洞");
    assert_eq!(persisted, checkpoint, "控制面重启不能重置持久 checkpoint");
    let complete = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = worker_checkpoint(&cp).await?;
            if state.0 >= 3 && state.3 {
                return Ok::<_, sqlx::Error>(state);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("释放首洞后，新控制面的真实 worker 必须恢复回填")
    .expect("读取后台完成状态");
    assert_eq!(complete, (3, high, Some(high), true));
    for n in 1..=3 {
        let projected = input(&cp, &format!("input-worker-{n}")).await;
        assert_eq!(field(&projected, "projection_version"), 1);
        assert_eq!(field(&projected, "u32_prompt_tokens"), &json!(n));
    }
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events")
        .fetch_one(cp.db().pool())
        .await
        .expect("恢复过程未借助新的遥测流量");
    assert_eq!(events, 3);
    assert_eq!(finances(&cp).await, money, "历史后台只补投影，不改账务");
}

async fn stats_triggers(cp: &TestCp, enabled: bool) -> Result<(), sqlx::Error> {
    let mut tx = cp.db().pool().begin().await?;
    for name in ["usage_metric_inputs_insert", "usage_metric_inputs_update"] {
        let action = if enabled { "ENABLE" } else { "DISABLE" };
        sqlx::query(&format!("ALTER TABLE usage_events {action} TRIGGER {name}"))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

async fn wal_position(cp: &TestCp) -> Result<Option<String>, sqlx::Error> {
    match sqlx::query_scalar("SELECT pg_current_wal_lsn()::text")
        .fetch_one(cp.db().pool())
        .await
    {
        Ok(value) => Ok(Some(value)),
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42501") => Ok(None),
        Err(error) => Err(error),
    }
}

async fn wal_bytes(cp: &TestCp, before: Option<String>, after: Option<String>) -> Option<String> {
    match (before, after) {
        (Some(before), Some(after)) => Some(
            sqlx::query_scalar("SELECT pg_wal_lsn_diff($1::pg_lsn, $2::pg_lsn)::text")
                .bind(after)
                .bind(before)
                .fetch_one(cp.db().pool())
                .await
                .expect("读取可用的全局 WAL 差值"),
        ),
        _ => None,
    }
}

fn measured_batches(w: &World, model: Uuid, prefix: &str) -> Vec<(Uuid, Vec<u8>)> {
    (0..10)
        .map(|batch| {
            let events: Vec<Value> = (0..50)
                .map(|row| {
                    let id = format!("{prefix}-{}", batch * 50 + row);
                    let mut event = usage(w, &id, model, tokens_680());
                    event["is_final_attempt"] = json!(true);
                    event["first_token_latency_ms"] = json!(0);
                    event["downstream_latency_ms"] = json!(20);
                    event["upstream_latency_ms"] = json!(100);
                    event["upstream_ttft_ms"] = json!(5);
                    event["upstream_tpot_ms"] = json!(1.5);
                    let payload: String = (0..96)
                        .map(|_| Uuid::new_v4().simple().to_string())
                        .collect();
                    assert_eq!(payload.len(), 3072);
                    event["payload"] = json!(payload);
                    event
                })
                .collect();
            (
                Uuid::new_v4(),
                serde_json::to_vec(&json!({"events":events})).expect("在关闭触发器前准备合法批次"),
            )
        })
        .collect()
}

async fn send_measured_batches(
    w: &World,
    batches: &[(Uuid, Vec<u8>)],
) -> Result<Vec<Resp>, reqwest::Error> {
    let mut responses = Vec::new();
    for (id, body) in batches {
        let response =
            w.dp.telemetry_raw(Some(&id.to_string()), body.clone())
                .await?;
        let success = response.status == 200;
        responses.push(response);
        if !success {
            break;
        }
    }
    Ok(responses)
}

fn assert_measured_batches(responses: Vec<Resp>) {
    assert_eq!(
        responses.len(),
        10,
        "每阶段必须完成十个真实 HTTP 批次: {responses:?}"
    );
    for response in responses {
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.body,
            json!({"accepted":50,"rejected":0,"duplicates":0})
        );
    }
}

async fn assert_measured_phase(cp: &TestCp, prefix: &str, inputs: i64) {
    let pattern = format!("{prefix}-%");
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_events WHERE event_id LIKE $1")
            .bind(&pattern)
            .fetch_one(cp.db().pool())
            .await
            .expect("实收五百条不同事件");
    assert_eq!(events, 500);
    let charges: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), (count(*) FILTER (WHERE status = 'charged'))::bigint, \
         COALESCE(sum(charge_micro_cny)::bigint, 0), \
         (count(*) FILTER (WHERE charge_micro_cny IS DISTINCT FROM 680))::bigint \
         FROM pricing_charges_cny WHERE event_id LIKE $1",
    )
    .bind(&pattern)
    .fetch_one(cp.db().pool())
    .await
    .expect("每次真实调用仍收 680");
    assert_eq!(charges, (500, 500, 340_000, 0));
    let ledger: (i64, i64) = sqlx::query_as(
        "SELECT count(*), COALESCE(sum(amount_micro_cny)::bigint, 0) FROM billing_ledger_cny WHERE event_id LIKE $1",
    ).bind(&pattern).fetch_one(cp.db().pool()).await.expect("真实账本不丢失或重复");
    assert_eq!(ledger, (500, -340_000));
    let projected: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), (count(*) FILTER (WHERE projection_version = 1))::bigint, \
         COALESCE(sum(u32_prompt_tokens) FILTER (WHERE projection_version = 1), 0)::bigint \
         FROM usage_metric_inputs WHERE event_id LIKE $1",
    )
    .bind(pattern)
    .fetch_one(cp.db().pool())
    .await
    .expect("检查投影覆盖及原始量");
    assert_eq!(projected, (inputs, inputs, inputs * 1000));
}

// 仅观察开销，不把噪声或样本规模当成提速通过阈值；全部费用与账本必须严格相等。
#[tokio::test]
async fn real_http_ingest_observes_projection_overhead_without_changing_billing_or_duplicate_io_behavior(
) {
    let Some(w) = world().await else { return };
    pin_fixture_backfill_complete(&w.cp).await;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let credit =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", w.tenant),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"2","reason":"真实摄入投影开销测试余额"})),
        )
        .await;
    assert_eq!(
        credit.status, 201,
        "必须先合法入账两元，不能降费用绕过余额: {credit:?}"
    );
    let baseline_batches = measured_batches(&w, model, "input-observe-baseline");
    let enabled_batches = measured_batches(&w, model, "input-observe-enabled");

    let disabled = stats_triggers(&w.cp, false).await;
    if let Err(error) = disabled {
        stats_triggers(&w.cp, true)
            .await
            .expect("关闭失败也恢复两个统计触发器");
        panic!("独立测试库关闭统计触发器失败: {error}");
    }
    let baseline_before = wal_position(&w.cp).await;
    let started = Instant::now();
    let baseline = send_measured_batches(&w, &baseline_batches).await;
    let baseline_elapsed = started.elapsed();
    let baseline_after = wal_position(&w.cp).await;
    // 在检查 HTTP、网络或观测错误前，先恢复已提交的 DDL 状态。
    stats_triggers(&w.cp, true)
        .await
        .expect("必须恢复两个统计触发器");
    let baseline_wal = wal_bytes(
        &w.cp,
        baseline_before.expect("读取基线起点"),
        baseline_after.expect("读取基线终点"),
    )
    .await;
    assert_measured_batches(baseline.expect("基线真实 HTTP 摄入必须有完整响应"));
    assert_measured_phase(&w.cp, "input-observe-baseline", 0).await;

    let enabled_before = wal_position(&w.cp).await.expect("读取开启投影的起点");
    let started = Instant::now();
    let enabled = send_measured_batches(&w, &enabled_batches).await;
    let enabled_elapsed = started.elapsed();
    let enabled_after = wal_position(&w.cp).await.expect("读取开启投影的终点");
    let enabled_wal = wal_bytes(&w.cp, enabled_before, enabled_after).await;
    assert_measured_batches(enabled.expect("开启投影的真实 HTTP 摄入必须有完整响应"));
    assert_measured_phase(&w.cp, "input-observe-enabled", 500).await;
    // WAL 属于整个 PG 实例，可能含其他测试事务，不能说成单个 fixture 的精确写放大。
    eprintln!("usage metric input HTTP observation: rows_per_phase=500 batches=10 batch_rows=50 payload_bytes=3072 baseline_ms={:.3} enabled_ms={:.3} baseline_global_wal_bytes={baseline_wal:?} enabled_global_wal_bytes={enabled_wal:?}",
        baseline_elapsed.as_secs_f64() * 1000.0, enabled_elapsed.as_secs_f64() * 1000.0);

    let stamp = input_stamp(&w.cp, "input-observe-enabled-0").await;
    let projection = input(&w.cp, "input-observe-enabled-0").await;
    let mut duplicate = usage(&w, "input-observe-enabled-0", model, tokens_680());
    duplicate["prompt_tokens"] = json!(999_999);
    let response = deliver(&w.dp, Uuid::new_v4(), vec![duplicate]).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.body,
        json!({"accepted":0,"rejected":0,"duplicates":1})
    );
    let response = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![json!({"kind":"io_audit_update",
        "event_id":"input-observe-enabled-0","io_audit":audit()})],
    )
    .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.body,
        json!({"accepted":0,"rejected":0,"duplicates":0})
    );
    assert_eq!(input(&w.cp, "input-observe-enabled-0").await, projection);
    assert_eq!(input_stamp(&w.cp, "input-observe-enabled-0").await, stamp);
    assert_measured_phase(&w.cp, "input-observe-baseline", 0).await;
    assert_measured_phase(&w.cp, "input-observe-enabled", 500).await;
}
