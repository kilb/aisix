//! 调用结果筛选在数据库分页前执行，租户只使用公开失败分类。
mod support;

use aisix_control_plane::auth::MemberRole;
use serde_json::{json, Value};
use support::world::{deliver, price, seed_model, usage, world, World};
use support::{Resp, TestCp};
use uuid::Uuid;

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
    .expect("grant fixture model");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    Some((w, model, owner.cookie().to_owned()))
}

fn row(w: &World, model: Uuid, id: &str, fields: Value) -> Value {
    let mut event = usage(w, id, model, fields);
    if event.get("is_final_attempt").is_none() {
        event["is_final_attempt"] = json!(true);
    }
    event
}

async fn send(w: &World, event: Value) {
    let r = deliver(&w.dp, Uuid::new_v4(), vec![event]).await;
    assert_eq!(r.status, 200, "{r:?}");
}

async fn get(cp: &TestCp, path: &str, cookie: &str) -> Resp {
    cp.send("GET", path, Some(cookie), None).await
}

fn ids(r: &Resp) -> Vec<String> {
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|r| r["event_id"].as_str().expect("event identity").to_owned())
        .collect()
}

#[tokio::test]
async fn call_filter_outcomes_precede_limit_and_keep_cursor_scope() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    for (id, status, error) in [
        ("old-failure", 503, "upstream_status"),
        ("old-stream-error", 200, "stream_aborted"),
        ("new-success-one", 200, ""),
        ("new-success-two", 201, ""),
    ] {
        send(
            &w,
            row(
                &w,
                model,
                id,
                json!({"status_code":status,"error_class":error}),
            ),
        )
        .await;
    }
    for (path, cookie) in [
        ("/api/usage/calls", owner.as_str()),
        ("/admin/pricing/charges", w.admin.cookie()),
    ] {
        let first = get(&w.cp, &format!("{path}?outcome=failed&limit=1"), cookie).await;
        assert_eq!(ids(&first), vec!["old-stream-error"]);
        let before = first
            .body
            .get("next_before")
            .and_then(Value::as_i64)
            .expect("another matching page");
        let second = get(
            &w.cp,
            &format!("{path}?outcome=failed&limit=1&before={before}"),
            cookie,
        )
        .await;
        assert_eq!(ids(&second), vec!["old-failure"]);
        assert_eq!(second.body.get("next_before"), Some(&Value::Null));
        let success = get(&w.cp, &format!("{path}?outcome=succeeded"), cookie).await;
        assert_eq!(ids(&success), vec!["new-success-two", "new-success-one"]);
    }
}

#[tokio::test]
async fn call_filter_combinations_use_reported_cache_tier_and_operation() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    for (id, cache, tier, operation) in [
        ("match-one", "hit", "default", "responses"),
        ("miss", "miss", "default", "responses"),
        ("wrong-tier", "hit", "priority", "responses"),
        ("wrong-operation", "hit", "default", "chat"),
        ("unknown-cache", "", "default", "responses"),
        ("match-two", "hit", "default", "responses"),
    ] {
        send(&w,row(&w,model,id,json!({"status_code":200,"cache_status":cache,"service_tier":tier,"operation":operation}))).await;
    }
    for (path, cookie) in [
        ("/api/usage/calls", owner.as_str()),
        ("/admin/pricing/charges", w.admin.cookie()),
    ] {
        let r=get(&w.cp,&format!("{path}?outcome=succeeded&cache_hit=true&service_tier=default&operation=responses&final_attempt=true"),cookie).await;
        assert_eq!(ids(&r), vec!["match-two", "match-one"]);
        let r = get(
            &w.cp,
            &format!("{path}?cache_hit=false&operation=responses"),
            cookie,
        )
        .await;
        assert_eq!(ids(&r), vec!["miss"]);
    }
}

