//! 本利汇算走真实 CP、遥测摄入和 PostgreSQL；金额期望由场景的价格独立算出。
mod support;

use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::profit_projection::project_once;
use aisix_control_plane::db::Db;
use aisix_control_plane::pricing::cost_rates::{self, NewCostRate};
use aisix_pricing::{CnyPerUsd, ENGINE_VERSION};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use futures::{future::join_all, FutureExt};
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::world::{
    card, db_now, deliver, insert_baseline, insert_baseline_backdated, price, seed_key, seed_model,
    seed_provider_key, seed_tenant, usage, world, World,
};
use support::{BareDatabase, OperatorSession, Resp, TestCp};
use uuid::Uuid;

const PATH: &str = "/admin/billing/profit-summary";

fn at(raw: &str) -> DateTime<Utc> {
    raw.parse().expect("测试时间必须合法")
}

fn path(window: Option<(DateTime<Utc>, DateTime<Utc>)>) -> String {
    match window {
        None => PATH.to_string(),
        Some((from, to)) => format!(
            "{PATH}?from={}&to={}",
            from.to_rfc3339_opts(SecondsFormat::Millis, true),
            to.to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
    }
}

// 只重试已约定的追赶状态；其它失败立即暴露，不能把真正错误吞成等待。
async fn report(
    cp: &TestCp,
    who: &OperatorSession,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Resp {
    let path = path(window);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let r = cp.send("GET", &path, Some(who.cookie()), None).await;
        if r.status != 503 || r.body["code"] != "profit_summary_catching_up" {
            assert_eq!(r.status, 200, "{path}: {r:?}");
            return r;
        }
        assert!(Instant::now() < deadline, "汇算没有在五秒内追齐: {r:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn totals(r: &Resp, expected: Value) {
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body.get("totals"), Some(&expected), "{r:?}");
}

fn empty() -> Value {
    json!({
        "charged_micro_cny":"0", "adjusted_micro_cny":"0", "known_cost_micro_cny":"0",
        "charged_with_cost_micro_cny":"0", "rows_with_cost":"0", "rows_without_cost":"0",
        "unpriced_rows":"0", "rows":"0"
    })
}

async fn drain(db: &Db) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        project_once(db).await.expect("短批投影成功");
        let ready: bool = sqlx::query_scalar(
            "SELECT complete AND NOT EXISTS (SELECT 1 FROM billing_profit_pending) \
             FROM billing_profit_backfill WHERE version = 1",
        )
        .fetch_one(db.pool())
        .await
        .expect("读取持久追赶进度");
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "短批投影没有在五秒内追齐");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn cost_rate(cp: &TestCp, account: Uuid, rate: &str) {
    let row = NewCostRate {
        provider_key_id: account,
        model_id: None,
        effective_from: db_now(cp).await,
        cny_per_usd: Some(CnyPerUsd::parse(rate).expect("测试成本倍率合法")),
        note: "本利汇算测试成本".to_string(),
    };
    cp.db()
        .tx(move |conn| {
            async move {
                cost_rates::insert_cost_rate(conn, &row, Actor::System)
                    .await
                    .map(|_| ())
            }
            .boxed()
        })
        .await
        .expect("配置真实成本倍率");
}

fn served(w: &World, id: &str, model: Uuid, account: Uuid, when: DateTime<Utc>) -> Value {
    usage(
        w,
        id,
        model,
        json!({"occurred_at":when, "prompt_tokens":1000, "provider_key_id":account}),
    )
}

// 仅测试夹具：真实生产价格写入禁止回溯，历史遥测场景用 SQL 准备当年的成本配置。
async fn historical_cost(cp: &TestCp, account: Uuid, when: DateTime<Utc>) {
    let mut tx = cp.db().pool().begin().await.expect("准备历史成本");
    sqlx::query(
        "INSERT INTO pricing_cost_rates \
         (engine_version, provider_key_id, effective_from, cny_per_usd_e4, note) \
         VALUES ($1, $2, $3, 3000, '本利汇算历史测试夹具')",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(account)
    .bind(when)
    .execute(&mut *tx)
    .await
    .expect("写入测试历史成本");
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1) \
         WHERE key = 'pricing_version'",
    )
    .execute(&mut *tx)
    .await
    .expect("发布测试价格版本");
    tx.commit().await.expect("提交历史成本夹具");
}

// 去掉历史、端点边界或两个边缘之间的完整日，都会改变这里手算的金额和行数。
#[tokio::test]
async fn all_time_and_long_explicit_windows_include_old_real_telemetry_at_half_open_boundaries() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let start = at("2020-01-01T00:00:00Z");
    insert_baseline_backdated(
        w.cp.db(),
        model,
        serde_json::to_value(card()).expect("测试基准价格"),
        start,
    )
    .await;
    historical_cost(&w.cp, account, start).await;
    let from = at("2021-02-01T12:00:00Z");
    let to = at("2022-11-03T12:00:00Z");
    let events = vec![
        served(&w, "before", model, account, from - TimeDelta::seconds(1)),
        served(&w, "at-from", model, account, from),
        usage(
            &w,
            "middle",
            model,
            json!({"occurred_at":"2021-09-01T00:00:00Z", "prompt_tokens":1000}),
        ),
        served(
            &w,
            "before-to",
            model,
            account,
            to - TimeDelta::milliseconds(1),
        ),
        served(&w, "at-to", model, account, to),
    ];
    let sent = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body["accepted"], 5, "{sent:?}");

    let all = report(&w.cp, &w.admin, None).await;
    assert_eq!(all.body.get("from"), Some(&Value::Null));
    assert_eq!(all.body.get("to"), Some(&Value::Null));
    totals(
        &all,
        json!({
            "charged_micro_cny":"5000", "adjusted_micro_cny":"0", "known_cost_micro_cny":"1200",
            "charged_with_cost_micro_cny":"4000", "rows_with_cost":"4", "rows_without_cost":"1",
            "unpriced_rows":"0", "rows":"5"
        }),
    );
    let selected = report(&w.cp, &w.admin, Some((from, to))).await;
    totals(
        &selected,
        json!({
            "charged_micro_cny":"3000", "adjusted_micro_cny":"0", "known_cost_micro_cny":"600",
            "charged_with_cost_micro_cny":"2000", "rows_with_cost":"2", "rows_without_cost":"1",
            "unpriced_rows":"0", "rows":"3"
        }),
    );
    assert_eq!(
        at(selected.body["from"].as_str().expect("返回开始时间")),
        from
    );
    assert_eq!(at(selected.body["to"].as_str().expect("返回结束时间")), to);
    let one = report(&w.cp, &w.admin, Some((from, from + TimeDelta::seconds(1)))).await;
    totals(
        &one,
        json!({
            "charged_micro_cny":"1000", "adjusted_micro_cny":"0", "known_cost_micro_cny":"300",
            "charged_with_cost_micro_cny":"1000", "rows_with_cost":"1", "rows_without_cost":"0",
            "unpriced_rows":"0", "rows":"1"
        }),
    );
}

