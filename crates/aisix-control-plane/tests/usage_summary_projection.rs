//! 历史用量汇总的独立合同：合法归属、原始发生期、修正明细及权限均走真实后端。
mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::{json, Value};
use support::world::{
    card, deliver, insert_baseline_backdated, price, seed_key, seed_model, seed_tenant, tokens_680,
    usage, world, World,
};
use support::TestCp;
use uuid::Uuid;

const EVENT_FROM: &str = "2026-01-01T00:00:00Z";
const EVENT_TO: &str = "2026-01-02T00:00:00Z";

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("固定合法时间")
        .with_timezone(&Utc)
}

fn required<'a>(value: &'a Value, name: &str) -> &'a Value {
    value
        .get(name)
        .unwrap_or_else(|| panic!("缺少 {name}: {value}"))
}

fn items(value: &Value) -> &[Value] {
    required(value, "items")
        .as_array()
        .expect("汇总必须返回 items 数组")
}

fn model_row(value: &Value, model: Uuid) -> &Value {
    items(value)
        .iter()
        .find(|row| row["model"]["id"] == json!(model))
        .unwrap_or_else(|| panic!("缺少已授权模型 {model}: {value}"))
}

fn assert_row(row: &Value, requests: i64, charged: i64, estimated: i64, quantities: Value) {
    assert_eq!(required(row, "requests"), &json!(requests));
    assert_eq!(required(row, "charged_micro_cny"), &json!(charged));
    assert_eq!(required(row, "estimated_requests"), &json!(estimated));
    assert_eq!(required(row, "quantities"), &quantities);
    assert!(required(row, "period_start").as_str().is_some());
    assert!(row["model"]["name"]
        .as_str()
        .is_some_and(|name| !name.is_empty()));
}

fn quantities(calls: i64) -> Value {
    json!({"input":200 * calls,"cache_read":800 * calls,"output":200 * calls})
}

fn query(group: &str, from: &str, to: &str) -> String {
    format!("group={group}&from={from}&to={to}")
}

struct KeyScope {
    tenant: Uuid,
    member: Uuid,
    key: String,
}

fn own_scope(w: &World) -> KeyScope {
    KeyScope {
        tenant: w.tenant,
        member: w.member,
        key: w.key.clone(),
    }
}

fn event(
    w: &World,
    id: &str,
    scope: &KeyScope,
    model: Uuid,
    time: DateTime<Utc>,
    final_flag: Option<Value>,
    estimated: bool,
) -> Value {
    let mut row = usage(w, id, model, tokens_680());
    row["team_id"] = json!(scope.tenant);
    row["user_id"] = json!(scope.member);
    row["api_key_id"] = json!(scope.key);
    row["model_id"] = json!(model);
    row["occurred_at"] = json!(time);
    row["usage_estimated"] = json!(estimated);
    if let Some(flag) = final_flag {
        row["is_final_attempt"] = flag;
    }
    row
}

async fn grant(cp: &TestCp, tenant: Uuid, models: &[Uuid]) {
    let changed = sqlx::query(
        "UPDATE tenants SET allowed_model_ids = allowed_model_ids || $2::uuid[] WHERE id = $1",
    )
    .bind(tenant)
    .bind(models)
    .execute(cp.db().pool())
    .await
    .expect("独立 fixture 明确授权模型");
    assert_eq!(changed.rows_affected(), 1);
}

async fn pin_empty_history(cp: &TestCp) {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_events")
        .fetch_one(cp.db().pool())
        .await
        .expect("固定独立空历史");
    assert_eq!(count, 0);
    let changed = sqlx::query("UPDATE usage_metric_input_backfill SET after_seq = 0, high_seq = 0, complete = true WHERE version = 1")
        .execute(cp.db().pool()).await.expect("不让后台替技术用例抢补缺口");
    assert_eq!(changed.rows_affected(), 1);
}

async fn send(w: &World, events: Vec<Value>) {
    let count = events.len();
    let response = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.body,
        json!({"accepted":count,"rejected":0,"duplicates":0})
    );
}

