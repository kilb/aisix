//! 中断的 Messages 流保留用量证据，账本只记录可计费的尝试。
mod support;

use aisix_pricing::RateCard;
use serde_json::{json, Value};
use support::world::{
    db_now, deliver, group_id, insert_baseline, insert_rate, usage, world, World, SHARED_GROUP,
};
use uuid::Uuid;

async fn resource(w: &World, collection: &str, doc: Value) -> Uuid {
    let response =
        w.cp.send(
            "POST",
            &format!("/admin/resources/{collection}"),
            Some(w.admin.cookie()),
            Some(json!({"doc":doc})),
        )
        .await;
    assert_eq!(response.status, 201, "{response:?}");
    serde_json::from_value(response.body["id"].clone()).expect("资源 ID")
}

#[tokio::test]
async fn interrupted_502_messages_keep_usage_without_posting_a_debit() {
    let Some(w) = world().await else { return };
    let account = resource(
        &w,
        "provider_keys",
        json!({"display_name":"billing-502-account","provider":"anthropic",
               "api_key":"billing-502-test-only","groups":["shared"]}),
    )
    .await;
    let model = resource(
        &w,
        "models",
        json!({"display_name":"billing-502-model","provider":"anthropic",
               "model_name":"claude-sonnet-4-5","provider_key_id":account}),
    )
    .await;
    let now = db_now(&w.cp).await;
    let group = group_id(&w.cp, SHARED_GROUP).await;
    insert_rate(&w.cp, group, now).await.expect("租户费率");
    let card: RateCard = serde_json::from_value(json!({"default":{"base":{
        "input":"1","output":"2","cache_read":"3","cache_write":"4"
    }}}))
    .expect("测试价目卡");
    insert_baseline(&w.cp, model, now, card)
        .await
        .expect("模型价格");
    let rate =
        w.cp.send(
            "POST",
            &format!("/admin/provider-keys/{account}/cost-rates"),
            Some(w.admin.cookie()),
            Some(json!({"cny_per_usd":"2","effective_from":now})),
        )
        .await;
    assert_eq!(rate.status, 201, "{rate:?}");

    let mut initial = usage(
        &w,
        "initial-403",
        model,
        json!({"request_id":"failed-messages","operation":"messages","stream":true,
               "status_code":403,"error_class":"upstream_status","attempt_index":0,
               "attempt_kind":"initial","is_final_attempt":false}),
    );
    initial["provider_key_id"] = json!(account);
    let free = (0, None);
    let mut events = vec![(initial, free)];
    // 每个可计费用量的费用为 28 + 46×2 + 296967×3 + 3004×4，成本费率为 2。
    let paid = (903_037, Some(1_806_074));
    for (id, status, class, estimated, output, money) in [
        ("fallback-502", 502, "stream_aborted", Some(true), 46, free),
        (
            "provisional-502",
            502,
            "stream_aborted",
            Some(false),
            1,
            free,
        ),
        ("legacy-502", 502, "stream_aborted", None, 1, free),
        (
            "estimated-502",
            502,
            "upstream_status",
            Some(true),
            46,
            free,
        ),
        (
            "reported-502",
            502,
            "upstream_status",
            Some(false),
            46,
            paid,
        ),
        ("estimated-200", 200, "", Some(true), 46, paid),
        ("cancelled-499", 499, "", Some(true), 46, paid),
    ] {
        let mut event = usage(
            &w,
            id,
            model,
            json!({"operation":"messages","stream":true,"status_code":status,
                   "error_class":class,"provider_key_id":account,"is_final_attempt":true,
                   "prompt_tokens":28,"completion_tokens":output,
                   "cache_read_tokens":296_967,"cache_creation_tokens":3_004}),
        );
        if id == "fallback-502" {
            event["request_id"] = json!("failed-messages");
            event["attempt_index"] = json!(1);
            event["attempt_kind"] = json!("fallback");
        }
        if let Some(estimated) = estimated {
            event["usage_estimated"] = json!(estimated);
        }
        events.push((event, money));
    }
    let delivered = deliver(
        &w.dp,
        Uuid::new_v4(),
        events.iter().map(|(event, _)| event.clone()).collect(),
    )
    .await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(
        delivered.body,
        json!({"accepted":8,"rejected":0,"duplicates":0})
    );

    for (event, (charge, cost)) in events {
        let id = event["event_id"].as_str().expect("用量事件 ID");
        let response =
            w.cp.send(
                "GET",
                &format!("/admin/pricing/charges?event_id={id}"),
                Some(w.admin.cookie()),
                None,
            )
            .await;
        assert_eq!(response.status, 200, "{response:?}");
        let items = response.body["items"].as_array().expect("收费记录");
        assert_eq!(items.len(), 1, "{id}: {response:?}");
        let row = &items[0];
        let status = if charge == 0 {
            "nothing_to_bill"
        } else {
            "charged"
        };
        assert_eq!(row.get("event_id"), Some(&json!(id)));
        assert_eq!(
            row.get("charge_micro_cny"),
            Some(&json!(charge)),
            "{id}: {row}"
        );
        assert_eq!(row.get("status"), Some(&json!(status)), "{id}: {row}");
        assert_eq!(row.get("cost_micro_cny"), Some(&json!(cost)), "{id}: {row}");
        if charge == 0 {
            assert_eq!(row.get("lines"), Some(&json!([])), "{id}: {row}");
        }

        let debit: Option<(String, i64)> = sqlx::query_as(
            "SELECT kind, amount_micro_cny FROM billing_ledger_cny WHERE event_id = $1",
        )
        .bind(id)
        .fetch_optional(w.cp.db().pool())
        .await
        .expect("读取对应的账本记录");
        assert_eq!(
            debit,
            (charge > 0).then(|| ("usage".to_owned(), -charge)),
            "{id}"
        );

        let stored: Value =
            sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id = $1")
                .bind(id)
                .fetch_one(w.cp.db().pool())
                .await
                .expect("读取原始用量事件");
        for field in [
            "request_id",
            "attempt_index",
            "attempt_kind",
            "is_final_attempt",
            "operation",
            "stream",
            "status_code",
            "error_class",
            "usage_estimated",
            "prompt_tokens",
            "completion_tokens",
            "cache_read_tokens",
            "cache_creation_tokens",
        ] {
            assert_eq!(stored.get(field), event.get(field), "{id}: {field}");
        }
    }
    let balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id = $1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("读取租户余额");
    assert_eq!(balance, -2_709_111, "只有三次可计费用量扣减余额");
}