// 把充值混入收入、把调整再加一次、回填历史成本或把未定价行算作已知成本均应失败。
#[tokio::test]
async fn repricing_changes_revenue_once_but_keeps_original_known_and_unknown_costs() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let unpriced_model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let unknown = seed_provider_key(&w.cp, &w.admin).await;
    cost_rate(&w.cp, account, "0.3").await;
    let now = db_now(&w.cp).await;
    let mut cached = served(&w, "cache", model, account, now);
    cached["cache_status"] = json!("hit");
    let sent = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            served(&w, "known", model, account, now),
            served(&w, "unknown", model, unknown, now),
            usage(
                &w,
                "unpriced",
                unpriced_model,
                json!({"prompt_tokens":1000}),
            ),
            cached,
            usage(&w, "empty", model, json!({})),
        ],
    )
    .await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body["accepted"], 5, "{sent:?}");
    let before = report(&w.cp, &w.admin, None).await;
    totals(
        &before,
        json!({
            "charged_micro_cny":"2000", "adjusted_micro_cny":"0", "known_cost_micro_cny":"300",
            "charged_with_cost_micro_cny":"1000", "rows_with_cost":"1", "rows_without_cost":"1",
            "unpriced_rows":"1", "rows":"5"
        }),
    );

    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let credited =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", w.tenant),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"2", "reason":"本利汇算测试充值"})),
        )
        .await;
    assert_eq!(credited.status, 201, "{credited:?}");
    assert_eq!(
        report(&w.cp, &w.admin, None).await.body["totals"],
        before.body["totals"]
    );

    cost_rate(&w.cp, account, "0.9").await;
    cost_rate(&w.cp, unknown, "0.5").await;
    let doubled = serde_json::from_value(json!({"default":{"base":{
        "input":"2", "output":"4", "cache_read":"0.2"
    }}}))
    .expect("新售价基准");
    insert_baseline(&w.cp, model, db_now(&w.cp).await, doubled)
        .await
        .expect("新售价");
    price(&w.cp, unpriced_model).await;
    let preview =
        w.cp.send(
            "POST",
            "/admin/billing/repricing",
            Some(w.admin.cookie()),
            Some(
                json!({"from":now-TimeDelta::hours(1), "to":db_now(&w.cp).await,
            "reason":"本利汇算真实重新定价"}),
            ),
        )
        .await;
    assert_eq!(preview.status, 201, "{preview:?}");
    assert_eq!(preview.body["delta_micro_cny"], 3000, "{preview:?}");
    let id = preview.body["id"].as_str().expect("重新定价编号");
    let applied =
        w.cp.send(
            "POST",
            &format!("/admin/billing/repricing/{id}/apply"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(applied.status, 200, "{applied:?}");
    let after = report(&w.cp, &w.admin, None).await;
    totals(
        &after,
        json!({
            "charged_micro_cny":"5000", "adjusted_micro_cny":"3000", "known_cost_micro_cny":"300",
            "charged_with_cost_micro_cny":"2000", "rows_with_cost":"1", "rows_without_cost":"2",
            "unpriced_rows":"0", "rows":"5"
        }),
    );
    let again =
        w.cp.send(
            "POST",
            &format!("/admin/billing/repricing/{id}/apply"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(again.status, 409, "{again:?}");
    assert_eq!(
        report(&w.cp, &w.admin, None).await.body["totals"],
        after.body["totals"]
    );
    let costs: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT cost_micro_cny FROM pricing_charges_cny \
         WHERE event_id IN ('known', 'unknown', 'unpriced') ORDER BY event_id",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("历史成本没有被报表或重新定价改写");
    assert_eq!(costs, vec![Some(300), None, None]);
}

// 仅测试夹具：直接准备合法收费快照，避免为边界金额制造付费上游调用。
async fn fixture_charge(
    conn: &mut PgConnection,
    id: &str,
    when: DateTime<Utc>,
    amount: i64,
    cost: Option<i64>,
) -> i64 {
    sqlx::query(
        "INSERT INTO usage_events(event_id, dp_id, batch_id, occurred_at, event) \
         VALUES ($1, $2, $2, $3, '{\"test_fixture\":true}')",
    )
    .bind(id)
    .bind(Uuid::new_v4())
    .bind(when)
    .execute(&mut *conn)
    .await
    .expect("准备测试用量");
    sqlx::query_scalar(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, status, \
          charge_micro_cny, cost_micro_cny, engine_version) \
         VALUES ($1, 'test-tenant', 'test-key', '', 'test-model', $2, 'charged', $3, $4, $5) \
         RETURNING seq",
    )
    .bind(id)
    .bind(when)
    .bind(amount)
    .bind(cost)
    .bind(i64::from(ENGINE_VERSION))
    .fetch_one(conn)
    .await
    .expect("准备测试收费快照")
}

// 两笔合法 bigint 收费的和大于 i64::MAX；任何浮点或 bigint 聚合都会破坏精确结果。
#[tokio::test]
async fn money_stays_exact_above_javascript_and_bigint_aggregate_limits() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let mut tx = cp.db().pool().begin().await.expect("准备极值测试");
    fixture_charge(
        &mut tx,
        "large-one",
        at("2021-01-01T00:00:00Z"),
        i64::MAX,
        Some(42),
    )
    .await;
    fixture_charge(
        &mut tx,
        "large-two",
        at("2022-01-01T00:00:00Z"),
        i64::MAX,
        Some(0),
    )
    .await;
    tx.commit().await.expect("提交极值测试夹具");
    let result = report(&cp, &viewer, None).await;
    totals(
        &result,
        json!({
            "charged_micro_cny":"18446744073709551614", "adjusted_micro_cny":"0", "known_cost_micro_cny":"42",
            "charged_with_cost_micro_cny":"18446744073709551614", "rows_with_cost":"2", "rows_without_cost":"0",
            "unpriced_rows":"0", "rows":"2"
        }),
    );
    drain(cp.db()).await;
    assert_eq!(
        report(&cp, &viewer, None).await.body["totals"],
        result.body["totals"]
    );
}

// 待投影数据与已刷日汇总必须相同；边缘明细不能再叠加一次 pending 差量。
#[tokio::test]
async fn pending_overlay_and_flushed_rollups_agree_for_partial_and_whole_days() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    insert_baseline_backdated(
        w.cp.db(),
        model,
        serde_json::to_value(card()).expect("基准"),
        at("2020-01-01T00:00:00Z"),
    )
    .await;
    report(&w.cp, &w.admin, None).await;
    let mut fence =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("冻结独立测试库的投影工人");
    sqlx::query("SELECT version FROM billing_profit_backfill WHERE version = 1 FOR UPDATE")
        .execute(&mut *fence)
        .await
        .expect("仅测试：占用投影互斥行");
    let events = vec![
        usage(
            &w,
            "edge-start",
            model,
            json!({"occurred_at":"2021-01-01T12:00:00Z", "prompt_tokens":1000}),
        ),
        usage(
            &w,
            "whole-day",
            model,
            json!({"occurred_at":"2021-01-02T12:00:00Z", "prompt_tokens":1000}),
        ),
        usage(
            &w,
            "edge-end",
            model,
            json!({"occurred_at":"2021-01-03T11:59:59Z", "prompt_tokens":1000}),
        ),
    ];
    let sent = deliver(&w.dp, Uuid::new_v4(), events.clone()).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    let duplicate = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(duplicate.status, 200, "{duplicate:?}");
    assert_eq!(
        duplicate.body,
        json!({"accepted":0,"rejected":0,"duplicates":3})
    );
    let windows = [
        None,
        Some((at("2021-01-01T12:00:00Z"), at("2021-01-03T12:00:00Z"))),
        Some((at("2021-01-02T00:00:00Z"), at("2021-01-03T00:00:00Z"))),
        Some((at("2021-01-01T12:00:00Z"), at("2021-01-01T13:00:00Z"))),
    ];
    let mut before = Vec::new();
    for (window, (charged, rows)) in
        windows
            .into_iter()
            .zip([("3000", "3"), ("3000", "3"), ("1000", "1"), ("1000", "1")])
    {
        let r = report(&w.cp, &w.admin, window).await;
        totals(
            &r,
            json!({
                "charged_micro_cny":charged, "adjusted_micro_cny":"0", "known_cost_micro_cny":"0",
                "charged_with_cost_micro_cny":"0", "rows_with_cost":"0", "rows_without_cost":rows,
                "unpriced_rows":"0", "rows":rows
            }),
        );
        before.push(r.body);
    }
    fence.rollback().await.expect("释放测试投影互斥行");
    drain(w.cp.db()).await;
    for (window, expected) in windows.into_iter().zip(before) {
        assert_eq!(report(&w.cp, &w.admin, window).await.body, expected);
    }
}