#[tokio::test]
async fn call_filter_error_vocabulary_hides_tenant_upstream_details() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    send(&w,row(&w,model,"upstream-private",json!({"status_code":529,"error_class":"upstream_status","error_message":"private upstream detail"}))).await;
    send(
        &w,
        row(
            &w,
            model,
            "public-invalid",
            json!({"status_code":400,"error_class":"invalid_request_error"}),
        ),
    )
    .await;
    let r = get(
        &w.cp,
        "/api/usage/calls?outcome=failed&error_class=upstream_unavailable",
        &owner,
    )
    .await;
    assert_eq!(ids(&r), vec!["upstream-private"]);
    assert!(!r.body.to_string().contains("private upstream detail"));
    assert_eq!(r.body["items"][0].get("status_code"), Some(&Value::Null));
    let raw = get(
        &w.cp,
        "/api/usage/calls?error_class=upstream_status",
        &owner,
    )
    .await;
    assert_eq!(
        raw.status, 422,
        "raw upstream class is not a tenant selector: {raw:?}"
    );
    let status = get(&w.cp, "/api/usage/calls?http_status=529", &owner).await;
    assert_eq!(
        status.status, 422,
        "private status cannot be probed: {status:?}"
    );
    let admin = get(
        &w.cp,
        "/admin/pricing/charges?outcome=failed&http_status=529&error_class=upstream_status",
        w.admin.cookie(),
    )
    .await;
    assert_eq!(ids(&admin), vec!["upstream-private"]);
    send(
        &w,
        row(
            &w,
            model,
            "private-stream-status",
            json!({"status_code":200,"error_class":"stream_aborted"}),
        ),
    )
    .await;
    let r = get(&w.cp, "/api/usage/calls?http_status=200", &owner).await;
    assert!(
        ids(&r).is_empty(),
        "tenant status filters only match status codes exposed by successful rows"
    );
}

#[tokio::test]
async fn call_filter_validation_is_explicit_and_members_remain_scoped() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    let own = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let key = support::world::seed_key(&w.cp, &w.admin, w.tenant, Some(own.id)).await;
    send(
        &w,
        row(
            &w,
            model,
            "other-members-failure",
            json!({"status_code":503}),
        ),
    )
    .await;
    send(
        &w,
        row(
            &w,
            model,
            "own-failure",
            json!({"status_code":503,"api_key_id":key,"user_id":own.id}),
        ),
    )
    .await;
    let r = get(&w.cp, "/api/usage/calls?outcome=failed", own.cookie()).await;
    assert_eq!(ids(&r), vec!["own-failure"]);
    let r = get(
        &w.cp,
        &format!("/api/usage/calls?member_id={}", w.member),
        own.cookie(),
    )
    .await;
    assert!(ids(&r).is_empty(), "a member cannot widen their scope");
    let r = get(
        &w.cp,
        &format!("/api/usage/calls?member_id={}", own.id),
        &owner,
    )
    .await;
    assert_eq!(ids(&r), vec!["own-failure"]);
    for filter in [
        "outcome=not-real",
        "http_status=99",
        "cache_hit=perhaps",
        "final_attempt=perhaps",
        "from=2026-11-12T02%3A00%3A00Z&to=2026-11-12T01%3A00%3A00Z",
    ] {
        for (path, cookie) in [
            ("/api/usage/calls", owner.as_str()),
            ("/admin/pricing/charges", w.admin.cookie()),
        ] {
            let r = get(&w.cp, &format!("{path}?{filter}"), cookie).await;
            assert_eq!(r.status, 422, "{path} {filter}: {r:?}");
        }
    }
}

