//! 请求级日志在数据库分页前归并；详情保留完整尝试和既有权限边界。
mod support;

use aisix_control_plane::auth::MemberRole;
use serde_json::{json, Value};
use support::world::{
    deliver, price, seed_key, seed_model, seed_tenant, tokens_680, usage, world, World,
};
use support::{Resp, TestCp};
use uuid::Uuid;

const TRACE: &str = "1234567890abcdef1234567890abcdef";

async fn setup() -> Option<(World, Uuid, String)> {
    let w = world().await?;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    sqlx::query(
        "UPDATE tenants SET allowed_model_ids = array_append(allowed_model_ids, $2) WHERE id = $1",
    )
    .bind(w.tenant)
    .bind(model)
    .execute(w.cp.db().pool())
    .await
    .expect("授予测试模型");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    Some((w, model, owner.cookie().to_owned()))
}

fn attempt(w: &World, model: Uuid, id: &str, request: &str, fields: Value) -> Value {
    let mut event = usage(
        w,
        id,
        model,
        json!({
            "request_id":request,"trace_id":TRACE,"attempt_index":0,
            "attempt_kind":"initial","is_final_attempt":true,
        }),
    );
    for (key, value) in fields.as_object().expect("尝试字段") {
        event[key] = value.clone();
    }
    event
}

async fn send(w: &World, events: Vec<Value>) {
    let reply = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(reply.status, 200, "{reply:?}");
    assert_eq!(reply.body.get("rejected"), Some(&json!(0)));
}

async fn get(cp: &TestCp, path: &str, cookie: &str) -> Resp {
    let reply = cp.send("GET", path, Some(cookie), None).await;
    assert_eq!(reply.status, 200, "{path}: {reply:?}");
    reply
}

fn items(reply: &Resp) -> &[Value] {
    reply.body["items"].as_array().expect("显式日志列表")
}

fn ids(rows: &[Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row["event_id"].as_str().expect("事件标识"))
        .collect()
}

fn group(row: &Value) -> &Value {
    row.get("request_group").expect("显式请求级摘要")
}

fn assert_counts(row: &Value, attempts: u64, failovers: u64, retries: u64, complete: bool) {
    for (field, expected) in [
        ("attempt_count", json!(attempts)),
        ("failover_count", json!(failovers)),
        ("retry_count", json!(retries)),
        ("complete", json!(complete)),
    ] {
        assert_eq!(group(row).get(field), Some(&expected), "{field}: {row}");
    }
}

fn assert_tenant_fields(value: &Value) {
    match value {
        Value::Object(fields) => {
            for (key, child) in fields {
                assert!(
                    !matches!(
                        key.as_str(),
                        "cost_micro_cny"
                            | "cost_micro_usd"
                            | "provider_key_id"
                            | "upstream_account_id"
                            | "upstream_account_name"
                            | "upstream_model"
                            | "model_id"
                            | "stored_event"
                    ),
                    "租户详情不能出现上游或成本字段 {key}: {value}"
                );
                assert_tenant_fields(child);
            }
        }
        Value::Array(rows) => rows.iter().for_each(assert_tenant_fields),
        _ => {}
    }
}

async fn success_chain(w: &World, model: Uuid, request: &str) {
    let first_target = Uuid::new_v4();
    let next_target = Uuid::new_v4();
    let mut first = attempt(
        w,
        model,
        "chain-initial",
        request,
        json!({
            "status_code":503,"error_class":"upstream_status",
            "error_message":"initial private-upstream-error sk-pricing-upstream-7c1",
            "is_final_attempt":false,"model_id":first_target,"provider_key_id":first_target,
        }),
    );
    let retry = attempt(
        w,
        model,
        "chain-retry",
        request,
        json!({
            "status_code":503,"error_class":"upstream_status","error_message":"retry private-upstream-error",
            "attempt_index":1,"attempt_kind":"retry","is_final_attempt":false,
            "model_id":first_target,"provider_key_id":first_target,
        }),
    );
    let mut success = attempt(
        w,
        model,
        "chain-success",
        request,
        json!({
            "attempt_index":2,"attempt_kind":"fallback","model_id":next_target,
            "provider_key_id":next_target,"status_code":200,"error_class":"",
        }),
    );
    for (key, value) in tokens_680().as_object().expect("已计费用量") {
        first[key] = value.clone();
        success[key] = value.clone();
    }
    send(w, vec![first, retry, success]).await;
}