async fn get(cp: &TestCp, path: &str, cookie: &str) -> Value {
    let response = cp.send("GET", path, Some(cookie), None).await;
    assert_eq!(response.status, 200, "{path}: {response:?}");
    let _ = items(&response.body);
    response.body
}

async fn snapshots(cp: &TestCp, requests: &[(String, String)]) -> Vec<Value> {
    let mut result = Vec::new();
    for (path, cookie) in requests {
        result.push(get(cp, path, cookie).await);
    }
    result
}

async fn finances(cp: &TestCp) -> Vec<Value> {
    let mut result = Vec::new();
    for table in [
        "pricing_charges_cny",
        "pricing_adjustments_cny",
        "billing_ledger_cny",
    ] {
        let sql = format!("SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb) FROM {table} t");
        result.push(
            sqlx::query_scalar(&sql)
                .fetch_one(cp.db().pool())
                .await
                .expect("固定原账务快照"),
        );
    }
    result
}

// 使用现有旧 writer 合同追加已应用修正；不 UPDATE 原 charge，也不伪造 HTTP 回答。
async fn correction(
    w: &World,
    event_id: &str,
    run: Uuid,
    delta: i64,
    created: DateTime<Utc>,
    lines: Option<Value>,
) {
    let mut tx =
        w.cp.db()
            .pool()
            .begin()
            .await
            .expect("开始旧 writer 修正事务");
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id,status,filter,reason,pricing_version,engine_version,events,changed,still_unpriced, \
          skipped_unattributed,delta_micro_cny,by_tenant,created_by,applied_by,created_at,applied_at) \
         VALUES ($1,'applied','{}','历史汇总独立合同',0,$2,1,1,0,0,$3,$4,$5,$5,$6,$6)",
    ).bind(run).bind(i64::from(aisix_pricing::ENGINE_VERSION)).bind(delta)
        .bind(json!([{"tenant_id":w.tenant,"events":1,"changed":1,"delta_micro_cny":delta}]))
        .bind(w.admin.id).bind(created).execute(&mut *tx).await.expect("已应用批次真实存在");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny \
         (run_id,event_id,delta_micro_cny,engine_version,reason,created_by,created_at,lines) \
         VALUES ($1,$2,$3,$4,'历史汇总独立合同',$5,$6,$7)",
    )
    .bind(run)
    .bind(event_id)
    .bind(delta)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .bind(w.admin.id)
    .bind(created)
    .bind(lines)
    .execute(&mut *tx)
    .await
    .expect("真实追加修正及可空旧明细");
    tx.commit().await.expect("提交完整旧 writer 修正");
}

struct CorrectedWorld {
    w: World,
    paid: Uuid,
    unpaid: Uuid,
}

