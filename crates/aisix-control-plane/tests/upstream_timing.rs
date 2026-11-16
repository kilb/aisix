//! 同一次上游尝试的可选时间点通过正式接收与真实查询保留，不参与计费或补值。
mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use support::world::{deliver, price, seed_key, seed_model, tokens_680, usage, world};
use uuid::Uuid;

const TIMING_FIELDS: [&str; 6] = [
    "http_send_started_ms",
    "response_headers_ms",
    "first_response_byte_ms",
    "first_protocol_frame_ms",
    "first_canonical_chunk_ms",
    "first_content_ms",
];

fn item<'a>(page: &'a Value, event_id: &str) -> &'a Value {
    page["items"]
        .as_array()
        .expect("调用列表")
        .iter()
        .find(|row| row["event_id"] == event_id)
        .unwrap_or_else(|| panic!("缺少事件 {event_id}: {page}"))
}

#[tokio::test]
async fn measured_zero_partial_and_unknown_offsets_survive_intake_and_scoped_call_reads() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let key = seed_key(&w.cp, &w.admin, w.tenant, Some(member.id)).await;
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let full = json!({
        "http_send_started_ms":13,"response_headers_ms":17,"first_response_byte_ms":19,
        "first_protocol_frame_ms":23,"first_canonical_chunk_ms":29,"first_content_ms":31
    });
    let cases = [
        ("timing-full", Some(full)),
        (
            "timing-zero",
            Some(json!({
                "http_send_started_ms":0,"response_headers_ms":0,"first_response_byte_ms":0,
                "first_protocol_frame_ms":0,"first_canonical_chunk_ms":0,"first_content_ms":0
            })),
        ),
        (
            "timing-partial",
            Some(json!({
                "http_send_started_ms":0,"response_headers_ms":50,"first_response_byte_ms":62,
                "first_protocol_frame_ms":null,"first_canonical_chunk_ms":75
            })),
        ),
        ("timing-null", Some(Value::Null)),
        ("timing-absent", None),
    ];
    let events: Vec<Value> = cases
        .iter()
        .map(|(id, timing)| {
            let mut fields = tokens_680();
            fields["api_key_id"] = json!(key);
            fields["user_id"] = json!(member.id);
            fields["is_final_attempt"] = json!(true);
            // 旧计数与新时间点不相等，不能从旧 TTFT 或总耗时反推新字段。
            fields["first_token_latency_ms"] = json!(1000);
            fields["upstream_ttft_ms"] = json!(900);
            fields["upstream_latency_ms"] = json!(2000);
            if let Some(timing) = timing {
                fields["upstream_timing"] = timing.clone();
            }
            usage(&w, id, model, fields)
        })
        .collect();
    let accepted = deliver(&w.dp, Uuid::new_v4(), events.clone()).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":5,"rejected":0,"duplicates":0})
    );

    for (path, cookie) in [
        (
            format!("/admin/pricing/charges?tenant_id={}", w.tenant),
            viewer.cookie(),
        ),
        ("/api/usage/calls".to_owned(), owner.cookie()),
        ("/api/usage/calls".to_owned(), member.cookie()),
    ] {
        let response = w.cp.get_with_cookie(&path, cookie).await;
        assert_eq!(response.status, 200, "{path}: {response:?}");
        assert_eq!(
            response.body["items"].as_array().expect("调用列表").len(),
            cases.len()
        );
        for (id, timing) in &cases {
            let row = item(&response.body, id);
            let projected = row
                .get("upstream_timing")
                .expect("调用契约明确返回可选时间线");
            if let Some(timing) = timing.as_ref().filter(|value| !value.is_null()) {
                let projected = projected.as_object().expect("已测时间线");
                for field in TIMING_FIELDS {
                    match timing.get(field).filter(|value| !value.is_null()) {
                        Some(expected) => assert_eq!(
                            projected.get(field),
                            Some(expected),
                            "真实零也须存在：{field}: {row}"
                        ),
                        None => assert!(
                            projected.get(field).is_none_or(Value::is_null),
                            "未知时间点不能填零：{field}: {row}"
                        ),
                    }
                }
            } else {
                assert_eq!(
                    projected,
                    &Value::Null,
                    "旧记录不能从 TTFT 补出时间线：{row}"
                );
            }
            let amount = row
                .get("charge_micro_cny")
                .or_else(|| row["charge"].get("amount_micro_cny"));
            assert_eq!(amount, Some(&json!(680)), "时间线不能改变实际计价：{row}");
        }
    }
    for ((id, timing), original) in cases.iter().zip(events.iter()) {
        let response =
            w.cp.get_with_cookie(&format!("/admin/usage/events/{id}"), viewer.cookie())
                .await;
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.body["event"].get("upstream_timing"),
            timing.as_ref(),
            "管理员原始详情保持已存字段的缺失与 null"
        );
        let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id=$1")
            .bind(id)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("已存原事件");
        assert_eq!(&stored, original, "读取投影不得改写事实");
    }
    let postings: Vec<i64> = sqlx::query_scalar(
        "SELECT amount_micro_cny FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'",
    )
    .bind(w.tenant)
    .fetch_all(w.cp.db().pool())
    .await
    .expect("真实用量扣费");
    assert_eq!(postings.len(), cases.len());
    assert!(
        postings.iter().all(|amount| *amount == -680),
        "时间线不改变账本金额"
    );

    let unrelated_member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let other_tenant = w.cp.seed_tenant(TenantStatus::Active).await;
    let unrelated_owner =
        w.cp.member_login_seeded(other_tenant, MemberRole::Owner)
            .await;
    for cookie in [unrelated_member.cookie(), unrelated_owner.cookie()] {
        let response = w.cp.get_with_cookie("/api/usage/calls", cookie).await;
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.body["items"],
            json!([]),
            "时间线不得扩大成员或租户范围"
        );
    }
    let refused =
        w.cp.get_with_cookie("/admin/usage/events/timing-full", owner.cookie())
            .await;
    assert_eq!(
        refused.status, 401,
        "租户会话不能读取管理员原记录：{refused:?}"
    );
}

