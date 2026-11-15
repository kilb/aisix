//! 管理员按事件定位完整的已存调用记录，不重放请求或补造正文。
mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use serde_json::{json, Value};
use support::world::{deliver, price, seed_model, tokens_680, usage, world};
use uuid::Uuid;

fn detail_path(event_id: &str) -> String {
    let mut url =
        reqwest::Url::parse("http://localhost/admin/usage/events").expect("固定的管理接口地址");
    url.path_segments_mut().expect("分层地址").push(event_id);
    url.path().to_owned()
}

#[tokio::test]
async fn a_viewer_reads_one_complete_record_by_its_event_id_with_the_recorded_error() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let event_id = "detail-target/错误?#";
    let error_message = format!(
        "上游拒绝此请求：{}；请检查请求参数。",
        "详细错误。".repeat(300)
    );
    let io_audit = json!({
        "version":1,"boundary":"client_exchange","request_body_bytes":91,
        "response_body_bytes":47,"response_complete":true,
        "input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true}
    });
    let mut fields = tokens_680();
    for (key, value) in json!({
        "request_id":"detail-shared-request","requested_model":"public-model",
        "model_id":model,"upstream_provider":"openai","upstream_model":"served-model",
        "status_code":529,"error_class":"upstream_status","error_message":error_message,
        "is_final_attempt":true,"upstream_latency_ms":381,"downstream_latency_ms":402,
        "io_audit":io_audit,"future_field":{"nested":[1,false,{"detail":"preserved"}]}
    })
    .as_object()
    .expect("已记录字段")
    {
        fields[key] = value.clone();
    }
    let event = usage(&w, event_id, model, fields);
    let newer = usage(
        &w,
        "detail-newer",
        model,
        json!({"request_id":"detail-shared-request"}),
    );
    let free_failure = usage(
        &w,
        "detail-free-failure",
        model,
        json!({
            "status_code":502,"error_class":"upstream_status","error_message":"上游连接失败",
            "is_final_attempt":true
        }),
    );
    let batch_id = Uuid::new_v4();
    let accepted = deliver(&w.dp, batch_id, vec![event.clone(), newer, free_failure]).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":3,"rejected":0,"duplicates":0})
    );

    let detail =
        w.cp.get_with_cookie(&detail_path(event_id), viewer.cookie())
            .await;
    assert_eq!(detail.status, 200, "{detail:?}");
    assert_eq!(detail.body.get("event_id"), Some(&json!(event_id)));
    assert_eq!(
        detail.body.get("event"),
        Some(&event),
        "未报告字段不能被补造，未知字段不能丢失"
    );
    assert_eq!(detail.body.get("batch_id"), Some(&json!(batch_id)));
    assert_eq!(detail.body.get("team_id"), Some(&json!(w.tenant)));
    assert_eq!(detail.body.get("api_key_id"), Some(&json!(w.key)));
    assert_eq!(
        detail.body.get("request_id"),
        Some(&json!("detail-shared-request"))
    );
    let occurred_at = chrono::DateTime::parse_from_rfc3339(
        detail.body["occurred_at"].as_str().expect("发生时间"),
    )
    .expect("有效时间");
    let sent_at =
        chrono::DateTime::parse_from_rfc3339(event["occurred_at"].as_str().expect("原发生时间"))
            .expect("有效原时间");
    assert!(
        occurred_at
            .signed_duration_since(sent_at)
            .num_nanoseconds()
            .is_some_and(|difference| difference.abs() < 1000),
        "数据库时间精度以内保留原发生时间"
    );
    assert!(detail
        .body
        .get("seq")
        .and_then(Value::as_i64)
        .is_some_and(|seq| seq > 0));
    assert!(detail
        .body
        .get("dp_id")
        .and_then(Value::as_str)
        .is_some_and(|id| Uuid::parse_str(id).is_ok()));
    assert!(detail
        .body
        .get("received_at")
        .and_then(Value::as_str)
        .is_some_and(|time| chrono::DateTime::parse_from_rfc3339(time).is_ok()));

    let list =
        w.cp.get_with_cookie(
            "/admin/usage/events?request_id=detail-shared-request",
            viewer.cookie(),
        )
        .await;
    assert_eq!(list.status, 200, "{list:?}");
    let listed = list.body["items"]
        .as_array()
        .expect("事件列表")
        .iter()
        .find(|item| item["event_id"] == event_id)
        .expect("目标事件");
    assert_eq!(listed, &detail.body, "列表与详情使用同一记录契约");

    let mut charges_url =
        reqwest::Url::parse("http://localhost/admin/pricing/charges").expect("计费地址");
    charges_url
        .query_pairs_mut()
        .append_pair("event_id", event_id)
        .append_pair("limit", "1");
    let charges_path = format!(
        "{}?{}",
        charges_url.path(),
        charges_url.query().expect("查询条件")
    );
    let charges = w.cp.get_with_cookie(&charges_path, viewer.cookie()).await;
    assert_eq!(charges.status, 200, "{charges:?}");
    let rows = charges.body["items"].as_array().expect("计费记录");
    assert_eq!(rows.len(), 1, "按事件只读一笔费用");
    assert_eq!(rows[0].get("event_id"), Some(&json!(event_id)));
    assert_eq!(rows[0].get("charge_micro_cny"), Some(&json!(680)));
    assert_eq!(
        rows[0].get("error_message"),
        Some(&json!(error_message)),
        "具体错误不能被换成计价原因或截断"
    );
    assert_eq!(rows[0].get("error_class"), Some(&json!("upstream_status")));

    let all =
        w.cp.get_with_cookie("/admin/pricing/charges", viewer.cookie())
            .await;
    assert_eq!(all.status, 200, "{all:?}");
    let free = all.body["items"]
        .as_array()
        .expect("全部调用")
        .iter()
        .find(|row| row["event_id"] == "detail-free-failure")
        .expect("零费用失败同样是调用记录");
    assert_eq!(free.get("charge_micro_cny"), Some(&json!(0)));
    assert_eq!(free.get("status_code"), Some(&json!(502)));
    assert_eq!(free.get("error_message"), Some(&json!("上游连接失败")));
    let no_error = all.body["items"]
        .as_array()
        .expect("全部调用")
        .iter()
        .find(|row| row["event_id"] == "detail-newer")
        .expect("未采集错误的调用");
    assert_eq!(no_error.get("error_message"), Some(&Value::Null));
}