// 空集合必须真实返回所有零值字符串；缺字段、租户泄露或错误范围都应被发现。
#[tokio::test]
async fn an_empty_report_is_exact_and_requires_an_operator_with_a_valid_optional_window() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    totals(&report(&w.cp, &viewer, None).await, empty());
    totals(
        &report(
            &w.cp,
            &viewer,
            Some((at("2020-01-01T00:00:00Z"), at("2025-01-01T00:00:00Z"))),
        )
        .await,
        empty(),
    );
    for query in [
        "from=2021-01-01T00:00:00Z",
        "to=2022-01-01T00:00:00Z",
        "from=nope&to=nope",
        "from=2022-01-01T00:00:00Z&to=2021-01-01T00:00:00Z",
        "from=2022-01-01T00:00:00Z&to=2022-01-01T00:00:00Z",
    ] {
        let r =
            w.cp.send(
                "GET",
                &format!("{PATH}?{query}"),
                Some(viewer.cookie()),
                None,
            )
            .await;
        assert_eq!(r.status, 422, "{query}: {r:?}");
    }
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for cookie in [None, Some(owner.cookie())] {
        let r = w.cp.send("GET", PATH, cookie, None).await;
        assert_eq!(r.status, 401, "成本报表不得暴露给租户: {r:?}");
        assert_eq!(r.body["code"], "unauthenticated", "{r:?}");
    }
}

// 仅测试夹具：批量准备高基数历史，所有收费为 3、已知成本为 1，未发任何上游请求。
async fn fixture_many(db: &Db, prefix: &str, count: i64) {
    let mut tx = db.pool().begin().await.expect("批量测试夹具");
    sqlx::query(
        "INSERT INTO usage_events(event_id, dp_id, batch_id, occurred_at, event) \
         SELECT $1 || '-' || n, $2, $2, '2021-01-01T00:00:00Z', '{\"test_fixture\":true}' \
         FROM generate_series(1, $3::bigint) n ORDER BY n",
    )
    .bind(prefix)
    .bind(Uuid::new_v4())
    .bind(count)
    .execute(&mut *tx)
    .await
    .expect("准备批量测试用量");
    sqlx::query(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, status, \
          charge_micro_cny, cost_micro_cny, engine_version) \
         SELECT event_id, 'test-tenant', 'test-key', '', 'test-model', occurred_at, 'charged', 3, 1, $2 \
         FROM usage_events WHERE event_id LIKE $1 || '-%' ORDER BY seq",
    ).bind(prefix).bind(i64::from(ENGINE_VERSION)).execute(&mut *tx).await.expect("准备批量测试收费");
    tx.commit().await.expect("提交批量测试夹具");
}

async fn daily(db: &Db) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object( \
         'charged_micro_cny', COALESCE(sum(charged_micro_cny)::numeric, 0)::text, \
         'adjusted_micro_cny', COALESCE(sum(adjusted_micro_cny)::numeric, 0)::text, \
         'known_cost_micro_cny', COALESCE(sum(known_cost_micro_cny)::numeric, 0)::text, \
         'charged_with_cost_micro_cny', COALESCE(sum(charged_with_cost_micro_cny)::numeric, 0)::text, \
         'rows_with_cost', COALESCE(sum(rows_with_cost)::numeric, 0)::text, \
         'rows_without_cost', COALESCE(sum(rows_without_cost)::numeric, 0)::text, \
         'unpriced_rows', COALESCE(sum(unpriced_rows)::numeric, 0)::text, \
         'rows', COALESCE(sum(rows)::numeric, 0)::text) FROM billing_profit_daily",
    ).fetch_one(db.pool()).await.expect("读取持久日汇总")
}

// 一次扫完历史、失去持久游标、重复队列重复记账或两个工人重入都会破坏手算总数。
#[tokio::test]
async fn historical_backfill_resumes_in_bounded_batches_and_duplicate_workers_do_not_add_money() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    // 仅测试：模拟安装触发器以前已经存在的历史，而不是删除生产账务。
    sqlx::query("ALTER TABLE pricing_charges_cny DISABLE TRIGGER USER")
        .execute(db.pool())
        .await
        .expect("准备触发器之前的历史夹具");
    fixture_many(db, "history", 600).await;
    sqlx::query("ALTER TABLE pricing_charges_cny ENABLE TRIGGER USER")
        .execute(db.pool())
        .await
        .expect("恢复测试库全部用户触发器");
    let first = project_once(db).await.expect("第一短批");
    assert!(
        first > 0 && first <= 256,
        "一次最多处理 256 个事件: {first}"
    );
    let state: (i64, Option<i64>, bool) = sqlx::query_as(
        "SELECT after_seq, high_seq, complete FROM billing_profit_backfill WHERE version = 1",
    )
    .fetch_one(db.pool())
    .await
    .expect("读取第一批持久进度");
    assert!(
        state.0 > 0 && state.1.is_some() && !state.2,
        "不能把 600 行历史宣布完成: {state:?}"
    );
    drain(db).await;
    let expected = json!({
        "charged_micro_cny":"1800", "adjusted_micro_cny":"0", "known_cost_micro_cny":"600",
        "charged_with_cost_micro_cny":"1800", "rows_with_cost":"600", "rows_without_cost":"0",
        "unpriced_rows":"0", "rows":"600"
    });
    assert_eq!(daily(db).await, expected);
    sqlx::query("INSERT INTO billing_profit_pending(event_id) SELECT event_id FROM pricing_charges_cny ON CONFLICT DO NOTHING")
        .execute(db.pool()).await.expect("仅测试：重复排队已经处理的全部事件");
    let (one, two, three, four) = tokio::join!(
        project_once(db),
        project_once(db),
        project_once(db),
        project_once(db)
    );
    for result in [one, two, three, four] {
        result.expect("并发工人不互相阻塞或重复计费");
    }
    drain(db).await;
    assert_eq!(daily(db).await, expected);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM billing_profit_rows")
        .fetch_one(db.pool())
        .await
        .expect("每个事件只有一份快照");
    assert_eq!(rows, 600);
}

