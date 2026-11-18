mod support;

use aisix_pricing::ENGINE_VERSION;
use serde_json::{json, Value};
use support::world::{db_now, deliver, price, seed_model, usage, world};
use uuid::Uuid;

#[tokio::test]
async fn published_cache_lifetimes_and_short_tiers_reach_charges_and_the_ledger() {
    let Some(w) = world().await else { return };
    let mixed = seed_model(&w.cp, &w.admin).await;
    let missing = seed_model(&w.cp, &w.admin).await;
    let limited = seed_model(&w.cp, &w.admin).await;
    for (model, card) in [
        (
            mixed,
            json!({"default":{"base":{"input":"1","output":"2","cache_read":"0.1","cache_write":"1.25","cache_write_12h":"2.5"}}}),
        ),
        (
            missing,
            json!({"default":{"base":{"input":"1","output":"2","cache_write":"1.25"}}}),
        ),
        (
            limited,
            json!({
                "default":{"base":{"input":"2.5","output":"15","cache_read":"0.25"},"long_context":[{"above_input_tokens":272000,"rates":{"input":"5","output":"22.5","cache_read":"0.5"}}]},
                "priority":{"base":{"input":"5","output":"30","cache_read":"0.5"},"unpriced_above_input_tokens":272000}
            }),
        ),
    ] {
        price(&w.cp, model).await;
        let written = w.cp.send("POST", "/admin/pricing/baselines", Some(w.admin.cookie()), Some(json!({
            "model_id":model,"effective_from":db_now(&w.cp).await,"card":card,"note":"官方计量合同回归的新建测试模型"
        }))).await;
        assert_eq!(written.status, 201, "{written:?}");
        assert_eq!(written.body["revision"]["card"], card);
    }
    let answer = deliver(&w.dp, Uuid::new_v4(), vec![
        usage(&w, "mixed-ttl", mixed, json!({"prompt_tokens":12000,"cached_prompt_tokens":4000,"cache_write_tokens":7000,"cache_write_30m_tokens":3000,"cache_write_12h_tokens":4000,"completion_tokens":100,"total_tokens":12100})),
        usage(&w, "missing-12h", missing, json!({"prompt_tokens":1000,"cache_write_tokens":1000,"cache_write_30m_tokens":0,"cache_write_12h_tokens":1000})),
        usage(&w, "short-priority", limited, json!({"service_tier":"priority","prompt_tokens":1000,"completion_tokens":100})),
        usage(&w, "long-priority", limited, json!({"service_tier":"priority","prompt_tokens":272001})),
    ]).await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        answer.body,
        json!({"accepted":4,"rejected":0,"duplicates":0})
    );
    for (id, status, charge, code) in [
        ("mixed-ttl", "charged", Some(15350_i64), None),
        (
            "missing-12h",
            "unpriced",
            None,
            Some("missing_dimension_price"),
        ),
        ("short-priority", "charged", Some(8000), None),
        (
            "long-priority",
            "unpriced",
            None,
            Some("missing_long_context_price"),
        ),
    ] {
        let stored: (String, Option<i64>, Option<String>, i64) = sqlx::query_as(
            "SELECT status,charge_micro_cny,error_code,engine_version FROM pricing_charges_cny WHERE event_id=$1"
        ).bind(id).fetch_one(w.cp.db().pool()).await.unwrap();
        assert_eq!(
            stored,
            (
                status.into(),
                charge,
                code.map(str::to_owned),
                i64::from(ENGINE_VERSION)
            ),
            "{id}"
        );
    }
    // 缺价只保留未定价记录；正式账本仅有两笔正确借记。
    let ledger: (i64, i64) = sqlx::query_as("SELECT count(*)::bigint,COALESCE(sum(amount_micro_cny),0)::bigint FROM billing_ledger_cny WHERE tenant_id=$1 AND kind='usage'")
        .bind(w.tenant).fetch_one(w.cp.db().pool()).await.unwrap();
    assert_eq!(ledger, (2, -23350));
    let raw: Value =
        sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id='missing-12h'")
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
    assert_eq!(raw.get("cache_write_30m_tokens"), Some(&json!(0)));
    assert_eq!(raw.get("cache_write_12h_tokens"), Some(&json!(1000)));
}