#[tokio::test]
async fn call_filter_unknown_is_distinct_from_failure_and_retry() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    for (id, fields) in [
        ("known-success", json!({"status_code": 200})),
        ("known-failure", json!({"status_code": 503})),
        ("unknown-null", json!({"status_code": null})),
        ("unknown-string", json!({"status_code": "503"})),
        (
            "unknown-huge",
            json!({"status_code": 9999999999999999999_u64}),
        ),
        ("unknown-final", json!({"is_final_attempt": null})),
        (
            "known-retry",
            json!({"status_code": 503, "is_final_attempt": false}),
        ),
    ] {
        send(&w, row(&w, model, id, fields)).await;
    }
    for (path, cookie) in [
        ("/api/usage/calls", owner.as_str()),
        ("/admin/pricing/charges", w.admin.cookie()),
    ] {
        let r = get(&w.cp, &format!("{path}?outcome=succeeded"), cookie).await;
        assert_eq!(ids(&r), vec!["known-success"]);
        let r = get(&w.cp, &format!("{path}?outcome=failed"), cookie).await;
        assert_eq!(ids(&r), vec!["known-failure"]);
        let r = get(
            &w.cp,
            &format!("{path}?outcome=unknown&final_attempt=true"),
            cookie,
        )
        .await;
        assert_eq!(
            ids(&r),
            vec!["unknown-huge", "unknown-string", "unknown-null"]
        );
        if path == "/api/usage/calls" {
            for call in r.body["items"].as_array().expect("items") {
                assert_eq!(call.get("status"), Some(&json!("unknown")), "{call}");
                assert_eq!(call.get("status_code"), Some(&Value::Null), "{call}");
                assert_eq!(call.get("error_class"), Some(&Value::Null), "{call}");
            }
        }
    }
    let r = get(
        &w.cp,
        "/admin/pricing/charges?outcome=retried&final_attempt=false",
        w.admin.cookie(),
    )
    .await;
    assert_eq!(ids(&r), vec!["known-retry"]);
    let r = get(
        &w.cp,
        "/admin/pricing/charges?outcome=unknown",
        w.admin.cookie(),
    )
    .await;
    assert_eq!(
        ids(&r),
        vec![
            "unknown-final",
            "unknown-huge",
            "unknown-string",
            "unknown-null"
        ]
    );
}

#[tokio::test]
async fn tenant_unknown_projection_preserves_missing_final_and_invalid_http_status() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    for (id, fields, removed) in [
        ("unknown-missing-status", json!({}), Some("status_code")),
        ("unknown-low-status", json!({"status_code":99}), None),
        ("unknown-high-status", json!({"status_code":600}), None),
        (
            "unknown-fractional-status",
            json!({"status_code":200.5}),
            None,
        ),
        (
            "unknown-missing-final",
            json!({"prompt_tokens":1000000}),
            Some("is_final_attempt"),
        ),
        (
            "unknown-null-final",
            json!({"prompt_tokens":1000000,"is_final_attempt":null}),
            None,
        ),
        (
            "unknown-string-final",
            json!({"prompt_tokens":1000000,"is_final_attempt":"false"}),
            None,
        ),
    ] {
        let mut event = row(&w, model, id, fields);
        if let Some(field) = removed {
            event.as_object_mut().expect("event object").remove(field);
        }
        send(&w, event).await;
    }
    let result = get(&w.cp, "/api/usage/calls?outcome=unknown", &owner).await;
    assert_eq!(
        ids(&result),
        vec![
            "unknown-string-final",
            "unknown-null-final",
            "unknown-missing-final",
            "unknown-fractional-status",
            "unknown-high-status",
            "unknown-low-status",
            "unknown-missing-status",
        ]
    );
    for call in result.body["items"].as_array().expect("items") {
        assert_eq!(call.get("status"), Some(&json!("unknown")), "{call}");
        assert_eq!(call.get("status_code"), Some(&Value::Null), "{call}");
        assert_eq!(call.get("error_class"), Some(&Value::Null), "{call}");
    }
    for outcome in ["succeeded", "failed", "retried"] {
        let result = get(
            &w.cp,
            &format!("/api/usage/calls?outcome={outcome}"),
            &owner,
        )
        .await;
        assert!(ids(&result).is_empty(), "{outcome}: {:?}", result.body);
    }
}