// identity 的分配顺序不是提交顺序；仅靠 high_seq 越过这个洞会永久漏掉五元微量收费。
#[tokio::test]
async fn a_lower_sequence_committed_after_backfill_completion_is_still_counted() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    let mut low = PgConnection::connect(bare.url())
        .await
        .expect("真实低序号控制会话");
    sqlx::query("BEGIN")
        .execute(&mut low)
        .await
        .expect("延迟提交低序号");
    let low_seq =
        fixture_charge(&mut low, "late-low", at("2021-01-01T00:00:00Z"), 5, Some(1)).await;
    let mut high = db.pool().acquire().await.expect("高序号会话");
    let high_seq = fixture_charge(
        &mut high,
        "early-high",
        at("2021-01-01T00:00:00Z"),
        7,
        Some(2),
    )
    .await;
    drop(high);
    assert!(low_seq < high_seq);
    drain(db).await;
    let state: (i64, bool) =
        sqlx::query_as("SELECT after_seq, complete FROM billing_profit_backfill WHERE version = 1")
            .fetch_one(db.pool())
            .await
            .expect("高水位已越过未提交洞");
    assert!(state.0 >= high_seq && state.1, "{state:?}");
    assert_eq!(daily(db).await["charged_micro_cny"], "7");
    sqlx::query("COMMIT")
        .execute(&mut low)
        .await
        .expect("历史洞现在提交");
    drain(db).await;
    assert_eq!(
        daily(db).await,
        json!({
            "charged_micro_cny":"12", "adjusted_micro_cny":"0", "known_cost_micro_cny":"3",
            "charged_with_cost_micro_cny":"12", "rows_with_cost":"2", "rows_without_cost":"0",
            "unpriced_rows":"0", "rows":"2"
        })
    );
}

// 日汇总写入受阻时必须整批回滚；不可先删队列或只写逐事件快照而丢掉钱。
#[tokio::test]
async fn a_blocked_projection_rolls_back_then_recovers_without_losing_pending_money() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    let mut conn = db.pool().acquire().await.expect("准备收费夹具");
    fixture_charge(&mut conn, "retry", at("2021-01-01T00:00:00Z"), 19, Some(4)).await;
    drop(conn);
    let mut blocker = PgConnection::connect(bare.url())
        .await
        .expect("真实阻塞会话");
    sqlx::query("BEGIN")
        .execute(&mut blocker)
        .await
        .expect("锁定日汇总");
    sqlx::query("LOCK billing_profit_daily IN SHARE MODE")
        .execute(&mut blocker)
        .await
        .expect("阻塞日汇总写入");
    let attempt = tokio::time::timeout(Duration::from_secs(1), project_once(db)).await;
    assert!(attempt.is_ok(), "工人持锁时间不能超过短事务预算");
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM billing_profit_pending), \
                (SELECT count(*) FROM billing_profit_rows), (SELECT count(*) FROM billing_profit_daily)",
    ).fetch_one(db.pool()).await.expect("失败批次没有部分提交");
    assert_eq!(counts, (1, 0, 0));
    sqlx::query("ROLLBACK")
        .execute(&mut blocker)
        .await
        .expect("释放日汇总阻塞");
    drain(db).await;
    assert_eq!(
        daily(db).await,
        json!({
            "charged_micro_cny":"19", "adjusted_micro_cny":"0", "known_cost_micro_cny":"4",
            "charged_with_cost_micro_cny":"19", "rows_with_cost":"1", "rows_without_cost":"0",
            "unpriced_rows":"0", "rows":"1"
        })
    );
}

// 后台工人不得等待或新开连接争抢耗尽的共享池；释放后同一数据仍可正常追齐。
#[tokio::test]
async fn projection_skips_a_busy_shared_pool_instead_of_waiting_for_a_connection() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    fixture_many(db, "pool-busy", 1).await;
    let mut held = Vec::new();
    for _ in 0..db.pool().options().get_max_connections() {
        held.push(db.pool().acquire().await.expect("仅测试：占满共享池"));
    }
    let result = tokio::time::timeout(Duration::from_millis(200), project_once(db)).await;
    assert_eq!(
        result.expect("后台工人应立即让路").expect("忙池不是错误"),
        0
    );
    drop(held);
    drain(db).await;
    assert_eq!(daily(db).await["charged_micro_cny"], "3");
}

// 超出有界 overlay 时明确拒绝；不能返回貌似完整但缺历史或缺排队事件的总数。
#[tokio::test]
async fn too_many_pending_events_return_catching_up_then_the_complete_total() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    report(&cp, &viewer, None).await;
    let mut fence = cp.db().pool().begin().await.expect("冻结测试投影");
    sqlx::query("SELECT version FROM billing_profit_backfill WHERE version = 1 FOR UPDATE")
        .execute(&mut *fence)
        .await
        .expect("占用投影互斥行");
    fixture_many(cp.db(), "backlog", 4097).await;
    let waiting = cp.send("GET", PATH, Some(viewer.cookie()), None).await;
    assert_eq!(waiting.status, 503, "{waiting:?}");
    assert_eq!(
        waiting.body["code"], "profit_summary_catching_up",
        "{waiting:?}"
    );
    fence.rollback().await.expect("释放测试投影");
    totals(
        &report(&cp, &viewer, None).await,
        json!({
            "charged_micro_cny":"12291", "adjusted_micro_cny":"0", "known_cost_micro_cny":"4097",
            "charged_with_cost_micro_cny":"12291", "rows_with_cost":"4097", "rows_without_cost":"0",
            "unpriced_rows":"0", "rows":"4097"
        }),
    );
}

// 追赶历史没有完成时，空队列也不能被误认为历史已经齐全。
#[tokio::test]
async fn unfinished_historical_backfill_never_returns_a_successful_partial_report() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    report(&cp, &viewer, None).await;
    let mut fence = cp.db().pool().begin().await.expect("冻结测试历史扫描");
    sqlx::query("LOCK pricing_charges_cny IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *fence)
        .await
        .expect("仅测试：阻止历史工人读取来源");
    sqlx::query(
        "UPDATE billing_profit_backfill SET after_seq = 0, high_seq = NULL, complete = false WHERE version = 1",
    )
    .execute(cp.db().pool())
    .await
    .expect("仅测试：持久化尚未完成的历史状态");
    let waiting = tokio::time::timeout(
        Duration::from_secs(1),
        cp.send("GET", PATH, Some(viewer.cookie()), None),
    )
    .await
    .expect("未完成的历史应立即返回追赶状态");
    assert_eq!(waiting.status, 503, "{waiting:?}");
    assert_eq!(
        waiting.body["code"], "profit_summary_catching_up",
        "{waiting:?}"
    );
    fence.rollback().await.expect("释放历史扫描");
    totals(&report(&cp, &viewer, None).await, empty());
}

