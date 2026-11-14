//! 长度审计通过真实遥测接收和详情查询；它与计费用量分开。
mod support;

use aisix_control_plane::auth::MemberRole;
use serde_json::{json, Value};
use support::world::{deliver, price, seed_model, tokens_680, usage, world};
use uuid::Uuid;

fn audit() -> Value {
    json!({
        "version": 1,
        "boundary": "client_exchange",
        "request_body_bytes": 91,
        "response_body_bytes": 47,
        "response_complete": false,
        "input_text": {"utf8_bytes": 8, "unicode_scalars": 3, "complete": true},
        "output_text": {"utf8_bytes": 4, "unicode_scalars": 1, "complete": false}
    })
}

fn call<'a>(items: &'a Value, id: &str) -> &'a Value {
    items["items"]
        .as_array()
        .expect("调用列表")
        .iter()
        .find(|item| item["event_id"] == id)
        .unwrap_or_else(|| panic!("缺少调用 {id}: {items}"))
}

#[tokio::test]
async fn io_audit_survives_intake_and_both_call_details_without_changing_charges() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let mut events = Vec::new();
    for (id, measurement) in [
        ("io-old", None),
        ("io-unicode", Some(audit())),
        (
            "io-zero",
            Some(json!({"version":1,"boundary":"client_exchange",
                "request_body_bytes":0,"response_body_bytes":0,"response_complete":true,
                "input_text":{"utf8_bytes":0,"unicode_scalars":0,"complete":true},
                "output_text":{"utf8_bytes":0,"unicode_scalars":0,"complete":true}})),
        ),
        (
            "io-ws",
            Some(
                json!({"version":1,"boundary":"client_exchange","response_complete":true,
                "input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true}}),
            ),
        ),
        (
            "io-unsupported",
            Some({
                let mut a = audit();
                a["version"] = json!(2);
                a
            }),
        ),
    ] {
        let mut fields = tokens_680();
        fields["is_final_attempt"] = json!(true);
        if let Some(measurement) = measurement {
            fields["io_audit"] = measurement;
        }
        events.push(usage(&w, id, model, fields));
    }
    let delivered = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(
        delivered.body,
        json!({"accepted":5,"rejected":0,"duplicates":0})
    );
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for (path, cookie) in [
        (
            format!("/admin/pricing/charges?tenant_id={}", w.tenant),
            w.admin.cookie(),
        ),
        ("/api/usage/calls".to_owned(), owner.cookie()),
    ] {
        let response = w.cp.send("GET", &path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{response:?}");
        let old = call(&response.body, "io-old");
        assert_eq!(old.get("io_audit"), Some(&Value::Null), "{old}");
        assert_eq!(old.get("io_token_reference"), Some(&Value::Null), "{old}");
        let unicode = call(&response.body, "io-unicode");
        assert_eq!(unicode.get("io_audit"), Some(&audit()), "{unicode}");
        assert_eq!(
            unicode.get("io_token_reference"),
            Some(&json!({
                "method":"utf8_bytes_div4_v1","input_tokens":2,"output_tokens":1
            })),
            "{unicode}"
        );
        let zero = call(&response.body, "io-zero");
        assert_eq!(
            zero["io_audit"].get("request_body_bytes"),
            Some(&json!(0)),
            "{zero}"
        );
        assert_eq!(
            zero["io_audit"].get("response_body_bytes"),
            Some(&json!(0)),
            "{zero}"
        );
        assert_eq!(
            zero["io_token_reference"].get("input_tokens"),
            Some(&json!(0)),
            "{zero}"
        );
        assert_eq!(
            zero["io_token_reference"].get("output_tokens"),
            Some(&json!(0)),
            "{zero}"
        );
        let ws = call(&response.body, "io-ws");
        assert!(ws["io_audit"].get("request_body_bytes").is_none(), "{ws}");
        assert!(ws["io_audit"].get("response_body_bytes").is_none(), "{ws}");
        assert_eq!(
            ws["io_token_reference"].get("input_tokens"),
            Some(&json!(2)),
            "{ws}"
        );
        assert!(
            ws["io_token_reference"].get("output_tokens").is_none(),
            "{ws}"
        );
        let unsupported = call(&response.body, "io-unsupported");
        assert_eq!(
            unsupported.get("io_audit"),
            Some(&Value::Null),
            "{unsupported}"
        );
        assert_eq!(
            unsupported.get("io_token_reference"),
            Some(&Value::Null),
            "{unsupported}"
        );
        for id in ["io-old", "io-unicode", "io-zero", "io-ws", "io-unsupported"] {
            let row = call(&response.body, id);
            let amount = row
                .get("charge_micro_cny")
                .or_else(|| row["charge"].get("amount_micro_cny"));
            assert_eq!(amount, Some(&json!(680)), "长度审计不能参与计价: {row}");
        }
    }
}

#[tokio::test]
async fn call_details_preserve_usage_source_and_disjoint_cache_input_totals() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let mut events = Vec::new();
    for (id, prompt, cached, read, created, estimated) in [
        ("io-subset", 100, 80, 0, 0, Some(false)),
        ("io-additive", 100, 0, 80, 20, Some(true)),
        ("io-source-unknown", 0, 0, 0, 0, None),
    ] {
        let mut fields = json!({"is_final_attempt":true,"status_code":200,
            "prompt_tokens":prompt,"completion_tokens":7,"cached_prompt_tokens":cached,
            "cache_read_tokens":read,"cache_creation_tokens":created,"io_audit":audit()});
        if let Some(estimated) = estimated {
            fields["usage_estimated"] = json!(estimated);
        }
        events.push(usage(&w, id, model, fields));
    }
    let delivered = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for (path, cookie) in [
        (
            format!("/admin/pricing/charges?tenant_id={}", w.tenant),
            w.admin.cookie(),
        ),
        ("/api/usage/calls".to_owned(), owner.cookie()),
    ] {
        let response = w.cp.send("GET", &path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{response:?}");
        for (id, input, estimated) in [
            ("io-subset", 100, json!(false)),
            ("io-additive", 200, json!(true)),
            ("io-source-unknown", 0, Value::Null),
        ] {
            let row = call(&response.body, id);
            assert_eq!(row.get("usage_input_tokens"), Some(&json!(input)), "{row}");
            assert_eq!(row.get("usage_estimated"), Some(&estimated), "{row}");
            assert_eq!(
                row["usage"].get("completion_tokens"),
                Some(&json!(7)),
                "{row}"
            );
            assert_eq!(
                row["io_token_reference"].get("input_tokens"),
                Some(&json!(2)),
                "{row}"
            );
        }
    }
}

#[tokio::test]
async fn a_partial_text_reference_rounds_non_multiple_bytes_up() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let measured = json!({"version":1,"boundary":"client_exchange","response_complete":false,
        "input_text":{"utf8_bytes":5,"unicode_scalars":2,"complete":false},
        "output_text":{"utf8_bytes":1,"unicode_scalars":1,"complete":false}});
    let delivered = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(
            &w,
            "io-ceiling",
            model,
            json!({"is_final_attempt":true,"io_audit":measured}),
        )],
    )
    .await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let response =
        w.cp.send(
            "GET",
            "/admin/pricing/charges?event_id=io-ceiling",
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(response.status, 200, "{response:?}");
    let row = call(&response.body, "io-ceiling");
    assert_eq!(row.get("io_audit"), Some(&measured), "{row}");
    assert_eq!(
        row.get("io_token_reference"),
        Some(&json!({
            "method":"utf8_bytes_div4_v1","input_tokens":2,"output_tokens":1
        })),
        "取整向下或缺失变零必须失败: {row}"
    );
}