async fn corrected_world() -> Option<CorrectedWorld> {
    let w = world().await?;
    pin_empty_history(&w.cp).await;
    let paid = seed_model(&w.cp, &w.admin).await;
    let unpaid = seed_model(&w.cp, &w.admin).await;
    grant(&w.cp, w.tenant, &[paid, unpaid]).await;
    insert_baseline_backdated(
        w.cp.db(),
        paid,
        serde_json::to_value(card()).expect("公开价格卡"),
        at("2025-12-30T00:00:00Z"),
    )
    .await;
    let scope = own_scope(&w);
    let time = at("2026-01-01T10:00:00Z");
    let rows = vec![
        event(
            &w,
            "summary-paid-final",
            &scope,
            paid,
            time,
            Some(json!(true)),
            false,
        ),
        event(
            &w,
            "summary-paid-retry",
            &scope,
            paid,
            time,
            Some(json!(false)),
            true,
        ),
        event(
            &w,
            "summary-unpaid-final",
            &scope,
            unpaid,
            time,
            Some(json!(true)),
            false,
        ),
        event(
            &w,
            "summary-unpaid-false",
            &scope,
            unpaid,
            time,
            Some(json!(false)),
            false,
        ),
        event(
            &w,
            "summary-unpaid-missing",
            &scope,
            unpaid,
            time,
            None,
            false,
        ),
        event(
            &w,
            "summary-unpaid-null",
            &scope,
            unpaid,
            time,
            Some(Value::Null),
            false,
        ),
        event(
            &w,
            "summary-unpaid-string",
            &scope,
            unpaid,
            time,
            Some(json!("true")),
            false,
        ),
        event(
            &w,
            "summary-corrected-retry",
            &scope,
            unpaid,
            time,
            Some(json!(false)),
            false,
        ),
    ];
    send(&w, rows).await;
    let paid_rows: Vec<(String, String, Option<i64>, bool)> = sqlx::query_as(
        "SELECT event_id,status,charge_micro_cny,estimated FROM pricing_charges_cny \
         WHERE requested_model_id = $1::text ORDER BY event_id",
    )
    .bind(paid)
    .fetch_all(w.cp.db().pool())
    .await
    .expect("先验证合法付费前提");
    assert_eq!(
        paid_rows,
        vec![
            (
                "summary-paid-final".into(),
                "charged".into(),
                Some(680),
                false
            ),
            (
                "summary-paid-retry".into(),
                "charged".into(),
                Some(680),
                true
            )
        ]
    );
    let unpaid_rows: (i64, i64) = sqlx::query_as(
        "SELECT count(*),(count(*) FILTER (WHERE status='unpriced' AND charge_micro_cny IS NULL))::bigint \
         FROM pricing_charges_cny WHERE requested_model_id=$1::text",
    ).bind(unpaid).fetch_one(w.cp.db().pool()).await.expect("无价格模型必须真实未计价");
    assert_eq!(unpaid_rows, (6, 6));
    let billed_lines = json!([{"dimension":"input","quantity":200,"rate":"1"},
        {"dimension":"cache_read","quantity":800,"rate":"0.1"},{"dimension":"output","quantity":200,"rate":"2"}]);
    correction(
        &w,
        "summary-paid-final",
        Uuid::from_u128(1),
        17,
        at("2026-01-03T08:00:00Z"),
        Some(billed_lines),
    )
    .await;
    // 时间更新优先于 UUID：更早的最高 UUID 不能胜出；同一时刻再由较大 UUID 决定。
    correction(
        &w,
        "summary-corrected-retry",
        Uuid::from_u128(0xffff),
        7,
        at("2026-01-03T08:00:00Z"),
        Some(json!([{"dimension":"input","quantity":11,"rate":"1"}])),
    )
    .await;
    correction(
        &w,
        "summary-corrected-retry",
        Uuid::from_u128(0x20),
        -3,
        at("2026-01-03T09:00:00Z"),
        Some(json!([{"dimension":"input","quantity":13,"rate":"1"}])),
    )
    .await;
    correction(
        &w,
        "summary-corrected-retry",
        Uuid::from_u128(0x21),
        -4,
        at("2026-01-03T09:00:00Z"),
        None,
    )
    .await;
    let net: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),COALESCE(sum(delta_micro_cny)::bigint,0), \
          (count(*) FILTER (WHERE lines IS NULL))::bigint FROM pricing_adjustments_cny \
         WHERE event_id='summary-corrected-retry'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("净零不是没有修正记录");
    assert_eq!(net, (3, 0, 1));
    Some(CorrectedWorld { w, paid, unpaid })
}

fn assert_corrected_summary(body: &Value, paid: Uuid, unpaid: Uuid) {
    assert_eq!(items(body).len(), 2);
    assert_row(model_row(body, paid), 2, 1377, 1, quantities(2));
    // 一个真实 final 加一个净零已修正 retry；NULL 最新明细不能回落到旧的 11/13。
    assert_row(model_row(body, unpaid), 2, 0, 0, json!({}));
    for row in items(body) {
        assert_eq!(required(row, "period_start"), "2025-12-31T16:00:00Z");
    }
}