// 仅测试夹具：已应用重新定价的元数据，不改变账本或调用真实上游。
async fn fixture_run(db: &Db, delta: i64) -> (Uuid, Uuid) {
    let operator = Uuid::new_v4();
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO operators(id, email, password_hash, role) VALUES ($1, $2, 'test-fixture-only', 'admin')",
    ).bind(operator).bind(format!("profit-{operator}@example.invalid"))
        .execute(db.pool()).await.expect("准备测试操作员外键");
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id, status, filter, reason, pricing_version, engine_version, events, changed, \
          still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by, \
          applied_by, applied_at) \
         VALUES ($1, 'applied', '{}', '本利汇算锁测试夹具', 0, $2, 1, 1, 0, 0, $3, '[]', $4, $4, now())",
    ).bind(run).bind(i64::from(ENGINE_VERSION)).bind(delta).bind(operator)
        .execute(db.pool()).await.expect("准备测试重新定价元数据");
    (run, operator)
}

// 若重复入队改成 DO NOTHING，生产者会提前完成，随后队列删除永久丢失已提交调整。
#[tokio::test]
async fn an_adjustment_waits_for_the_owned_pending_row_and_requeues_after_its_deletion() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    let mut original = db.pool().acquire().await.expect("准备收费");
    fixture_charge(
        &mut original,
        "adjust-race",
        at("2021-01-01T00:00:00Z"),
        5,
        Some(1),
    )
    .await;
    drop(original);
    drain(db).await;
    let (run, operator) = fixture_run(db, 2).await;
    sqlx::query("INSERT INTO billing_profit_pending(event_id) VALUES ('adjust-race')")
        .execute(db.pool())
        .await
        .expect("仅测试：再次排队已持久化事件");

    // 仅测试控制会话：停在工人已取得队列所有权、已读完本次源金额的时刻。
    let mut owner = PgConnection::connect(bare.url())
        .await
        .expect("队列所有者控制会话");
    sqlx::query("BEGIN")
        .execute(&mut owner)
        .await
        .expect("持有队列行");
    sqlx::query(
        "SELECT event_id FROM billing_profit_pending WHERE event_id = 'adjust-race' FOR UPDATE",
    )
    .execute(&mut owner)
    .await
    .expect("工人拥有事件的队列锁");
    let read_before: i64 = sqlx::query_scalar(
        "SELECT charge_micro_cny + COALESCE((SELECT sum(delta_micro_cny)::bigint \
         FROM pricing_adjustments_cny WHERE event_id = 'adjust-race'), 0) \
         FROM pricing_charges_cny WHERE event_id = 'adjust-race'",
    )
    .fetch_one(&mut owner)
    .await
    .expect("工人已看到调整前的源金额");
    assert_eq!(read_before, 5);

    let mut producer = PgConnection::connect(bare.url())
        .await
        .expect("调整生产者真实会话");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut producer)
        .await
        .expect("调整会话编号");
    let adjustment = tokio::spawn(async move {
        sqlx::query(
            "INSERT INTO pricing_adjustments_cny \
             (run_id, event_id, delta_micro_cny, engine_version, reason, created_by) \
             VALUES ($1, 'adjust-race', 2, $2, '并发调整测试夹具', $3)",
        )
        .bind(run)
        .bind(i64::from(ENGINE_VERSION))
        .bind(operator)
        .execute(&mut producer)
        .await
        .expect("调整和新的待处理通知原子提交");
    });
    let blocked = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if adjustment.is_finished() {
                return false;
            }
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')",
            ).bind(pid).fetch_one(db.pool()).await.expect("读取真实调整锁等待");
            if waiting {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("调整必须进入明确可观察的队列锁等待");
    assert!(blocked, "调整不能把已经被工人认领的队列行当成通知已经送达");
    sqlx::query("DELETE FROM billing_profit_pending WHERE event_id = 'adjust-race'")
        .execute(&mut owner)
        .await
        .expect("仅测试：工人结束原金额没有变化的批次");
    sqlx::query("COMMIT")
        .execute(&mut owner)
        .await
        .expect("释放队列所有权");
    tokio::time::timeout(Duration::from_secs(1), adjustment)
        .await
        .expect("调整在工人提交后继续")
        .expect("调整任务成功");
    let queued: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM billing_profit_pending WHERE event_id = 'adjust-race')",
    )
    .fetch_one(db.pool())
    .await
    .expect("调整提交后通知仍在");
    assert!(queued);
    drain(db).await;
    assert_eq!(
        daily(db).await,
        json!({
            "charged_micro_cny":"7", "adjusted_micro_cny":"2", "known_cost_micro_cny":"1",
            "charged_with_cost_micro_cny":"7", "rows_with_cost":"1", "rows_without_cost":"0",
            "unpriced_rows":"0", "rows":"1"
        })
    );
}

async fn measured_intake(
    w: &World,
    model: Uuid,
    prefix: &str,
    count: usize,
    repeat: usize,
) -> Vec<(usize, usize, Resp, Duration)> {
    let events = (0..count)
        .map(|n| {
            usage(
                w,
                &format!("{prefix}-{count}-{repeat}-{n}"),
                model,
                json!({"prompt_tokens":1000, "cached_prompt_tokens":800, "completion_tokens":200}),
            )
        })
        .collect();
    let start = Instant::now();
    let response = deliver(&w.dp, Uuid::new_v4(), events).await;
    vec![(count, repeat, response, start.elapsed())]
}

// 只切换独立测试库的队列触发器，切换时间不混入真实 HTTP 样本。
async fn profit_charge_trigger(db: &Db, enabled: bool) {
    let sql = if enabled {
        "ALTER TABLE pricing_charges_cny ENABLE TRIGGER billing_profit_charge_insert"
    } else {
        "ALTER TABLE pricing_charges_cny DISABLE TRIGGER billing_profit_charge_insert"
    };
    sqlx::query(sql)
        .execute(db.pool())
        .await
        .expect("切换测试库队列触发器");
}

fn print_intake(phase: &str, measured: Vec<(usize, usize, Resp, Duration)>) {
    for (count, repeat, response, elapsed) in measured {
        assert_eq!(
            response.status, 200,
            "{phase}/{count}/{repeat}: {response:?}"
        );
        assert_eq!(
            response.body,
            json!({"accepted":count,"rejected":0,"duplicates":0}),
            "{response:?}"
        );
        eprintln!(
            "{}",
            json!({
                "observation":"billing_profit_projection_intake", "phase":phase,
                "batch_rows":count, "repeat":repeat, "elapsed_us":elapsed.as_micros().to_string(),
                "rows_per_second":count as f64 / elapsed.as_secs_f64(),
            })
        );
    }
}