#[tokio::test]
async fn upstream_snapshots_survive_normal_intake_and_both_projections_with_exact_original_billing()
{
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let measurements = [
        (
            "upstream-io-unicode",
            json!({"version":2,"boundary":"upstream_attempt",
                "transport":"http_entity",
                "request_body_bytes":123,"response_body_bytes":79,
                "request_complete":true,"response_complete":false,"exchange_count":1,
                "input_text":{"utf8_bytes":8,"unicode_scalars":3,"complete":true},
                "output_text":{"utf8_bytes":5,"unicode_scalars":2,"complete":false}}),
            json!({"method":"utf8_bytes_div4_v1","input_tokens":2,"output_tokens":2}),
        ),
        (
            "upstream-io-zero",
            json!({"version":2,"boundary":"upstream_attempt",
                "transport":"websocket_payload",
                "request_body_bytes":0,"response_body_bytes":0,
                "request_complete":false,"response_complete":false,"exchange_count":0,
                "input_text":{"utf8_bytes":0,"unicode_scalars":0,"complete":true},
                "output_text":{"utf8_bytes":0,"unicode_scalars":0,"complete":true}}),
            json!({"method":"utf8_bytes_div4_v1","input_tokens":0,"output_tokens":0}),
        ),
        (
            "upstream-io-unknown",
            json!({"version":2,"boundary":"upstream_attempt","response_complete":false}),
            json!({"method":"utf8_bytes_div4_v1"}),
        ),
        (
            "upstream-io-legacy",
            audit(),
            json!({"method":"utf8_bytes_div4_v1","input_tokens":2,"output_tokens":1}),
        ),
        (
            "upstream-io-unsupported",
            json!({"version":255,"boundary":"upstream_attempt","response_complete":false}),
            Value::Null,
        ),
        (
            "upstream-io-mismatched",
            json!({"version":1,"boundary":"upstream_attempt","response_complete":false}),
            Value::Null,
        ),
    ];
    let events: Vec<Value> = measurements
        .iter()
        .map(|(id, measurement, _)| {
            let mut fields = tokens_680();
            fields["is_final_attempt"] = json!(true);
            fields["usage_estimated"] = json!(false);
            fields["total_tokens"] = json!(1200);
            fields["io_audit"] = measurement.clone();
            usage(&w, id, model, fields)
        })
        .collect();
    let sent = deliver(&w.dp, Uuid::new_v4(), events.clone()).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body, json!({"accepted":6,"rejected":0,"duplicates":0}));
    for event in &events {
        let stored: Value =
            sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id = $1")
                .bind(event["event_id"].as_str().expect("事件身份"))
                .fetch_one(w.cp.db().pool())
                .await
                .expect("读取原始事件");
        assert_eq!(&stored, event, "长度快照不能改写任何原始计数");
        let charge: (i64, i64, i64) = sqlx::query_as(
            "SELECT charge_micro_cny, \
             (SELECT count(*) FROM billing_ledger_cny WHERE event_id = $1), \
             (SELECT amount_micro_cny FROM billing_ledger_cny WHERE event_id = $1) \
             FROM pricing_charges_cny WHERE event_id = $1",
        )
        .bind(event["event_id"].as_str().expect("事件身份"))
        .fetch_one(w.cp.db().pool())
        .await
        .expect("读取一次计价及原始账本");
        assert_eq!(charge, (680, 1, -680));
    }
    assert_eq!(w.cp.count("usage_io_audit_pending").await, 0);
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for (path, cookie) in [
        (
            format!("/admin/pricing/charges?tenant_id={}", w.tenant),
            w.admin.cookie(),
        ),
        ("/api/usage/calls".to_owned(), owner.cookie()),
    ] {
        let response = w.cp.send("GET", &path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{response:?}");
        for (id, measurement, reference) in &measurements {
            let row = call(&response.body, id);
            let expected = if reference.is_null() {
                &Value::Null
            } else {
                measurement
            };
            assert_eq!(row.get("io_audit"), Some(expected), "{row}");
            assert_eq!(row.get("io_token_reference"), Some(reference), "{row}");
            assert_eq!(row.get("usage_estimated"), Some(&json!(false)), "{row}");
            assert_eq!(row.get("usage_input_tokens"), Some(&json!(1000)), "{row}");
            for (field, expected) in [
                ("prompt_tokens", 1000),
                ("cached_prompt_tokens", 800),
                ("completion_tokens", 200),
                ("total_tokens", 1200),
            ] {
                assert_eq!(row["usage"].get(field), Some(&json!(expected)), "{row}");
            }
            let amount = row
                .get("charge_micro_cny")
                .or_else(|| row["charge"].get("amount_micro_cny"));
            assert_eq!(amount, Some(&json!(680)), "{row}");
        }
    }
}