#[tokio::test]
async fn summaries_keep_billed_retries_net_zero_corrections_and_the_original_event_period() {
    let Some(s) = corrected_world().await else {
        return;
    };
    let owner =
        s.w.cp
            .member_login_seeded(s.w.tenant, MemberRole::Owner)
            .await;
    let viewer = s.w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let range = query("day", EVENT_FROM, EVENT_TO);
    let tenant = get(
        &s.w.cp,
        &format!("/api/usage/summary?{range}"),
        owner.cookie(),
    )
    .await;
    let admin = get(
        &s.w.cp,
        &format!("/admin/tenants/{}/usage/summary?{range}", s.w.tenant),
        viewer.cookie(),
    )
    .await;
    assert_corrected_summary(&tenant, s.paid, s.unpaid);
    assert_eq!(admin, tenant);
    // 原付费行的明细非空；最新 NULL 不得退回原 charge 或更早的已知修正。
    let original_lines: Value = sqlx::query_scalar(
        "SELECT lines FROM pricing_charges_cny WHERE event_id='summary-paid-retry'",
    )
    .fetch_one(s.w.cp.db().pool())
    .await
    .expect("先验证付费行确有非空原明细");
    assert_eq!(original_lines.as_array().expect("原付费明细数组").len(), 3);
    correction(
        &s.w,
        "summary-paid-retry",
        Uuid::from_u128(0x30),
        5,
        at("2026-01-03T10:00:00Z"),
        Some(original_lines),
    )
    .await;
    correction(
        &s.w,
        "summary-paid-retry",
        Uuid::from_u128(0x31),
        -5,
        at("2026-01-03T11:00:00Z"),
        None,
    )
    .await;
    let net: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*),COALESCE(sum(delta_micro_cny)::bigint,0), \
         (count(*) FILTER (WHERE lines IS NULL))::bigint FROM pricing_adjustments_cny \
         WHERE event_id='summary-paid-retry'",
    )
    .fetch_one(s.w.cp.db().pool())
    .await
    .expect("两个非零修正净额为零且最新缺明细");
    assert_eq!(net, (2, 0, 1));
    let without_detail = get(
        &s.w.cp,
        &format!("/api/usage/summary?{range}"),
        owner.cookie(),
    )
    .await;
    let admin_without_detail = get(
        &s.w.cp,
        &format!("/admin/tenants/{}/usage/summary?{range}", s.w.tenant),
        viewer.cookie(),
    )
    .await;
    assert_eq!(items(&without_detail).len(), 2);
    assert_row(
        model_row(&without_detail, s.paid),
        2,
        1377,
        1,
        quantities(1),
    );
    assert_row(model_row(&without_detail, s.unpaid), 2, 0, 0, json!({}));
    assert_ne!(without_detail, tenant, "未知明细不能伪装成原来两份已知量");
    assert_eq!(admin_without_detail, without_detail);
    let late = query("day", "2026-01-03T00:00:00Z", "2026-01-04T00:00:00Z");
    for (path, cookie) in [
        (format!("/api/usage/summary?{late}"), owner.cookie()),
        (
            format!("/admin/tenants/{}/usage/summary?{late}", s.w.tenant),
            viewer.cookie(),
        ),
    ] {
        assert_eq!(get(&s.w.cp, &path, cookie).await, json!({"items":[]}));
    }
}