// 标准后端集成夹具通过真实遥测摄入；只报告观测，不用抖动的倍率阈值宣布性能通过。
// 基线阶段的收费没有排队，故这个专用数据库不能作为全历史汇总的正确性 oracle。
#[tokio::test]
async fn profit_projection_ingest_throughput_observation() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    report(&w.cp, &w.admin, None).await;
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let credited =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", w.tenant),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"100", "reason":"真实遥测摄入性能观测余额"})),
        )
        .await;
    assert_eq!(credited.status, 201, "{credited:?}");

    let warm = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(
            &w,
            "profit-observe-warm",
            model,
            json!({"prompt_tokens":1000}),
        )],
    )
    .await;
    assert_eq!(warm.status, 200, "{warm:?}");
    drain(w.cp.db()).await;
    // 日热表被其它会话占用时，真实摄入只能追加独立事件通知，不能跟着等待日汇总。
    let mut blocked_daily = w.cp.db().pool().begin().await.expect("冻结日汇总写入");
    sqlx::query("LOCK billing_profit_daily IN SHARE MODE")
        .execute(&mut *blocked_daily)
        .await
        .expect("仅测试：持有日汇总表共享锁");
    let while_blocked = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(
            &w.dp,
            Uuid::new_v4(),
            vec![usage(
                &w,
                "profit-observe-isolation",
                model,
                json!({"prompt_tokens":1000}),
            )],
        ),
    )
    .await
    .expect("真实遥测摄入不能等待日汇总热表");
    assert_eq!(while_blocked.status, 200, "{while_blocked:?}");
    assert_eq!(
        while_blocked.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    blocked_daily.rollback().await.expect("释放测试日汇总锁");

    for count in [1_usize, 10, 100, 1000] {
        // 每个批量和开关状态均先预热连接、价格缓存及相关索引。
        for (phase, enabled) in [("disabled", false), ("enabled", true)] {
            drain(w.cp.db()).await;
            profit_charge_trigger(w.cp.db(), enabled).await;
            let warmup =
                measured_intake(&w, model, &format!("profit-warm-{phase}"), count, 0).await;
            profit_charge_trigger(w.cp.db(), true).await;
            for (_, _, response, _) in warmup {
                assert_eq!(response.status, 200, "预热失败: {response:?}");
                assert_eq!(
                    response.body,
                    json!({"accepted":count,"rejected":0,"duplicates":0})
                );
            }
        }
        for repeat in 0..5 {
            // 每轮交错两个状态并反转先后，避免把统一的冷启动或负载漂移当成开关开销。
            let order = if repeat % 2 == 0 {
                [("disabled", false), ("enabled", true)]
            } else {
                [("enabled", true), ("disabled", false)]
            };
            for (phase, enabled) in order {
                drain(w.cp.db()).await;
                profit_charge_trigger(w.cp.db(), enabled).await;
                let measured =
                    measured_intake(&w, model, &format!("profit-observe-{phase}"), count, repeat)
                        .await;
                profit_charge_trigger(w.cp.db(), true).await;
                print_intake(phase, measured);
            }
        }
    }

    // 每阶段 5 × (1+10+100+1000) = 5555 行，每行 680 微元；预热不混入账务断言。
    for prefix in ["profit-observe-disabled", "profit-observe-enabled"] {
        let billed: (i64, i64) = sqlx::query_as(
            "SELECT count(*), COALESCE(sum(charge_micro_cny)::bigint, 0) \
             FROM pricing_charges_cny WHERE event_id LIKE $1 || '-%'",
        )
        .bind(prefix)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("比较观测阶段的原始收费");
        assert_eq!(billed, (5555, 3_777_400));
    }
}

#[derive(Clone)]
struct IntakeLane {
    tenant: Uuid,
    member: Option<Uuid>,
    key: String,
}

#[derive(Default)]
struct IntakeLockSamples {
    count: usize,
    lock_waiter_sum: i64,
    max_lock_waiters: i64,
    max_profit_lock_waiters: i64,
    worker_active_samples: usize,
    min_pool_idle: usize,
    max_pool_size: u32,
}

// 观察会话不占 CP 共享池；采样当前测试库的真实 PG 锁和进程状态。
async fn concurrent_intake(
    w: &World,
    lanes: &[IntakeLane],
    model: Uuid,
    prefix: &str,
    count: usize,
    observer: &mut PgConnection,
) -> (Vec<(Resp, Duration)>, Duration, IntakeLockSamples) {
    let batches: Vec<Vec<Value>> = lanes
        .iter()
        .enumerate()
        .map(|(lane, identity)| {
            (0..count).map(|n| usage(w, &format!("{prefix}-{lane}-{n}"), model, json!({
            "team_id":identity.tenant, "api_key_id":identity.key,
            "user_id":identity.member.map(|id| id.to_string()).unwrap_or_default(),
            "prompt_tokens":1000, "cached_prompt_tokens":800, "completion_tokens":200,
        }))).collect()
        })
        .collect();
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let requests = async {
        let start = Instant::now();
        let replies = join_all(batches.into_iter().map(|events| async move {
            let one = Instant::now();
            let response = deliver(&w.dp, Uuid::new_v4(), events).await;
            (response, one.elapsed())
        }))
        .await;
        let elapsed = start.elapsed();
        let _ = stop.send(true);
        (replies, elapsed)
    };
    let sample = async {
        let mut samples = IntakeLockSamples {
            min_pool_idle: w.cp.db().pool().num_idle(),
            ..Default::default()
        };
        let mut every = tokio::time::interval(Duration::from_millis(5));
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = stopped.changed() => return samples,
                _ = every.tick() => {
                    samples.min_pool_idle = samples.min_pool_idle.min(w.cp.db().pool().num_idle());
                    samples.max_pool_size = samples.max_pool_size.max(w.cp.db().pool().size());
                    let (locks, profit_locks, workers): (i64, i64, i64) = sqlx::query_as(
                        "SELECT count(*) FILTER (WHERE wait_event_type = 'Lock'), \
                         count(*) FILTER (WHERE wait_event_type = 'Lock' AND query LIKE '%billing_profit%'), \
                         count(*) FILTER (WHERE state = 'active' AND query LIKE '%billing_profit%') \
                         FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid()",
                    ).fetch_one(&mut *observer).await.expect("采样真实摄入的 PG 锁等待");
                    samples.count += 1;
                    samples.lock_waiter_sum += locks;
                    samples.max_lock_waiters = samples.max_lock_waiters.max(locks);
                    samples.max_profit_lock_waiters = samples.max_profit_lock_waiters.max(profit_locks);
                    samples.worker_active_samples += usize::from(workers > 0);
                }
            }
        }
    };
    let ((replies, elapsed), samples) = tokio::join!(requests, sample);
    (replies, elapsed, samples)
}