#[tokio::test]
async fn a_successful_request_is_one_row_and_details_keep_billed_and_free_attempts() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    success_chain(&w, model, "chain").await;
    let before: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(charge_micro_cny),0)::bigint FROM pricing_charges_cny",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("读取既有费用");
    assert_eq!(before, (3, 1360), "失败收费与最终成功都已独立入账");
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let listed = get(
            &w.cp,
            &format!("{path}?group_requests=true&limit=1"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&listed)), ["chain-success"]);
        assert_eq!(listed.body.get("next_before"), Some(&Value::Null));
        let call = &items(&listed)[0];
        assert_eq!(
            call.get("request_id"),
            Some(&json!("chain")),
            "逻辑请求行保留可检索的请求身份"
        );
        assert_counts(call, 3, 1, 1, true);
        assert_eq!(group(call).get("charge_micro_cny"), Some(&json!("1360")));
        assert_eq!(group(call).get("adjusted_micro_cny"), Some(&json!("0")));
        assert!(call.get("attempts").is_none(), "列表不能预取逐条日志");
        for anchor in ["chain-initial", "chain-retry", "chain-success"] {
            let detail = get(
                &w.cp,
                &format!("{path}?group_requests=true&include_attempts=true&event_id={anchor}&outcome=failed"),
                cookie,
            )
            .await;
            assert_eq!(ids(items(&detail)), ["chain-success"]);
            let attempts = items(&detail)[0]["attempts"]
                .as_array()
                .expect("完整尝试链");
            assert_eq!(
                ids(attempts),
                ["chain-initial", "chain-retry", "chain-success"]
            );
            assert_eq!(
                items(&detail)[0].get("next_attempt_after"),
                Some(&Value::Null)
            );
            if path.starts_with("/admin") {
                assert!(attempts[0]["error_message"]
                    .as_str()
                    .is_some_and(|message| message.contains("initial private-upstream-error")));
                assert!(attempts[1]["error_message"]
                    .as_str()
                    .is_some_and(|message| message.contains("retry private-upstream-error")));
                for attempt in attempts {
                    assert_eq!(attempt["stored_event"]["event_id"], attempt["event_id"]);
                    assert!(attempt["stored_event"].get("event").is_some());
                }
                assert!(!detail.body.to_string().contains("sk-pricing-upstream-7c1"));
            } else {
                assert_tenant_fields(&detail.body);
                assert!(!detail.body.to_string().contains("private-upstream-error"));
                assert!(attempts[0]["error_message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty()));
                assert_eq!(attempts[1].get("attempt_kind"), Some(&json!("retry")));
                assert_eq!(attempts[2].get("attempt_index"), Some(&json!(2)));
                assert_eq!(attempts[2].get("is_final_attempt"), Some(&json!(true)));
            }
        }
    }
    let after: (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(charge_micro_cny),0)::bigint FROM pricing_charges_cny",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("只读查询后的费用");
    assert_eq!(after, before, "分组只是查询投影，不能重写费用");
    let original = get(&w.cp, "/admin/pricing/charges", w.admin.cookie()).await;
    assert_eq!(items(&original).len(), 3, "默认接口保留逐尝试语义");
}

#[tokio::test]
async fn grouped_pagination_and_outcome_filters_use_whole_requests() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    success_chain(&w, model, "success-group").await;
    let first_target = Uuid::new_v4();
    let next_target = Uuid::new_v4();
    send(&w, vec![
        attempt(&w, model, "failure-initial", "failure-group", json!({"model_id":first_target,"is_final_attempt":false,"status_code":503,"error_class":"upstream_status"})),
        attempt(&w, model, "failure-final", "failure-group", json!({"model_id":next_target,"attempt_index":1,"attempt_kind":"fallback","status_code":502,"error_class":"upstream_status","error_message":"final-private-error"})),
    ]).await;
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let first = get(
            &w.cp,
            &format!("{path}?group_requests=true&limit=1"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&first)), ["failure-final"]);
        assert_counts(&items(&first)[0], 2, 1, 0, true);
        let before = first.body["next_before"]
            .as_i64()
            .expect("下一逻辑请求的游标");
        let second = get(
            &w.cp,
            &format!("{path}?group_requests=true&limit=1&before={before}"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&second)), ["chain-success"]);
        assert_counts(&items(&second)[0], 3, 1, 1, true);
        assert_eq!(second.body.get("next_before"), Some(&Value::Null));
        let failed = get(
            &w.cp,
            &format!("{path}?group_requests=true&outcome=failed"),
            cookie,
        )
        .await;
        assert_eq!(
            ids(items(&failed)),
            ["failure-final"],
            "早期失败不能把最终成功组筛入失败结果"
        );
        let succeeded = get(
            &w.cp,
            &format!("{path}?group_requests=true&outcome=succeeded"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&succeeded)), ["chain-success"]);
        if path.starts_with("/api") {
            assert_eq!(items(&failed)[0].get("status"), Some(&json!("failed")));
            assert_eq!(
                items(&failed)[0].get("error_class"),
                Some(&json!("upstream_unavailable"))
            );
            assert!(!failed.body.to_string().contains("final-private-error"));
        }
    }
}