#[tokio::test]
async fn aggregate_upstream_lengths_keep_exact_usage_and_charges_without_a_single_call_reference() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let measurement = json!({"version":2,"boundary":"upstream_attempt","transport":"mixed",
        "request_body_bytes":234,"response_body_bytes":156,
        "request_complete":true,"response_complete":true,"exchange_count":2,
        "input_text":{"utf8_bytes":16,"unicode_scalars":6,"complete":true},
        "output_text":{"utf8_bytes":10,"unicode_scalars":4,"complete":true}});
    let mut fields = tokens_680();
    fields["is_final_attempt"] = json!(true);
    fields["usage_estimated"] = json!(false);
    fields["total_tokens"] = json!(1200);
    fields["io_audit"] = measurement.clone();
    let event = usage(&w, "upstream-io-aggregate", model, fields);
    let sent = deliver(&w.dp, Uuid::new_v4(), vec![event.clone()]).await;
    assert_eq!(sent.status, 200, "{sent:?}");
    assert_eq!(sent.body, json!({"accepted":1,"rejected":0,"duplicates":0}));
    let stored: Value = sqlx::query_scalar(
        "SELECT event FROM usage_events WHERE event_id = 'upstream-io-aggregate'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("读取原始聚合长度与记录用量");
    assert_eq!(stored, event);
    let charge: (i64, i64, i64) = sqlx::query_as(
        "SELECT charge_micro_cny, \
         (SELECT count(*) FROM billing_ledger_cny WHERE event_id = 'upstream-io-aggregate'), \
         (SELECT amount_micro_cny FROM billing_ledger_cny WHERE event_id = 'upstream-io-aggregate') \
         FROM pricing_charges_cny WHERE event_id = 'upstream-io-aggregate'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("读取聚合长度事件的唯一收费");
    assert_eq!(charge, (680, 1, -680));
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    for (path, cookie) in [
        (
            format!("/admin/pricing/charges?tenant_id={}", w.tenant),
            w.admin.cookie(),
        ),
        ("/api/usage/calls".to_owned(), owner.cookie()),
    ] {
        let response = w.cp.send("GET", &path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{response:?}");
        let row = call(&response.body, "upstream-io-aggregate");
        assert_eq!(row.get("io_audit"), Some(&measurement), "{row}");
        assert_eq!(row.get("io_token_reference"), Some(&Value::Null), "{row}");
        assert_eq!(row.get("usage_estimated"), Some(&json!(false)), "{row}");
        assert_eq!(row.get("usage_input_tokens"), Some(&json!(1000)), "{row}");
        for (field, expected) in [
            ("prompt_tokens", 1000),
            ("cached_prompt_tokens", 800),
            ("completion_tokens", 200),
            ("total_tokens", 1200),
        ] {
            assert_eq!(row["usage"].get(field), Some(&json!(expected)), "{row}");
        }
        let amount = row
            .get("charge_micro_cny")
            .or_else(|| row["charge"].get("amount_micro_cny"));
        assert_eq!(amount, Some(&json!(680)), "{row}");
    }
}