// 八路真实 HTTP 同时覆盖账户争用和独立租户；不以不稳定耗时倍率判绿。
// 保留每个请求的成功/费用合同，并要求负载后工人使用同一个共享池完整追齐。
#[tokio::test]
async fn profit_projection_concurrent_intake_observation() {
    const CONCURRENCY: usize = 8;
    const SAMPLES: usize = 5;
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    report(&w.cp, &w.admin, None).await;
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let original = IntakeLane {
        tenant: w.tenant,
        member: Some(w.member),
        key: w.key.clone(),
    };
    let same = vec![original.clone(); CONCURRENCY];
    let mut multiple = vec![original];
    for _ in 1..CONCURRENCY {
        let tenant = seed_tenant(&w.cp, &w.admin).await;
        let key = seed_key(&w.cp, &w.admin, tenant, None).await;
        multiple.push(IntakeLane {
            tenant,
            member: None,
            key,
        });
    }
    for identity in &multiple {
        let credited =
            w.cp.send(
                "POST",
                &format!("/admin/tenants/{}/credits", identity.tenant),
                Some(admin.cookie()),
                Some(json!({"amount_cny":"100", "reason":"并发遥测摄入观测余额"})),
            )
            .await;
        assert_eq!(credited.status, 201, "{credited:?}");
    }
    let mut observer = PgConnection::connect(w.cp.database_url())
        .await
        .expect("独立 PG 观察会话");
    for (scenario, lanes) in [("same_tenant", &same), ("multiple_tenants", &multiple)] {
        for count in [1_usize, 10, 100, 1000] {
            for (phase, enabled) in [("disabled", false), ("enabled", true)] {
                drain(w.cp.db()).await;
                profit_charge_trigger(w.cp.db(), enabled).await;
                let prefix = format!("profit-matrix-warm-{scenario}-{phase}-{count}");
                let (replies, _, _) =
                    concurrent_intake(&w, lanes, model, &prefix, count, &mut observer).await;
                profit_charge_trigger(w.cp.db(), true).await;
                for (response, _) in replies {
                    assert_eq!(response.status, 200, "并发预热失败: {response:?}");
                    assert_eq!(
                        response.body,
                        json!({"accepted":count,"rejected":0,"duplicates":0})
                    );
                }
            }
            for repeat in 0..SAMPLES {
                let order = if repeat % 2 == 0 {
                    [("disabled", false), ("enabled", true)]
                } else {
                    [("enabled", true), ("disabled", false)]
                };
                for (phase, enabled) in order {
                    drain(w.cp.db()).await;
                    profit_charge_trigger(w.cp.db(), enabled).await;
                    let prefix = format!("profit-matrix-{scenario}-{phase}-{count}-{repeat}");
                    let (replies, elapsed, locks) =
                        concurrent_intake(&w, lanes, model, &prefix, count, &mut observer).await;
                    profit_charge_trigger(w.cp.db(), true).await;
                    let mut latency = Vec::new();
                    for (response, duration) in replies {
                        assert_eq!(response.status, 200, "{prefix}: {response:?}");
                        assert_eq!(
                            response.body,
                            json!({"accepted":count,"rejected":0,"duplicates":0})
                        );
                        latency.push(duration.as_micros());
                    }
                    latency.sort_unstable();
                    let pending: i64 =
                        sqlx::query_scalar("SELECT count(*) FROM billing_profit_pending")
                            .fetch_one(w.cp.db().pool())
                            .await
                            .expect("读取负载结束后的追赶量");
                    let catchup = Instant::now();
                    drain(w.cp.db()).await;
                    let catchup_elapsed = catchup.elapsed();
                    let queued: i64 =
                        sqlx::query_scalar("SELECT count(*) FROM billing_profit_pending")
                            .fetch_one(w.cp.db().pool())
                            .await
                            .expect("共享池可用后工人完成追赶");
                    assert_eq!(queued, 0, "不能在负载后饿死后台工人");
                    eprintln!(
                        "{}",
                        json!({
                            "observation":"billing_profit_projection_concurrent_intake", "scenario":scenario,
                            "phase":phase, "batch_rows":count, "repeat":repeat, "concurrency":CONCURRENCY,
                            "elapsed_us":elapsed.as_micros().to_string(),
                            "rows_per_second":(count * CONCURRENCY) as f64 / elapsed.as_secs_f64(),
                            "latency_p50_us":latency[(CONCURRENCY - 1) / 2].to_string(),
                            "latency_p95_us":latency[CONCURRENCY - 1].to_string(),
                            "lock_samples":locks.count, "lock_sample_period_ms":5,
                            "lock_waiter_sample_sum":locks.lock_waiter_sum, "max_lock_waiters":locks.max_lock_waiters,
                            "max_profit_lock_waiters":locks.max_profit_lock_waiters,
                            "worker_active_samples":locks.worker_active_samples,
                            "min_pool_idle":locks.min_pool_idle, "max_pool_size":locks.max_pool_size,
                            "pending_after_http":pending, "pending_after_catchup":queued,
                            "worker_catchup_us":catchup_elapsed.as_micros().to_string(),
                        })
                    );
                }
            }
        }
        // 样本规模决定金额；预热用另一个前缀，既不遗漏多租户，也不让一次摄入重复计费。
        let expected_rows = (SAMPLES * CONCURRENCY * (1 + 10 + 100 + 1000)) as i64;
        for phase in ["disabled", "enabled"] {
            let billed: (i64, i64) = sqlx::query_as(
                "SELECT count(*), COALESCE(sum(charge_micro_cny)::bigint, 0) \
                 FROM pricing_charges_cny WHERE event_id LIKE $1 || '-%'",
            )
            .bind(format!("profit-matrix-{scenario}-{phase}"))
            .fetch_one(w.cp.db().pool())
            .await
            .expect("每种并发场景原始收费严格相同");
            assert_eq!(billed, (expected_rows, expected_rows * 680));
        }
    }
}

fn charge_plan_nodes<'a>(value: &'a Value, nodes: &mut Vec<&'a Value>) {
    match value {
        Value::Object(object) => {
            if object.get("Relation Name").and_then(Value::as_str) == Some("pricing_charges_cny") {
                nodes.push(value);
            }
            for child in object.values() {
                charge_plan_nodes(child, nodes);
            }
        }
        Value::Array(values) => {
            for child in values {
                charge_plan_nodes(child, nodes);
            }
        }
        _ => {}
    }
}

// 两笔收费均位于完整日，边界查询读出或过滤掉任何一笔，都违反不扫描长窗口明细的合同。
// 恢复原来的整段窗口加首尾 OR 时，实际执行计划会在明细节点过滤掉这两笔收费。
#[tokio::test]
async fn profit_summary_plans_never_scan_complete_day_charges_for_empty_partial_edges() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let db = &bare.db;
    let mut tx = db.pool().begin().await.expect("准备六年跨度的执行计划夹具");
    fixture_charge(
        &mut tx,
        "plan-2020",
        at("2020-01-02T08:00:00Z"),
        19,
        Some(4),
    )
    .await;
    fixture_charge(
        &mut tx,
        "plan-2026",
        at("2026-01-02T08:00:00Z"),
        23,
        Some(5),
    )
    .await;
    tx.commit().await.expect("提交执行计划夹具");
    drain(db).await;
    let fixture: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM pricing_charges_cny), (SELECT count(*) FROM billing_profit_pending)",
    ).fetch_one(db.pool()).await.expect("明确验证非空收费来源和空待处理队列");
    assert_eq!(fixture, (2, 0));
    let explain = format!(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {}",
        include_str!("../src/billing/profit_summary.sql"),
    );
    for (name, from, to, whole_from, whole_to) in [
        ("all_time", None, None, None, None),
        (
            "six_year_window",
            Some(at("2020-01-01T08:00:00Z")),
            Some(at("2026-01-03T08:00:00Z")),
            Some(at("2020-01-02T00:00:00Z")),
            Some(at("2026-01-03T00:00:00Z")),
        ),
    ] {
        let plan: Value = sqlx::query_scalar(&explain)
            .bind(from)
            .bind(to)
            .bind(whole_from)
            .bind(whole_to)
            .bind(Vec::<String>::new())
            .persistent(false)
            .fetch_one(db.pool())
            .await
            .expect("运行真实汇总 SQL 的实际执行计划");
        let mut nodes = Vec::new();
        charge_plan_nodes(&plan, &mut nodes);
        let mut executed = 0;
        for node in nodes {
            let loops = node
                .get("Actual Loops")
                .and_then(Value::as_f64)
                .expect("实际执行计划必须报告循环数");
            if loops == 0.0 {
                continue;
            }
            executed += 1;
            let rows = node
                .get("Actual Rows")
                .and_then(Value::as_f64)
                .expect("实际明细扫描必须报告返回行数");
            let removed = match node.get("Rows Removed by Filter") {
                Some(value) => value.as_f64().expect("实际过滤行数必须为数字"),
                None => {
                    assert!(
                        node.get("Filter").is_none(),
                        "过滤节点缺少实际过滤计数: {node}"
                    );
                    0.0
                }
            };
            let rechecked = node
                .get("Rows Removed by Index Recheck")
                .map(|value| value.as_f64().expect("索引重检的实际过滤行数必须为数字"))
                .unwrap_or(0.0);
            assert_eq!(
                rows + removed + rechecked,
                0.0,
                "{name}: 日汇总已覆盖的收费不应被边界明细读取或过滤: {node}; plan={plan}"
            );
        }
        if name == "all_time" {
            assert_eq!(
                executed, 0,
                "空 pending 的全历史汇算不应执行原始收费扫描: {plan}"
            );
        }
        eprintln!(
            "{}",
            json!({"observation":"billing_profit_summary_query_plan", "scenario":name, "plan":plan})
        );
    }
}