#[tokio::test]
async fn later_delivery_of_an_old_failure_cannot_replace_the_successful_terminal_record() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    let first_target = Uuid::new_v4();
    let next_target = Uuid::new_v4();
    let failed = attempt(
        &w,
        model,
        "delivered-failure-late",
        "late-chain",
        json!({
            "model_id":first_target,"is_final_attempt":false,"status_code":503,"error_class":"upstream_status",
        }),
    );
    let success = attempt(
        &w,
        model,
        "delivered-success-first",
        "late-chain",
        json!({
            "model_id":next_target,"attempt_index":1,"attempt_kind":"fallback",
        }),
    );
    send(&w, vec![success]).await;
    send(&w, vec![failed]).await;
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let reply = get(&w.cp, &format!("{path}?group_requests=true"), cookie).await;
        assert_eq!(ids(items(&reply)), ["delivered-success-first"]);
        assert_counts(&items(&reply)[0], 2, 1, 0, true);
        let failed_filter = get(
            &w.cp,
            &format!("{path}?group_requests=true&outcome=failed"),
            cookie,
        )
        .await;
        assert!(items(&failed_filter).is_empty(), "晚交付不能覆盖实际终态");
    }
}

#[tokio::test]
async fn attempt_detail_pagination_keeps_all_pages_and_does_not_invent_a_final_attempt() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    success_chain(&w, model, "paged-chain").await;
    let mut unfinished = attempt(&w, model, "unfinished", "unfinished-chain", tokens_680());
    unfinished
        .as_object_mut()
        .expect("事件对象")
        .remove("is_final_attempt");
    send(&w, vec![unfinished]).await;
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let mut cursor = None;
        let mut seen = Vec::new();
        for number in 0..3 {
            let continuation = cursor
                .map(|seq| format!("&attempts_after={seq}"))
                .unwrap_or_default();
            let response = get(&w.cp, &format!("{path}?group_requests=true&include_attempts=true&event_id=chain-retry&attempt_limit=1{continuation}"), cookie).await;
            let call = &items(&response)[0];
            assert_counts(call, 3, 1, 1, true);
            let attempts = call["attempts"].as_array().expect("尝试分页");
            assert_eq!(attempts.len(), 1);
            seen.extend(ids(attempts).into_iter().map(str::to_owned));
            cursor = call
                .get("next_attempt_after")
                .expect("显式尝试游标")
                .as_i64();
            assert_eq!(cursor.is_some(), number < 2, "最后一页才到链末尾");
        }
        assert_eq!(seen, ["chain-initial", "chain-retry", "chain-success"]);
        let response = get(
            &w.cp,
            &format!("{path}?group_requests=true&include_attempts=true&event_id=unfinished"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&response)), ["unfinished"]);
        assert_counts(&items(&response)[0], 1, 0, 0, false);
    }
}

#[tokio::test]
async fn a_request_group_sums_adjustments_on_every_billed_attempt() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    success_chain(&w, model, "adjusted-chain").await;
    let run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id,status,filter,reason,pricing_version,engine_version,events,changed,still_unpriced,skipped_unattributed,delta_micro_cny,by_tenant,created_by) \
         VALUES ($1,'previewed','{}','历史费用修正',1,1,2,2,0,0,-90,'[]',$2)",
    )
    .bind(run)
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("构造历史修正批次");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny \
         (run_id,event_id,delta_micro_cny,engine_version,reason,created_by) \
         VALUES ($1,'chain-initial',-80,1,'历史修正',$2),($1,'chain-success',-10,1,'历史修正',$2)",
    )
    .bind(run)
    .bind(w.admin.id)
    .execute(w.cp.db().pool())
    .await
    .expect("构造两笔既存修正");
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let reply = get(&w.cp, &format!("{path}?group_requests=true"), cookie).await;
        assert_eq!(ids(items(&reply)), ["chain-success"]);
        assert_eq!(
            group(&items(&reply)[0]).get("adjusted_micro_cny"),
            Some(&json!("-90"))
        );
        assert_eq!(
            group(&items(&reply)[0]).get("charge_micro_cny"),
            Some(&json!("1270"))
        );
    }
}