#[tokio::test]
async fn admin_record_reads_redact_credentials_without_rewriting_the_stored_event() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let secret = "sk-pricing-upstream-7c1";
    let bearer = "operator-detail-bearer-secret-7c1-123456";
    let message =
        format!("请求被拒绝：{secret}，Bearer {bearer}，https://user:password@public.example/path");
    let event = usage(
        &w,
        "detail-secrets",
        model,
        json!({
            "error_class":"upstream_status","error_message":message,"status_code":401,
            "request_headers":{"Authorization":format!("Basic {bearer}"),"x-api-key":"opaque-upstream-secret"},
            "future_field":{"api_key":"opaque-future-secret","api_key_id":"safe-key-id","entries":[{"client_secret":"opaque-client-secret","note":secret}],"url":"redis://redis-user:redis-password@public.example/0?token=redis-query-secret"}
        }),
    );
    let accepted = deliver(&w.dp, Uuid::new_v4(), vec![event.clone()]).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;

    for path in [
        detail_path("detail-secrets"),
        "/admin/usage/events?request_id=req-detail-secrets".to_owned(),
        "/admin/pricing/charges?event_id=detail-secrets".to_owned(),
    ] {
        let response = w.cp.get_with_cookie(&path, viewer.cookie()).await;
        assert_eq!(response.status, 200, "{path}: {response:?}");
        let raw = response.body.to_string();
        for value in [
            secret,
            bearer,
            "password",
            "opaque-upstream-secret",
            "opaque-future-secret",
            "opaque-client-secret",
            "redis-query-secret",
        ] {
            assert!(!raw.contains(value), "管理记录不能返回凭据 {value}：{raw}");
        }
        assert!(raw.contains("请求被拒绝"), "脱敏应保留可诊断的错误：{raw}");
    }
    let detail =
        w.cp.get_with_cookie(&detail_path("detail-secrets"), viewer.cookie())
            .await;
    assert_eq!(
        detail.body.pointer("/event/future_field/api_key_id"),
        Some(&json!("safe-key-id"))
    );
    let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id=$1")
        .bind("detail-secrets")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("已存原记录");
    assert_eq!(stored, event, "只对显示副本脱敏，不修改用量接收与计价依据");
}