#[tokio::test]
async fn an_incomplete_batch_cache_split_never_debits_the_account() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let rates = json!({"input":"1","output":"2","cache_write":"1.25","cache_write_12h":"2.5"});
    let card = json!({"default":{"base":rates},"batch":{"base":rates}});
    let written = w.cp.send("POST", "/admin/pricing/baselines", Some(w.admin.cookie()), Some(json!({
        "model_id":model,"effective_from":db_now(&w.cp).await,"card":card,"note":"批次缓存时长不完整时不可按短价扣款"
    }))).await;
    assert_eq!(written.status, 201, "{written:?}");
    assert_eq!(written.body["revision"]["card"], card);
    let balance_before: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
    let ledger_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_ledger_cny WHERE tenant_id=$1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();

    // 两种未知拆分均有合法总量；只有明确的不完整标记阻止旧短价回退。
    let refused = deliver(&w.dp, Uuid::new_v4(), vec![
        usage(&w, "batch-incomplete-absent", model, json!({"is_batch":true,"prompt_tokens":1000,"cache_write_tokens":1000,"cache_write_split_incomplete":true})),
        usage(&w, "batch-incomplete-null", model, json!({"is_batch":true,"prompt_tokens":1000,"cache_write_tokens":1000,"cache_write_30m_tokens":null,"cache_write_12h_tokens":null,"cache_write_split_incomplete":true})),
    ]).await;
    assert_eq!(refused.status, 200, "{refused:?}");
    assert_eq!(
        refused.body,
        json!({"accepted":2,"rejected":0,"duplicates":0})
    );
    for id in ["batch-incomplete-absent", "batch-incomplete-null"] {
        let charge: (String, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT status,charge_micro_cny,error_code FROM pricing_charges_cny WHERE event_id=$1",
        )
        .bind(id)
        .fetch_one(w.cp.db().pool())
        .await
        .unwrap();
        assert_eq!(
            charge,
            ("unpriced".into(), None, Some("inconsistent_usage".into())),
            "{id}"
        );
        let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id=$1")
            .bind(id)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
        assert_eq!(
            stored.get("cache_write_split_incomplete"),
            Some(&json!(true)),
            "{id}"
        );
    }
    let balance_after: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
    let ledger_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_ledger_cny WHERE tenant_id=$1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
    assert_eq!(balance_after, balance_before);
    assert_eq!(ledger_after, ledger_before);

    let accepted = deliver(&w.dp, Uuid::new_v4(), vec![
        usage(&w, "batch-legacy-absent", model, json!({"is_batch":true,"prompt_tokens":1000,"cache_write_tokens":1000})),
        usage(&w, "batch-legacy-false", model, json!({"is_batch":true,"prompt_tokens":1000,"cache_write_tokens":1000,"cache_write_split_incomplete":false})),
    ]).await;
    assert_eq!(accepted.status, 200, "{accepted:?}");
    assert_eq!(
        accepted.body,
        json!({"accepted":2,"rejected":0,"duplicates":0})
    );
    for (id, flag) in [
        ("batch-legacy-absent", None),
        ("batch-legacy-false", Some(json!(false))),
    ] {
        let charge: (String, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT status,charge_micro_cny,error_code FROM pricing_charges_cny WHERE event_id=$1",
        )
        .bind(id)
        .fetch_one(w.cp.db().pool())
        .await
        .unwrap();
        assert_eq!(charge, ("charged".into(), Some(1250), None), "{id}");
        let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id=$1")
            .bind(id)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
        assert_eq!(
            stored.get("cache_write_split_incomplete"),
            flag.as_ref(),
            "{id}"
        );
        let debit: i64 = sqlx::query_scalar("SELECT amount_micro_cny FROM billing_ledger_cny WHERE tenant_id=$1 AND event_id=$2 AND kind='usage'")
            .bind(w.tenant).bind(id).fetch_one(w.cp.db().pool()).await.unwrap();
        assert_eq!(debit, -1250, "{id}");
    }
    let final_balance: i64 =
        sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id=$1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .unwrap();
    assert_eq!(final_balance, balance_before - 2500);
}
