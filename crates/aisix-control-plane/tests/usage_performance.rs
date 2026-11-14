//! 真实 CP/Postgres 记录投影：性能、字节与用量必须保留同一事件身份。
mod support;

use aisix_control_plane::auth::MemberRole;
use serde_json::{json, Value};
use support::world::{deliver, price, seed_model, usage, world, World};
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
    .expect("grant the measured model");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    Some((w, model, owner.cookie().to_owned()))
}

#[tokio::test]
async fn one_event_keeps_reported_tokens_bytes_and_performance_on_both_usage_lists() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    let audit = json!({"version":2,"boundary":"upstream_attempt","transport":"http_entity",
        "exchange_count":1,"request_body_bytes":150,"response_body_bytes":180,
        "request_complete":true,"response_complete":true,
        "input_text":{"utf8_bytes":16,"unicode_scalars":8,"complete":true},
        "output_text":{"utf8_bytes":10,"unicode_scalars":5,"complete":true}});
    let event = usage(
        &w,
        "one-measured-event",
        model,
        json!({
            "request_id":"one-measured-request","status_code":200,"error_class":"",
            "is_final_attempt":true,"upstream_dispatched":true,"prompt_tokens":7,
            "completion_tokens":3,"total_tokens":10,"first_token_latency_ms":0,
            "downstream_latency_ms":20,"upstream_latency_ms":125,"upstream_tpot_ms":25,
            "io_audit":audit
        }),
    );
    for _ in 0..2 {
        let delivered = deliver(&w.dp, Uuid::new_v4(), vec![event.clone()]).await;
        assert_eq!(delivered.status, 200, "{delivered:?}");
    }
    for (path, cookie) in [
        ("/api/usage/calls", owner.as_str()),
        ("/admin/pricing/charges", w.admin.cookie()),
    ] {
        let response = w.cp.send("GET", path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{response:?}");
        let rows = response.body["items"].as_array().expect("explicit items");
        assert_eq!(
            rows.len(),
            1,
            "retry must retain one usage row: {response:?}"
        );
        let row = &rows[0];
        assert_eq!(row.get("event_id"), Some(&json!("one-measured-event")));
        assert_eq!(row.get("first_token_latency_ms"), Some(&json!(0)), "{row}");
        assert_eq!(row.get("attempt_duration_ms"), Some(&json!(125)), "{row}");
        assert_eq!(row.get("throughput_tps"), Some(&json!(40.0)), "{row}");
        assert_eq!(row.get("io_audit"), Some(&audit), "{row}");
        assert_eq!(
            row.get("io_token_reference"),
            Some(&json!({"method":"utf8_bytes_div4_v1","input_tokens":4,"output_tokens":3})),
            "{row}"
        );
        assert_eq!(row["usage"].get("prompt_tokens"), Some(&json!(7)), "{row}");
        assert_eq!(
            row["usage"].get("completion_tokens"),
            Some(&json!(3)),
            "{row}"
        );
    }
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM usage_events WHERE event_id='one-measured-event'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("one stored event");
    let charges: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM pricing_charges_cny WHERE event_id='one-measured-event'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("one stored charge");
    assert_eq!((events, charges), (1, 1));
}

#[tokio::test]
async fn performance_keeps_unknown_distinct_from_zero_and_never_invents_tps() {
    let Some((w, model, owner)) = setup().await else {
        return;
    };
    let cases = [
        (
            "known",
            json!({"first_token_latency_ms":0,"upstream_latency_ms":100,"upstream_tpot_ms":20}),
            Some(50.0),
        ),
        ("missing", json!({}), None),
        ("zero", json!({"upstream_tpot_ms":0}), None),
        ("negative", json!({"upstream_tpot_ms":-1}), None),
        ("text", json!({"upstream_tpot_ms":"20"}), None),
        ("out-of-range", json!({"upstream_tpot_ms":1e200}), None),
        (
            "failed",
            json!({"status_code":503,"error_class":"upstream_status","upstream_tpot_ms":20}),
            None,
        ),
        (
            "stream-error",
            json!({"error_class":"stream_aborted","upstream_tpot_ms":20}),
            None,
        ),
        (
            "cache",
            json!({"cache_status":"hit","upstream_tpot_ms":20}),
            None,
        ),
        (
            "not-dispatched",
            json!({"upstream_dispatched":false,"upstream_tpot_ms":20}),
            None,
        ),
    ];
    let mut events = Vec::new();
    for (id, extra, _) in &cases {
        let mut fields = json!({"status_code":200,"error_class":"","is_final_attempt":true,
            "upstream_dispatched":true,"prompt_tokens":7,"completion_tokens":3,"total_tokens":10});
        fields
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("case fields").clone());
        events.push(usage(&w, id, model, fields));
    }
    let delivered = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    for (path, cookie) in [
        ("/api/usage/calls", owner.as_str()),
        ("/admin/pricing/charges", w.admin.cookie()),
    ] {
        let response = w.cp.send("GET", path, Some(cookie), None).await;
        assert_eq!(response.status, 200, "{response:?}");
        let rows = response.body["items"].as_array().expect("explicit items");
        assert_eq!(rows.len(), cases.len(), "{response:?}");
        for (id, _, expected) in &cases {
            let row = rows
                .iter()
                .find(|row| row["event_id"] == *id)
                .expect("known event");
            assert_eq!(
                row.get("throughput_tps"),
                Some(&expected.map_or(Value::Null, |n| json!(n))),
                "{path}: {row}"
            );
            let (first, duration) = if *id == "known" {
                (json!(0), json!(100))
            } else {
                (Value::Null, Value::Null)
            };
            assert_eq!(row.get("first_token_latency_ms"), Some(&first), "{row}");
            assert_eq!(row.get("attempt_duration_ms"), Some(&duration), "{row}");
        }
    }
}