#[tokio::test]
async fn summaries_intersect_real_member_sessions_own_keys_and_models_with_nonempty_cells() {
    let Some(w) = world().await else { return };
    pin_empty_history(&w.cp).await;
    let models = [
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    ];
    for model in models {
        price(&w.cp, model).await;
    }
    grant(&w.cp, w.tenant, &models).await;
    let members = [
        w.cp.member_login_seeded(w.tenant, MemberRole::Member).await,
        w.cp.member_login_seeded(w.tenant, MemberRole::Member).await,
    ];
    let mut scopes = Vec::new();
    for member in &members {
        for _ in 0..2 {
            scopes.push(KeyScope {
                tenant: w.tenant,
                member: member.id,
                key: seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await,
            });
        }
    }
    let outside = seed_tenant(&w.cp, &w.admin).await;
    let outsider = w.cp.member_login_seeded(outside, MemberRole::Member).await;
    let foreign = KeyScope {
        tenant: outside,
        member: outsider.id,
        key: seed_key(&w.cp, &w.admin, outside, Some(outsider.id)).await,
    };
    grant(&w.cp, outside, &[models[0]]).await;
    let from = Utc::now() - TimeDelta::minutes(1);
    let to = Utc::now() + TimeDelta::minutes(1);
    let time = Utc::now();
    let mut events = Vec::new();
    for (index, scope) in scopes.iter().enumerate() {
        for (m, model) in models.iter().enumerate() {
            events.push(event(
                &w,
                &format!("summary-cell-{index}-{m}"),
                scope,
                *model,
                time,
                Some(json!(true)),
                false,
            ));
        }
    }
    events.push(event(
        &w,
        "summary-foreign",
        &foreign,
        models[0],
        time,
        Some(json!(true)),
        false,
    ));
    send(&w, events).await;
    let charges: (i64,i64) = sqlx::query_as(
        "SELECT count(*),(count(*) FILTER (WHERE status='charged' AND charge_micro_cny=680))::bigint \
         FROM pricing_charges_cny WHERE event_id LIKE 'summary-cell-%' OR event_id='summary-foreign'",
    ).fetch_one(w.cp.db().pool()).await.expect("九个合法归属必须全部真正收费");
    assert_eq!(charges, (9, 9));
    let range = query(
        "day",
        &from.to_rfc3339_opts(SecondsFormat::Secs, true),
        &to.to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let admin = w.cp.member_login_seeded(w.tenant, MemberRole::Admin).await;
    let billing =
        w.cp.member_login_seeded(w.tenant, MemberRole::Billing)
            .await;
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let tenant_path = format!("/api/usage/summary?{range}");
    let operator_path = format!("/admin/tenants/{}/usage/summary?{range}", w.tenant);
    let whole = get(&w.cp, &tenant_path, owner.cookie()).await;
    assert_eq!(items(&whole).len(), 2);
    for model in models {
        assert_row(model_row(&whole, model), 4, 2720, 0, quantities(4));
    }
    assert_eq!(get(&w.cp, &tenant_path, admin.cookie()).await, whole);
    assert_eq!(get(&w.cp, &tenant_path, billing.cookie()).await, whole);
    assert_eq!(get(&w.cp, &operator_path, viewer.cookie()).await, whole);
    for (index, member) in members.iter().enumerate() {
        let own = get(&w.cp, &tenant_path, member.cookie()).await;
        assert_eq!(items(&own).len(), 2);
        for model in models {
            assert_row(model_row(&own, model), 2, 1360, 0, quantities(2));
        }
        for scope in &scopes[index * 2..index * 2 + 2] {
            for model in models {
                let path = format!("{tenant_path}&api_key_id={}&model_id={model}", scope.key);
                for cookie in [
                    member.cookie(),
                    owner.cookie(),
                    admin.cookie(),
                    billing.cookie(),
                ] {
                    let cell = get(&w.cp, &path, cookie).await;
                    assert_eq!(items(&cell).len(), 1, "有效交集不能以空结果假通过");
                    assert_row(model_row(&cell, model), 1, 680, 0, quantities(1));
                }
                let path = format!("{operator_path}&api_key_id={}&model_id={model}", scope.key);
                let cell = get(&w.cp, &path, viewer.cookie()).await;
                assert_eq!(items(&cell).len(), 1);
                assert_row(model_row(&cell, model), 1, 680, 0, quantities(1));
            }
        }
        let other_key = &scopes[(1 - index) * 2].key;
        assert_eq!(
            get(
                &w.cp,
                &format!(
                    "{tenant_path}&api_key_id={other_key}&model_id={}",
                    models[0]
                ),
                member.cookie()
            )
            .await,
            json!({"items":[]})
        );
    }
    let foreign_body = get(
        &w.cp,
        &format!("/admin/tenants/{outside}/usage/summary?{range}"),
        viewer.cookie(),
    )
    .await;
    assert_eq!(items(&foreign_body).len(), 1);
    assert_row(
        model_row(&foreign_body, models[0]),
        1,
        680,
        0,
        quantities(1),
    );
    assert_eq!(
        get(
            &w.cp,
            &format!("{tenant_path}&api_key_id={}", foreign.key),
            owner.cookie()
        )
        .await,
        json!({"items":[]})
    );
    let unauthenticated = w.cp.send("GET", &tenant_path, None, None).await;
    assert_eq!(unauthenticated.status, 401, "{unauthenticated:?}");
    for cookie in [
        owner.cookie(),
        admin.cookie(),
        billing.cookie(),
        members[0].cookie(),
    ] {
        let denied = w.cp.send("GET", &operator_path, Some(cookie), None).await;
        assert_eq!(denied.status, 401, "租户身份不是 operator: {denied:?}");
    }
}

#[tokio::test]
async fn summaries_keep_complete_json_for_ready_missing_and_poisoned_unknown_projection_states() {
    let Some(s) = corrected_world().await else {
        return;
    };
    let owner =
        s.w.cp
            .member_login_seeded(s.w.tenant, MemberRole::Owner)
            .await;
    let viewer = s.w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let range = query("day", EVENT_FROM, EVENT_TO);
    let mut requests = Vec::new();
    for suffix in [
        String::new(),
        format!("&api_key_id={}", s.w.key),
        format!("&model_id={}", s.unpaid),
    ] {
        requests.push((
            format!("/api/usage/summary?{range}{suffix}"),
            owner.cookie().to_owned(),
        ));
        requests.push((
            format!(
                "/admin/tenants/{}/usage/summary?{range}{suffix}",
                s.w.tenant
            ),
            viewer.cookie().to_owned(),
        ));
    }
    let baseline = snapshots(&s.w.cp, &requests).await;
    assert_corrected_summary(&baseline[0], s.paid, s.unpaid);
    for body in &baseline {
        assert!(!items(body).is_empty());
    }
    let money = finances(&s.w.cp).await;
    let ready: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_metric_inputs WHERE projection_version=1")
            .fetch_one(s.w.cp.db().pool())
            .await
            .expect("八条事件均有实际 ready 投影");
    assert_eq!(ready, 8);
    // 下面故意损坏内部缓存，仅证明 marker=1 权威与未知版本回退；不是合法业务造数。
    let poisoned = sqlx::query("UPDATE usage_metric_inputs SET json_is_final_attempt='false'::jsonb WHERE event_id='summary-unpaid-final'")
        .execute(s.w.cp.db().pool()).await.expect("损坏已知版本 final 值");
    assert_eq!(poisoned.rows_affected(), 1);
    let changed = snapshots(&s.w.cp, &requests).await;
    assert_eq!(required(model_row(&changed[0], s.unpaid), "requests"), 1);
    assert_ne!(changed, baseline, "读路径必须实际使用已知版本投影");
    sqlx::query("UPDATE usage_metric_inputs SET json_is_final_attempt=NULL WHERE event_id='summary-unpaid-final'")
        .execute(s.w.cp.db().pool()).await.expect("合法 Unknown SQL NULL 不能逐字段 COALESCE");
    let changed = snapshots(&s.w.cp, &requests).await;
    assert_eq!(required(model_row(&changed[0], s.unpaid), "requests"), 1);
    sqlx::query(
        "UPDATE usage_metric_inputs SET projection_version=9 WHERE event_id='summary-unpaid-final'",
    )
    .execute(s.w.cp.db().pool())
    .await
    .expect("未知版本忽略其中坏值");
    assert_eq!(snapshots(&s.w.cp, &requests).await, baseline);
    sqlx::query("DELETE FROM usage_metric_inputs WHERE event_id IN ('summary-unpaid-final','summary-paid-retry')")
        .execute(s.w.cp.db().pool()).await.expect("真实混合缺失状态");
    assert_eq!(snapshots(&s.w.cp, &requests).await, baseline);
    sqlx::query("DELETE FROM usage_metric_inputs")
        .execute(s.w.cp.db().pool())
        .await
        .expect("全旧行 fallback");
    assert_eq!(snapshots(&s.w.cp, &requests).await, baseline);
    sqlx::query("UPDATE usage_events SET event=event")
        .execute(s.w.cp.db().pool())
        .await
        .expect("旧 UPDATE writer 重建所有投影");
    let restored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_metric_inputs WHERE projection_version=1")
            .fetch_one(s.w.cp.db().pool())
            .await
            .expect("八条 ready 重新成立");
    assert_eq!(restored, 8);
    assert_eq!(snapshots(&s.w.cp, &requests).await, baseline);
    assert_eq!(finances(&s.w.cp).await, money, "所有读路径均不改账务");
}

#[tokio::test]
async fn legacy_nonobject_events_keep_paid_history_and_never_invent_unpriced_final_attempts() {
    let Some(w) = world().await else { return };
    pin_empty_history(&w.cp).await;
    let paid = seed_model(&w.cp, &w.admin).await;
    let unpaid = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, paid).await;
    grant(&w.cp, w.tenant, &[paid, unpaid]).await;
    let scope = own_scope(&w);
    let from = Utc::now() - TimeDelta::minutes(1);
    let to = Utc::now() + TimeDelta::minutes(1);
    let time = Utc::now();
    let mut events = Vec::new();
    let mut ids = Vec::new();
    let mut roots = Vec::new();
    for (label, model) in [("paid", paid), ("unpaid", unpaid)] {
        for (index, root) in ["null", "true", "[\"is_final_attempt\"]"]
            .into_iter()
            .enumerate()
        {
            let id = format!("summary-legacy-{label}-{index}");
            events.push(event(
                &w,
                &id,
                &scope,
                model,
                time,
                Some(json!(true)),
                false,
            ));
            ids.push(id);
            roots.push(root.to_owned());
        }
    }
    send(&w, events).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let range = query(
        "day",
        &from.to_rfc3339_opts(SecondsFormat::Secs, true),
        &to.to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    let requests = [
        (
            format!("/api/usage/summary?{range}"),
            owner.cookie().to_owned(),
        ),
        (
            format!("/admin/tenants/{}/usage/summary?{range}", w.tenant),
            viewer.cookie().to_owned(),
        ),
    ];
    let before = snapshots(&w.cp, &requests).await;
    for body in &before {
        assert_eq!(items(body).len(), 2);
        assert_row(model_row(body, paid), 3, 2040, 0, quantities(3));
        assert_row(model_row(body, unpaid), 3, 0, 0, json!({}));
    }
    let money = finances(&w.cp).await;
    let changed = sqlx::query(
        "UPDATE usage_events ue SET event=legacy.raw::jsonb \
         FROM unnest($1::text[],$2::text[]) AS legacy(event_id,raw) WHERE ue.event_id=legacy.event_id",
    ).bind(ids).bind(roots).execute(w.cp.db().pool()).await.expect("仅技术用例注入已存在的旧非对象行");
    assert_eq!(changed.rows_affected(), 6);
    let after = snapshots(&w.cp, &requests).await;
    for body in &after {
        assert_eq!(items(body).len(), 1, "未收费非对象根不能成为 final");
        assert_row(model_row(body, paid), 3, 2040, 0, quantities(3));
    }
    assert_eq!(after[0], after[1]);
    assert_eq!(
        finances(&w.cp).await,
        money,
        "旧事件形态不得改写已记账的金额和数量"
    );
}