#[tokio::test]
async fn reused_request_ids_do_not_merge_different_traces_gateways_or_legacy_events() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    let mut legacy_one = attempt(&w, model, "legacy-one", "reused", json!({}));
    let mut legacy_two = attempt(&w, model, "legacy-two", "reused", json!({}));
    for legacy in [&mut legacy_one, &mut legacy_two] {
        legacy.as_object_mut().expect("历史事件").remove("trace_id");
    }
    send(
        &w,
        vec![
            attempt(&w, model, "first-trace", "reused", json!({})),
            attempt(
                &w,
                model,
                "another-trace",
                "reused",
                json!({"trace_id":"abcdef1234567890abcdef1234567890"}),
            ),
            legacy_one,
            legacy_two,
        ],
    )
    .await;
    let other_dp =
        w.cp.issue_dp_client("different-request-group-gateway")
            .await;
    let delivered = deliver(
        &other_dp,
        Uuid::new_v4(),
        vec![attempt(&w, model, "another-gateway", "reused", json!({}))],
    )
    .await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let reply = get(
            &w.cp,
            &format!("{path}?group_requests=true&request_id=reused"),
            cookie,
        )
        .await;
        assert_eq!(
            items(&reply).len(),
            5,
            "调用方可复用request_id，不能只按其合并"
        );
        for call in items(&reply) {
            assert_counts(call, 1, 0, 0, true);
        }
    }
}

#[tokio::test]
async fn grouping_and_anchor_details_keep_the_tenant_and_member_scope() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let own_key = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let foreign_tenant = seed_tenant(&w.cp, &w.admin).await;
    let foreign_member =
        w.cp.member_login_seeded(foreign_tenant, MemberRole::Owner)
            .await;
    let foreign_key = seed_key(&w.cp, &w.admin, foreign_tenant, Some(foreign_member.id)).await;
    send(&w, vec![
        attempt(&w, model, "same-tenant-other-member", "shared-identifiers", json!({})),
        attempt(&w, model, "own-member", "shared-identifiers", json!({"api_key_id":own_key,"user_id":member.id})),
        attempt(&w, model, "foreign-tenant", "shared-identifiers", json!({"api_key_id":foreign_key,"user_id":foreign_member.id,"team_id":foreign_tenant})),
    ]).await;
    let own = get(
        &w.cp,
        "/api/usage/calls?group_requests=true",
        member.cookie(),
    )
    .await;
    assert_eq!(ids(items(&own)), ["own-member"]);
    let tenant = get(&w.cp, "/api/usage/calls?group_requests=true", &owner).await;
    assert_eq!(items(&tenant).len(), 2);
    for call in items(&tenant) {
        assert_counts(call, 1, 0, 0, true);
    }
    for (cookie, forbidden) in [
        (member.cookie(), "same-tenant-other-member"),
        (owner.as_str(), "foreign-tenant"),
        (foreign_member.cookie(), "own-member"),
    ] {
        let reply = get(
            &w.cp,
            &format!(
                "/api/usage/calls?group_requests=true&include_attempts=true&event_id={forbidden}"
            ),
            cookie,
        )
        .await;
        assert!(
            items(&reply).is_empty(),
            "越权anchor不能加载同组或其他租户详情"
        );
    }
    let admin = get(
        &w.cp,
        "/admin/pricing/charges?group_requests=true",
        w.admin.cookie(),
    )
    .await;
    assert_eq!(items(&admin).len(), 3, "授权边界也属于分组键");
}