// 空队列上准备的旧计划不能在历史快速增长后把一个短批变成整表扫描。
#[tokio::test]
async fn profit_projection_replans_after_empty_queue_and_large_history() {
    let Some(bare) = BareDatabase::new().await else {
        return;
    };
    let one = bare.db.dedicated(1);
    let db = &one;
    async fn idle(db: &Db) {
        drop(db.pool().acquire().await.expect("连接真实的单连接池"));
        let deadline = Instant::now() + Duration::from_secs(1);
        while db.pool().num_idle() != 1 {
            assert!(Instant::now() < deadline, "投影连接没有归还池");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    // 固定冷统计条件；不能让自动ANALYZE在增长中替产品修复缓存计划。
    sqlx::query("ALTER TABLE pricing_charges_cny SET (autovacuum_enabled=false)")
        .execute(db.pool())
        .await
        .expect("保持收费表冷统计");
    sqlx::query("ALTER TABLE billing_profit_rows SET (autovacuum_enabled=false)")
        .execute(db.pool())
        .await
        .expect("保持快照表冷统计");
    sqlx::query("ALTER TABLE billing_profit_pending SET (autovacuum_enabled=false)")
        .execute(db.pool())
        .await
        .expect("保持脏队列冷统计");
    let connection: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(db.pool())
        .await
        .expect("记录真实投影连接");
    for _ in 0..8 {
        idle(db).await;
        assert_eq!(project_once(db).await.expect("空队列投影成功"), 0);
    }
    let cached: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(generic_plans),0)::bigint, COALESCE(sum(custom_plans),0)::bigint \
         FROM pg_prepared_statements WHERE statement LIKE '%billing_profit_current%' \
           AND statement NOT LIKE '%pg_prepared_statements%'",
    )
    .fetch_one(db.pool())
    .await
    .expect("记录真实计划复用次数");
    eprintln!(
        "warm projection connection={connection}, generic={}, custom={}",
        cached.0, cached.1
    );
    const ROWS: i64 = 1_000_000;
    const PREVIOUS: i64 = ROWS - 256;
    sqlx::query(
        "INSERT INTO usage_events(event_id, dp_id, batch_id, occurred_at, event) \
         SELECT 'profit-matrix-multiple_tenants-enabled-1000-4-0-' || lpad(n::text, 9, '0'), \
           '00000000-0000-0000-0000-000000000001'::uuid, \
           '00000000-0000-0000-0000-000000000001'::uuid, \
           '2026-11-17T00:00:00Z'::timestamptz, '{\"test_fixture\":true}'::jsonb \
         FROM generate_series(1, $1) n",
    )
    .bind(ROWS)
    .execute(db.pool())
    .await
    .expect("准备真实历史用量");
    sqlx::query(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, \
          status, charge_micro_cny, cost_micro_cny, engine_version) \
         SELECT event_id, 'test-tenant', 'test-key', '', 'test-model', occurred_at, \
           'charged', 680, NULL, $1 FROM usage_events",
    )
    .bind(i64::from(ENGINE_VERSION))
    .execute(db.pool())
    .await
    .expect("准备真实历史收费");
    sqlx::query(
        "INSERT INTO billing_profit_rows \
         (event_id, occurred_at, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, \
          charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows) \
         SELECT event_id, occurred_at, 680, 0, 0, 0, 0, 1, 0, 1 FROM usage_events \
         WHERE event_id > 'profit-matrix-multiple_tenants-enabled-1000-4-0-000000256'",
    )
    .execute(db.pool())
    .await
    .expect("准备已投影历史快照");
    sqlx::query(
        "INSERT INTO billing_profit_daily \
         (day, charged_micro_cny, adjusted_micro_cny, known_cost_micro_cny, \
          charged_with_cost_micro_cny, rows_with_cost, rows_without_cost, unpriced_rows, rows) \
         SELECT '2026-11-17'::date, $1::numeric * 680, 0, 0, 0, 0, $1::numeric, 0, $1::numeric",
    )
    .bind(PREVIOUS)
    .execute(db.pool())
    .await
    .expect("准备同一历史日汇总");
    let signature = "SELECT count(*)::bigint, sum(charge_micro_cny)::bigint, \
        md5(string_agg(event_id || ':' || charge_micro_cny::text || ':' || status, '' ORDER BY seq)) \
        FROM pricing_charges_cny";
    let before: (i64, i64, String) = sqlx::query_as(signature)
        .fetch_one(db.pool())
        .await
        .expect("读取原始金额指纹");
    assert_eq!((before.0, before.1), (ROWS, ROWS * 680));
    assert_eq!(before.2.len(), 32);
    let same_connection: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(db.pool())
        .await
        .expect("复核增长后的投影连接");
    assert_eq!(same_connection, connection);
    idle(db).await;
    // 仍使用产品原有150ms SQL与250ms整批预算；超时返回0也不能冒充处理成功。
    assert_eq!(project_once(db).await.expect("增长后短批仍成功"), 256);
    let after: (i64, i64, String) = sqlx::query_as(signature)
        .fetch_one(db.pool())
        .await
        .expect("复查原始金额指纹");
    assert_eq!(after, before, "后台投影不得改写收费来源");
    let progress: (i64, i64, String) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM billing_profit_pending), \
           (SELECT count(*) FROM billing_profit_rows), \
           (SELECT charged_micro_cny::text FROM billing_profit_daily WHERE day = '2026-11-17')",
    )
    .fetch_one(db.pool())
    .await
    .expect("复查实际短批与历史金额");
    assert_eq!(progress, (ROWS - 256, ROWS, (ROWS * 680).to_string()));
}