#[tokio::test]
async fn unknown_records_are_404_and_tenant_sessions_cannot_read_admin_details() {
    let Some(w) = world().await else { return };
    let accepted = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![json!({
            "event_id":"detail-legacy","occurred_at":"2026-11-06T08:15:30Z"
        })],
    )
    .await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let legacy =
        w.cp.get_with_cookie(&detail_path("detail-legacy"), viewer.cookie())
            .await;
    assert_eq!(legacy.status, 200, "{legacy:?}");
    assert_eq!(
        legacy.body.get("event"),
        Some(&json!({
            "event_id":"detail-legacy","occurred_at":"2026-11-06T08:15:30Z"
        }))
    );
    for field in [
        "error_message",
        "request_body",
        "response_body",
        "prompt_tokens",
        "io_audit",
    ] {
        assert!(
            legacy.body["event"].get(field).is_none(),
            "未采集字段保持缺失：{field}"
        );
    }
    for event_id in ["detail-absent", "detail-nul\0event"] {
        let absent =
            w.cp.get_with_cookie(&detail_path(event_id), viewer.cookie())
                .await;
        assert_eq!(absent.status, 404, "{absent:?}");
        assert_eq!(absent.body.get("code"), Some(&json!("not_found")));
    }

    let anonymous =
        w.cp.send("GET", &detail_path("detail-legacy"), None, None)
            .await;
    assert_eq!(anonymous.status, 401, "{anonymous:?}");
    for role in [
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Billing,
        MemberRole::Member,
    ] {
        let member = w.cp.member_login_seeded(w.tenant, role).await;
        let refused =
            w.cp.get_with_cookie(&detail_path("detail-legacy"), member.cookie())
                .await;
        assert_eq!(
            refused.status, 401,
            "租户 {role:?} 不持有管理员会话：{refused:?}"
        );
        assert_eq!(refused.body.get("code"), Some(&json!("unauthenticated")));
    }
}

#[tokio::test]
async fn internal_https_and_wss_urls_expose_no_query_credentials_on_admin_reads() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let cases = [
        (
            "detail-internal-https",
            "https://api.private.internal/path?api_key=opaque-secret-411",
            "opaque-secret-411",
            "api.private.internal",
        ),
        (
            "detail-internal-wss",
            "wss://socket.private.svc/path?access_token=opaque-wss-secret-411",
            "opaque-wss-secret-411",
            "socket.private.svc",
        ),
    ];
    let events = cases
        .iter()
        .map(|(id, url, _, _)| {
            usage(
                &w,
                id,
                model,
                json!({
                    "status_code":502,"error_class":"upstream_status","error_message":url,
                    "future_field":{"diagnostic_url":url}
                }),
            )
        })
        .collect();
    let accepted = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":2,"rejected":0,"duplicates":0})
    );
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;

    for (event_id, original_url, credential, internal_host) in cases {
        for path in [
            detail_path(event_id),
            format!("/admin/usage/events?request_id=req-{event_id}"),
            format!("/admin/pricing/charges?event_id={event_id}&limit=1"),
        ] {
            let response = w.cp.get_with_cookie(&path, viewer.cookie()).await;
            assert_eq!(response.status, 200, "{path}: {response:?}");
            let raw = response.body.to_string();
            assert!(
                !raw.contains(credential),
                "内部 URL 的查询凭据不能显示：{raw}"
            );
            assert!(!raw.contains(internal_host), "内部地址不能显示：{raw}");
            let row = if let Some(items) = response.body.get("items") {
                let items = items.as_array().expect("管理记录列表");
                assert_eq!(items.len(), 1, "精确查询的记录必须存在");
                &items[0]
            } else {
                &response.body
            };
            let message = row
                .get("error_message")
                .or_else(|| row.pointer("/event/error_message"))
                .and_then(Value::as_str)
                .expect("具体错误字段仍须返回");
            assert!(!message.is_empty(), "不能靠丢掉整条错误避开凭据检查");
            assert!(
                !message.contains('?'),
                "完整 URL 的查询字符串必须移除：{message}"
            );
            if let Some(event) = row.get("event") {
                let diagnostic_url = event
                    .pointer("/future_field/diagnostic_url")
                    .and_then(Value::as_str)
                    .expect("未知字段保留脱敏后的显示副本");
                assert!(!diagnostic_url.is_empty());
                assert!(
                    !diagnostic_url.contains('?'),
                    "嵌套 URL 的查询字符串必须移除"
                );
            }
        }
        let stored: String = sqlx::query_scalar(
            "SELECT event->>'error_message' FROM usage_events WHERE event_id=$1",
        )
        .bind(event_id)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("已存错误原文");
        assert_eq!(stored, original_url, "读取时脱敏不得修改已存事实");
    }
}