#[tokio::test]
async fn call_filter_admin_time_member_account_and_request_are_intersected() {
    let Some((w, model, _owner)) = setup().await else {
        return;
    };
    let account = support::world::seed_provider_key(&w.cp, &w.admin).await;
    let other_account = support::world::seed_provider_key(&w.cp, &w.admin).await;
    for (id, at, pk, request) in [
        (
            "before-range",
            "2026-11-10T00:00:00Z",
            account,
            "target-request",
        ),
        (
            "target-row",
            "2026-11-11T00:00:00Z",
            account,
            "target-request",
        ),
        (
            "wrong-account",
            "2026-11-11T00:00:00Z",
            other_account,
            "target-request",
        ),
        (
            "wrong-request",
            "2026-11-11T00:00:00Z",
            account,
            "other-request",
        ),
        (
            "at-range-end",
            "2026-11-12T00:00:00Z",
            account,
            "target-request",
        ),
    ] {
        send(
            &w,
            row(
                &w,
                model,
                id,
                json!({"occurred_at":at,"provider_key_id":pk,"request_id":request}),
            ),
        )
        .await;
    }
    let path = format!(
        "/admin/pricing/charges?from=2026-11-11T00%3A00%3A00Z&to=2026-11-12T00%3A00%3A00Z&member_id={}&provider_key_id={account}&request_id=target-request&limit=1",
        w.member
    );
    let r = get(&w.cp, &path, w.admin.cookie()).await;
    assert_eq!(ids(&r), vec!["target-row"]);
    assert_eq!(r.body.get("next_before"), Some(&Value::Null));
}

#[tokio::test]
async fn accepted_background_work_is_unknown_until_terminal_and_never_a_retry() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    for (id, fields) in [
        (
            "background-accepted",
            json!({"operation":"responses","inference_stage":"accepted","is_final_attempt":false}),
        ),
        (
            "background-pending",
            json!({"operation":"responses","inference_stage":"pending","is_final_attempt":false}),
        ),
        (
            "actual-retry",
            json!({"operation":"responses","inference_stage":"attempt","is_final_attempt":false,"status_code":503}),
        ),
        (
            "legacy-retry",
            json!({"operation":"responses","is_final_attempt":false,"status_code":503}),
        ),
        (
            "management-success",
            json!({"operation":"responses","inference_stage":"management","is_final_attempt":true}),
        ),
        (
            "management-failure",
            json!({"operation":"responses","inference_stage":"management","is_final_attempt":true,"status_code":404,"error_class":"invalid_request_error"}),
        ),
        (
            "background-terminal",
            json!({"operation":"responses","inference_stage":"terminal","is_final_attempt":true}),
        ),
    ] {
        send(&w, row(&w, model, id, fields)).await;
    }
    let unknown = get(
        &w.cp,
        "/admin/pricing/charges?outcome=unknown&final_attempt=false&limit=1",
        w.admin.cookie(),
    )
    .await;
    assert_eq!(ids(&unknown), vec!["background-pending"]);
    let before = unknown.body["next_before"]
        .as_i64()
        .expect("另一个受理记录分页");
    let accepted = get(
        &w.cp,
        &format!(
            "/admin/pricing/charges?outcome=unknown&final_attempt=false&limit=1&before={before}"
        ),
        w.admin.cookie(),
    )
    .await;
    assert_eq!(ids(&accepted), vec!["background-accepted"]);
    assert_eq!(accepted.body.get("next_before"), Some(&Value::Null));
    let retried = get(
        &w.cp,
        "/admin/pricing/charges?outcome=retried&final_attempt=false",
        w.admin.cookie(),
    )
    .await;
    assert_eq!(ids(&retried), vec!["legacy-retry", "actual-retry"]);
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.as_str()),
    ] {
        let succeeded = get(&w.cp, &format!("{path}?outcome=succeeded"), cookie).await;
        assert_eq!(
            ids(&succeeded),
            vec!["background-terminal", "management-success"]
        );
        let failed = get(&w.cp, &format!("{path}?outcome=failed"), cookie).await;
        assert_eq!(ids(&failed), vec!["management-failure"]);
    }
    let tenant_unknown = get(&w.cp, "/api/usage/calls?outcome=unknown", &owner).await;
    assert!(
        ids(&tenant_unknown).is_empty(),
        "非终态且未收费的受理记录保持不可见"
    );
}