#[tokio::test]
async fn invalid_timing_never_refuses_charges_and_unknown_keys_do_not_reach_tenants() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let cases = [
        (
            "timing-order",
            json!({"http_send_started_ms":50,"response_headers_ms":49}),
        ),
        ("timing-negative", json!({"http_send_started_ms":-1})),
        ("timing-text", json!({"http_send_started_ms":"0"})),
        (
            "timing-overflow",
            json!({"http_send_started_ms":4294967296u64}),
        ),
        (
            "timing-future",
            json!({"http_send_started_ms":0,"first_content_ms":10,"internal_unpublished_name":"timing-private-fixture"}),
        ),
    ];
    let events = cases
        .iter()
        .map(|(id, timing)| {
            let mut fields = tokens_680();
            fields["is_final_attempt"] = json!(true);
            fields["upstream_timing"] = timing.clone();
            usage(&w, id, model, fields)
        })
        .collect();
    let accepted = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":5,"rejected":0,"duplicates":0})
    );
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.cookie()),
    ] {
        let response = w.cp.get_with_cookie(path, cookie).await;
        assert_eq!(response.status, 200, "{response:?}");
        assert!(
            !response.body.to_string().contains("timing-private-fixture"),
            "可选时间线只投影六个公开时间点"
        );
        for (id, _) in &cases {
            let row = item(&response.body, id);
            let expected = if *id == "timing-future" {
                json!({"http_send_started_ms":0,"first_content_ms":10})
            } else {
                Value::Null
            };
            assert_eq!(
                row.get("upstream_timing"),
                Some(&expected),
                "非法时间线只降为未知：{row}"
            );
            let amount = row
                .get("charge_micro_cny")
                .or_else(|| row["charge"].get("amount_micro_cny"));
            assert_eq!(amount, Some(&json!(680)), "非法观测不能使原收费丢失：{row}");
        }
    }
}

#[tokio::test]
async fn the_public_dp_usage_event_wire_reaches_both_call_views_with_optional_offsets() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    // 固定偏移仅验证正式 DP 事件的线协议，不把合约夹具称为实际时钟观测。
    let event = aisix_obs::UsageEvent {
        request_id: "timing-wire-request".to_owned(),
        event_id: "timing-wire-event".to_owned(),
        occurred_at: aisix_obs::UsageEvent::occurred_at_now(),
        requested_model_id: model.to_string(),
        model_id: model.to_string(),
        api_key_id: w.key.clone(),
        team_id: w.tenant.to_string(),
        user_id: w.member.to_string(),
        operation: "chat".to_owned(),
        status_code: 200,
        is_final_attempt: true,
        prompt_tokens: 1000,
        cached_prompt_tokens: 800,
        completion_tokens: 200,
        upstream_timing: Some(aisix_core::usage_io::UpstreamTiming {
            http_send_started_ms: Some(0),
            response_headers_ms: Some(3),
            first_response_byte_ms: Some(5),
            first_protocol_frame_ms: None,
            first_canonical_chunk_ms: Some(8),
            first_content_ms: Some(11),
        }),
        ..Default::default()
    };
    let expected = json!({
        "http_send_started_ms":0,"response_headers_ms":3,"first_response_byte_ms":5,
        "first_canonical_chunk_ms":8,"first_content_ms":11
    });
    let wire = serde_json::to_value(event).expect("正式 UsageEvent 序列化");
    assert_eq!(wire.get("upstream_timing"), Some(&expected));
    let accepted = deliver(&w.dp, Uuid::new_v4(), vec![wire]).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    for (path, cookie) in [
        ("/admin/pricing/charges", w.admin.cookie()),
        ("/api/usage/calls", owner.cookie()),
    ] {
        let response = w.cp.get_with_cookie(path, cookie).await;
        assert_eq!(response.status, 200, "{response:?}");
        let row = item(&response.body, "timing-wire-event");
        assert_eq!(row.get("upstream_timing"), Some(&expected));
        let amount = row
            .get("charge_micro_cny")
            .or_else(|| row["charge"].get("amount_micro_cny"));
        assert_eq!(amount, Some(&json!(680)));
    }
}