#[tokio::test]
async fn stored_header_maps_redact_opaque_credentials_and_keep_correlation_headers() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let credentials = [
        ("Cookie", "session=opaque-cookie-411"),
        ("X-Goog-Api-Key", "opaque-google-key-411"),
        ("x-amz-security-token", "opaque-aws-session-411"),
        ("ocp-apim-subscription-key", "opaque-apim-key-411"),
    ];
    let mut headers = json!({"x-request-id":"visible-request-header-411"});
    for (name, value) in credentials {
        headers[name] = json!(value);
    }
    let header_maps = [
        "request_headers",
        "upstream_response_headers",
        "headers",
        "response_headers",
        "upstream_headers",
        "default_headers",
    ];
    let mut fields = json!({"future_field":{"api_key_id":"safe-key-id"}});
    for map in header_maps {
        fields[map] = headers.clone();
    }
    let event = usage(&w, "detail-header-credentials", model, fields);
    let accepted = deliver(&w.dp, Uuid::new_v4(), vec![event.clone()]).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":1,"rejected":0,"duplicates":0})
    );
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;

    for path in [
        detail_path("detail-header-credentials"),
        "/admin/usage/events?request_id=req-detail-header-credentials".to_owned(),
    ] {
        let response = w.cp.get_with_cookie(&path, viewer.cookie()).await;
        assert_eq!(response.status, 200, "{path}: {response:?}");
        let raw = response.body.to_string();
        for (_, credential) in credentials {
            assert!(
                !raw.contains(credential),
                "已存 header 的不透明凭据不能显示：{raw}"
            );
        }
        let row = if let Some(items) = response.body.get("items") {
            let items = items.as_array().expect("事件列表");
            assert_eq!(items.len(), 1, "目标事件必须存在");
            &items[0]
        } else {
            &response.body
        };
        for map in header_maps {
            let headers = row["event"]
                .get(map)
                .and_then(Value::as_object)
                .expect("已存 header 对象仍须可读");
            for (name, _) in credentials {
                let shown = headers
                    .get(name)
                    .and_then(Value::as_str)
                    .expect("敏感 header 保留脱敏占位符");
                assert!(!shown.is_empty());
            }
            assert_eq!(
                headers.get("x-request-id"),
                Some(&json!("visible-request-header-411")),
                "非凭据 header 保留可诊断的原值"
            );
        }
        assert_eq!(
            row.pointer("/event/future_field/api_key_id"),
            Some(&json!("safe-key-id")),
            "普通 JSON 中的标识不能当成 header 凭据"
        );
    }
    let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id=$1")
        .bind("detail-header-credentials")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("已存原记录");
    assert_eq!(stored, event, "展示脱敏不得修改已存 header 或用量事实");
}