#[tokio::test]
async fn a_late_superseded_attempt_does_not_move_a_completed_request_between_pages() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    send(
        &w,
        vec![attempt(&w, model, "page-oldest", "page-oldest", json!({}))],
    )
    .await;
    send(
        &w,
        vec![attempt(
            &w,
            model,
            "page-middle-final",
            "page-middle",
            json!({"attempt_index":1,"attempt_kind":"fallback"}),
        )],
    )
    .await;
    send(
        &w,
        vec![attempt(&w, model, "page-newest", "page-newest", json!({}))],
    )
    .await;
    let mut cursors = Vec::new();
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let response = get(
            &w.cp,
            &format!("{path}?group_requests=true&limit=1"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&response)), ["page-newest"]);
        cursors.push(response.body["next_before"].as_i64().expect("下一请求游标"));
    }
    send(
        &w,
        vec![attempt(
            &w,
            model,
            "page-middle-late-failure",
            "page-middle",
            json!({"is_final_attempt":false,"status_code":503,"error_class":"upstream_status"}),
        )],
    )
    .await;
    for ((path, cookie), cursor) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ]
    .into_iter()
    .zip(cursors)
    {
        let response = get(
            &w.cp,
            &format!("{path}?group_requests=true&limit=1&before={cursor}"),
            cookie,
        )
        .await;
        assert_eq!(
            ids(items(&response)),
            ["page-middle-final"],
            "旧尝试晚到不能使已完成请求在跨页时消失"
        );
        assert_counts(&items(&response)[0], 2, 1, 0, true);
        let cursor = response.body["next_before"].as_i64().expect("最早请求游标");
        let last = get(
            &w.cp,
            &format!("{path}?group_requests=true&limit=1&before={cursor}"),
            cookie,
        )
        .await;
        assert_eq!(ids(items(&last)), ["page-oldest"]);
        assert_eq!(last.body.get("next_before"), Some(&Value::Null));
    }
}

#[tokio::test]
async fn an_invalid_all_zero_trace_never_merges_reused_request_ids() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    send(
        &w,
        vec![
            attempt(
                &w,
                model,
                "zero-trace-first",
                "reused-zero-trace",
                json!({"trace_id":"00000000000000000000000000000000"}),
            ),
            attempt(
                &w,
                model,
                "zero-trace-second",
                "reused-zero-trace",
                json!({"trace_id":"00000000000000000000000000000000"}),
            ),
        ],
    )
    .await;
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let response = get(
            &w.cp,
            &format!("{path}?group_requests=true&request_id=reused-zero-trace"),
            cookie,
        )
        .await;
        assert_eq!(
            items(&response).len(),
            2,
            "全零不是有效追踪身份，不可误合并请求"
        );
        for row in items(&response) {
            assert_counts(row, 1, 0, 0, true);
        }
    }
}

#[tokio::test]
async fn public_error_candidates_preserve_masked_unknown_and_known_group_results() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    for (id, class, message) in [
        ("mapped-not-found", "model_not_found", "unknown model"),
        (
            "masked-not-found",
            "permission_denied",
            "API key is not allowed to use model internal-hidden-model",
        ),
        (
            "ordinary-denied",
            "permission_denied",
            "private authorization detail",
        ),
        (
            "unknown-error",
            "future_upstream_error",
            "private future detail",
        ),
        (
            "filtered-public",
            "content_filter",
            "private screening detail",
        ),
    ] {
        send(
            &w,
            vec![attempt(
                &w,
                model,
                id,
                id,
                json!({"status_code":403,"error_class":class,"error_message":message}),
            )],
        )
        .await;
    }
    for (id, value) in [
        ("absent-error", None),
        ("null-error", Some(Value::Null)),
        ("empty-error", Some(json!(""))),
        ("nonstring-error", Some(json!(503))),
        ("raw-public", Some(json!("content_filtered"))),
    ] {
        let mut event = attempt(&w, model, id, id, json!({"status_code":502}));
        match value {
            Some(value) => event["error_class"] = value,
            None => {
                event
                    .as_object_mut()
                    .expect("事件对象")
                    .remove("error_class");
            }
        }
        send(&w, vec![event]).await;
    }
    for (error, expected) in [
        (
            "model_not_found",
            vec!["mapped-not-found", "masked-not-found"],
        ),
        (
            "failed",
            vec![
                "absent-error",
                "empty-error",
                "nonstring-error",
                "null-error",
                "ordinary-denied",
                "raw-public",
                "unknown-error",
            ],
        ),
        ("content_filtered", vec!["filtered-public"]),
    ] {
        for grouped in [false, true] {
            let response = get(
                &w.cp,
                &format!("/api/usage/calls?group_requests={grouped}&error_class={error}"),
                &owner,
            )
            .await;
            let mut actual = ids(items(&response));
            actual.sort();
            assert_eq!(actual, expected, "公开错误映射的索引候选不能改变结果");
            assert!(!response.body.to_string().contains("private"));
            assert!(!response.body.to_string().contains("internal-hidden-model"));
        }
    }
}
