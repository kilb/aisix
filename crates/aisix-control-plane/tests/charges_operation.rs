//! 计费列表说明统计操作的用途，同时保留原始收费与账本结果。
mod support;

use serde_json::{json, Value};
use support::world::{deliver, price, seed_model, tokens_680, usage, world};
use uuid::Uuid;

#[tokio::test]
async fn charge_operation_explains_token_counting_without_changing_billing() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let counted = usage(
        &w,
        "operation-count",
        model,
        json!({"operation":"count_tokens"}),
    );
    let generated = usage(&w, "operation-chat", model, tokens_680());
    let failed = usage(
        &w,
        "operation-count-failed",
        model,
        json!({"operation":"count_tokens","status_code":502,"error_class":"upstream_status"}),
    );
    let mut absent = usage(&w, "operation-absent", model, json!({}));
    absent
        .as_object_mut()
        .expect("事件对象")
        .remove("operation");
    let null = usage(&w, "operation-null", model, json!({"operation":null}));
    let response = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![counted, generated, failed, absent, null],
    )
    .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.body,
        json!({"accepted":5,"rejected":0,"duplicates":0})
    );

    for (id, operation, status, amount) in [
        (
            "operation-count",
            json!("count_tokens"),
            "nothing_to_bill",
            Some(0),
        ),
        ("operation-chat", json!("chat"), "charged", Some(680)),
        (
            "operation-count-failed",
            json!("count_tokens"),
            "nothing_to_bill",
            Some(0),
        ),
        ("operation-absent", Value::Null, "nothing_to_bill", Some(0)),
        ("operation-null", Value::Null, "unpriced", None),
    ] {
        let response =
            w.cp.send(
                "GET",
                &format!("/admin/pricing/charges?event_id={id}"),
                Some(w.admin.cookie()),
                None,
            )
            .await;
        assert_eq!(response.status, 200, "{response:?}");
        let items = response.body["items"].as_array().expect("计费列表");
        assert_eq!(items.len(), 1, "{response:?}");
        let row = &items[0];
        assert_eq!(
            row.get("operation"),
            Some(&operation),
            "必须明确返回操作类型，包括未知值：{row}"
        );
        assert_eq!(row["status"], status, "{row}");
        assert_eq!(row.get("charge_micro_cny"), Some(&json!(amount)), "{row}");
        if id == "operation-count" {
            assert_eq!(row["usage"].get("prompt_tokens"), Some(&json!(0)));
            assert_eq!(row["usage"].get("completion_tokens"), Some(&json!(0)));
            assert_eq!(row["lines"], json!([]));
        }
        if id == "operation-null" {
            assert_eq!(row["error_code"], "undecodable_usage");
        }
    }
    let postings: Vec<(String, i64)> = sqlx::query_as(
        "SELECT event_id, amount_micro_cny FROM billing_ledger_cny WHERE event_id LIKE 'operation-%' ORDER BY event_id",
    ).fetch_all(w.cp.db().pool()).await.expect("读取账本");
    assert_eq!(postings, vec![("operation-chat".to_owned(), -680)]);
}
