//! Billing accounts as people read them: operators list every customer
//! account and read or change one tenant's credit limit, and a tenant's
//! owner, admin or billing member reads its own balance. A credit-limit
//! change settles the tenant's balance mark in the request itself.
//!
//! Balances a test needs are set with raw SQL: the routes that move them
//! come later.

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use sqlx::PgPool;
use support::world::world;
use support::TestCp;
use uuid::Uuid;

/// Sets `tenant`'s balance and credit limit by raw SQL, and its billing mode.
async fn set_account(pool: &PgPool, tenant: Uuid, mode: &str, balance: i64, limit: i64) {
    sqlx::query("UPDATE tenants SET billing_mode = $2 WHERE id = $1")
        .bind(tenant)
        .bind(mode)
        .execute(pool)
        .await
        .expect("set the billing mode");
    sqlx::query(
        "UPDATE billing_accounts SET balance_micro_cny = $2, credit_limit_micro_cny = $3 \
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .bind(balance)
    .bind(limit)
    .execute(pool)
    .await
    .expect("set the account");
}

/// The stored balance document of `tenant`: `(doc, deleted)`.
async fn balance_doc(pool: &PgPool, tenant: Uuid) -> Option<Value> {
    sqlx::query_scalar(
        "SELECT g.doc FROM gw_resources g JOIN billing_accounts a ON a.balance_budget_id = g.id \
         WHERE g.kind = 'budgets' AND a.tenant_id = $1 AND NOT g.deleted",
    )
    .bind(tenant)
    .fetch_optional(pool)
    .await
    .expect("read the balance document")
}

async fn audit_count(pool: &PgPool, tenant: Uuid, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1 AND tenant_id = $2")
        .bind(action)
        .bind(tenant)
        .fetch_one(pool)
        .await
        .expect("count audit rows")
}

#[tokio::test]
async fn raising_the_credit_limit_lifts_the_mark_in_the_same_request() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    set_account(pool, w.tenant, "postpaid", -10_200, 10_000).await;
    // Settled at its limit: marked.
    let r =
        w.cp.send(
            "PATCH",
            &format!("/admin/tenants/{}/billing", w.tenant),
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny": "0.01"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["exhausted"], true, "{r:?}");
    assert!(balance_doc(pool, w.tenant)
        .await
        .is_some_and(|d| d.get("exhausted").is_some()));

    let r =
        w.cp.send(
            "PATCH",
            &format!("/admin/tenants/{}/billing", w.tenant),
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny": "1.00"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["exhausted"], false, "{r:?}");
    assert_eq!(r.body["credit_limit_micro_cny"], 1_000_000);
    assert_eq!(r.body["balance_micro_cny"], -10_200);
    assert_eq!(r.body["available_micro_cny"], 989_800);
    // Read right after the answer, without waiting for anything.
    let doc = balance_doc(pool, w.tenant)
        .await
        .expect("a balance document");
    assert!(doc.get("exhausted").is_none(), "{doc}");
    assert_eq!(audit_count(pool, w.tenant, "billing.credit_limit").await, 2);
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'billing.credit_limit' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool)
    .await
    .expect("the audit detail");
    assert_eq!(detail, json!({"credit_limit_micro_cny": 1_000_000}));
}

#[tokio::test]
async fn lowering_the_credit_limit_marks_the_tenant_in_the_same_request() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    set_account(pool, w.tenant, "postpaid", -500_000, 1_000_000).await;
    let path = format!("/admin/tenants/{}/billing", w.tenant);
    let patch = |limit: &'static str| {
        w.cp.send(
            "PATCH",
            &path,
            Some(w.admin.cookie()),
            Some(json!({ "credit_limit_cny": limit })),
        )
    };
    let r = patch("2.00").await;
    assert_eq!(
        (r.status, &r.body["exhausted"]),
        (200, &json!(false)),
        "{r:?}"
    );
    let doc = balance_doc(pool, w.tenant)
        .await
        .expect("a balance document");
    assert!(doc.get("exhausted").is_none(), "{doc}");
    let r = patch("0.50").await;
    assert_eq!(
        (r.status, &r.body["exhausted"]),
        (200, &json!(true)),
        "{r:?}"
    );
    let doc = balance_doc(pool, w.tenant)
        .await
        .expect("a balance document");
    assert!(doc.get("exhausted").is_some(), "{doc}");
}

#[tokio::test]
async fn a_credit_limit_is_dollars_and_cents() {
    let Some(w) = world().await else { return };
    let path = format!("/admin/tenants/{}/billing", w.tenant);
    for bad in ["-1", "1.234", "abc", "", "1e3", " 1"] {
        let r =
            w.cp.send(
                "PATCH",
                &path,
                Some(w.admin.cookie()),
                Some(json!({ "credit_limit_cny": bad })),
            )
            .await;
        assert_eq!(r.status, 422, "{bad:?}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], "/credit_limit_cny", "{bad:?}");
    }
    // A number is not dollars and cents either.
    let r =
        w.cp.send(
            "PATCH",
            &path,
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny": 5})),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(
        audit_count(w.cp.db().pool(), w.tenant, "billing.credit_limit").await,
        0
    );
    let r =
        w.cp.send(
            "PATCH",
            &path,
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny": "0"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["credit_limit_micro_cny"], 0);
}

#[tokio::test]
async fn a_credit_limit_of_an_unknown_tenant_is_not_found_and_a_pending_one_has_no_mark() {
    let Some(w) = world().await else { return };
    let r =
        w.cp.send(
            "PATCH",
            &format!("/admin/tenants/{}/billing", Uuid::new_v4()),
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny": "1"})),
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    let pending = w.cp.seed_tenant(TenantStatus::Pending).await;
    let r =
        w.cp.send(
            "PATCH",
            &format!("/admin/tenants/{pending}/billing"),
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny": "2.50"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["credit_limit_micro_cny"], 2_500_000);
    // Its approval writes its first mark; until then it has none.
    assert!(balance_doc(w.cp.db().pool(), pending).await.is_none());
}

#[tokio::test]
async fn the_account_list_puts_the_internal_tenant_last() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    // Created out of name order, so that creation order cannot pass.
    for name in ["Zeta Co", "Alpha Co"] {
        let r = cp
            .send(
                "POST",
                "/admin/tenants",
                Some(operator.cookie()),
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
    }
    let pending = cp.seed_tenant(TenantStatus::Pending).await;
    let r = cp
        .send("GET", "/admin/billing/accounts", Some(admin.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3, "{r:?}");
    assert_eq!(
        items.iter().map(|i| &i["tenant_name"]).collect::<Vec<_>>(),
        vec![
            &json!("Alpha Co"),
            &json!("Zeta Co"),
            &items[2]["tenant_name"]
        ]
    );
    assert_eq!(
        items.iter().map(|i| &i["is_internal"]).collect::<Vec<_>>(),
        vec![&json!(false), &json!(false), &json!(true)]
    );
    assert!(items
        .iter()
        .all(|i| i["tenant_id"] != json!(pending.to_string())));
    // A customer at 0 prepaid is blocked; the internal tenant never is.
    assert_eq!(items[0]["exhausted"], true);
    assert_eq!(items[2]["exhausted"], false);
    assert_eq!(items[0]["pending_topups"], 0);
    assert_eq!(items[0]["billing_mode"], "prepaid");
    assert_eq!(items[0]["status"], "active");
}

#[tokio::test]
async fn one_tenants_account_reads_as_the_list_does() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    set_account(pool, w.tenant, "postpaid", -250_000, 1_000_000).await;
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/tenants/{}/billing", w.tenant),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["tenant_id"], json!(w.tenant.to_string()));
    assert_eq!(r.body["billing_mode"], "postpaid");
    assert_eq!(r.body["balance_micro_cny"], -250_000);
    assert_eq!(r.body["available_micro_cny"], 750_000);
    assert_eq!(r.body["exhausted"], false);
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/tenants/{}/billing", Uuid::new_v4()),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn the_available_figure_adds_the_credit_limit_in_either_mode() {
    let Some(w) = world().await else { return };
    let pool = w.cp.db().pool();
    let path = format!("/admin/tenants/{}/billing", w.tenant);
    for mode in ["prepaid", "postpaid"] {
        set_account(pool, w.tenant, mode, -250_000, 1_000_000).await;
        let r = w.cp.send("GET", &path, Some(w.admin.cookie()), None).await;
        assert_eq!(r.status, 200, "{mode}: {r:?}");
        assert_eq!(r.body["billing_mode"], mode, "{r:?}");
        assert_eq!(r.body["balance_micro_cny"], -250_000, "{mode}: {r:?}");
        assert_eq!(r.body["available_micro_cny"], 750_000, "{mode}: {r:?}");
        assert_eq!(r.body["exhausted"], false, "{mode}: {r:?}");
    }
}

#[tokio::test]
async fn owners_admins_and_billing_members_read_the_balance() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    set_account(cp.db().pool(), tenant, "prepaid", 5_000_000, 0).await;
    for role in [MemberRole::Owner, MemberRole::Admin, MemberRole::Billing] {
        let session = cp.member_login_seeded(tenant, role).await;
        let r = cp
            .send("GET", "/api/billing/balance", Some(session.cookie()), None)
            .await;
        assert_eq!(r.status, 200, "{role:?}: {r:?}");
        assert_eq!(
            r.body,
            json!({
                "billing_mode": "prepaid",
                "balance_micro_cny": 5_000_000,
                "credit_limit_micro_cny": 0,
                "available_micro_cny": 5_000_000,
                "exhausted": false,
                "pending_topups": 0,
                "calculation": {
                    "topups_micro_cny": "0", "usage_micro_cny": "0", "adjustments_micro_cny": "0",
                    "balance_micro_cny": "5000000", "credit_limit_micro_cny": "0",
                    "available_micro_cny": "5000000", "available_overflow": false,
                    "last_ledger_seq": null, "as_of": r.body["calculation"]["as_of"],
                },
            }),
            "{role:?}"
        );
    }
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let r = cp
        .send("GET", "/api/billing/balance", Some(member.cookie()), None)
        .await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );

    let pending = cp.seed_tenant(TenantStatus::Pending).await;
    let owner = cp.member_login_seeded(pending, MemberRole::Owner).await;
    let r = cp
        .send("GET", "/api/billing/balance", Some(owner.cookie()), None)
        .await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("tenant_not_active")),
        "{r:?}"
    );
}

#[tokio::test]
async fn only_operators_change_a_credit_limit() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let path = format!("/admin/tenants/{tenant}/billing");
    let body = || Some(json!({"credit_limit_cny": "3.00"}));
    let r = cp.send("PATCH", &path, Some(viewer.cookie()), body()).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    assert_eq!(
        audit_count(cp.db().pool(), tenant, "billing.credit_limit").await,
        0
    );
    let r = cp
        .send("PATCH", &path, Some(operator.cookie()), body())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["credit_limit_micro_cny"], 3_000_000);
}

#[tokio::test]
async fn balance_calculation_is_an_exact_all_ledger_snapshot_and_stays_private() {
    use support::world::{deliver, price, seed_model, tokens_680, usage};
    let Some(w) = world().await else { return };
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let credited =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", w.tenant),
            Some(admin.cookie()),
            Some(json!({"amount_cny":"0.05", "reason":"验证余额公式"})),
        )
        .await;
    assert_eq!(credited.status, 201, "{credited:?}");
    let m = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, m).await;
    let delivered = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "balance1", m, tokens_680()),
            usage(&w, "balance2", m, tokens_680()),
        ],
    )
    .await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let credit =
        w.cp.send(
            "PATCH",
            &format!("/admin/tenants/{}/billing", w.tenant),
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_cny":"1.00"})),
        )
        .await;
    assert_eq!(credit.status, 200, "{credit:?}");
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let pending =
        w.cp.send(
            "POST",
            "/api/billing/topups",
            Some(owner.cookie()),
            Some(json!({"amount_cny":"2.00"})),
        )
        .await;
    assert_eq!(pending.status, 201, "{pending:?}");
    let r =
        w.cp.send("GET", "/api/billing/balance", Some(owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    let f = r.body["calculation"]
        .as_object()
        .expect("balance calculation must exist");
    for (key, expected) in [
        ("topups_micro_cny", "50000"),
        ("usage_micro_cny", "1360"),
        ("adjustments_micro_cny", "0"),
        ("balance_micro_cny", "48640"),
        ("credit_limit_micro_cny", "1000000"),
        ("available_micro_cny", "1048640"),
    ] {
        assert_eq!(f.get(key), Some(&json!(expected)), "{key}: {r:?}");
    }
    assert_eq!(r.body["pending_topups"], 1);
    let last: i64 =
        sqlx::query_scalar("SELECT max(seq) FROM billing_ledger_cny WHERE tenant_id = $1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("last sequence");
    assert_eq!(f.get("last_ledger_seq"), Some(&json!(last.to_string())));
    chrono::DateTime::parse_from_rfc3339(f["as_of"].as_str().expect("snapshot timestamp"))
        .expect("valid timestamp");
    let member = w.cp.member_login_seeded(w.tenant, MemberRole::Member).await;
    let denied =
        w.cp.send("GET", "/api/billing/balance", Some(member.cookie()), None)
            .await;
    assert_eq!(denied.status, 403, "{denied:?}");
    assert!(denied.body.get("calculation").is_none());
}

#[tokio::test]
async fn balance_calculation_keeps_credit_and_balance_above_javascript_integer_range() {
    let Some(w) = world().await else { return };
    set_account(
        w.cp.db().pool(),
        w.tenant,
        "postpaid",
        9_007_199_254_740_993,
        2,
    )
    .await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let r =
        w.cp.send("GET", "/api/billing/balance", Some(owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["calculation"]["balance_micro_cny"],
        "9007199254740993"
    );
    assert_eq!(
        r.body["calculation"]["available_micro_cny"],
        "9007199254740995"
    );
}

#[tokio::test]
async fn balance_read_returns_while_another_connection_holds_the_account_lock() {
    let Some(w) = world().await else { return };
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let mut held = w.cp.db().pool().begin().await.expect("begin holder");
    sqlx::query("SELECT tenant_id FROM billing_accounts WHERE tenant_id = $1 FOR NO KEY UPDATE")
        .bind(w.tenant)
        .fetch_one(&mut *held)
        .await
        .expect("hold the account lock");
    // 锁在收到并检查回答后才释放；超时或锁超时回答都不能算成功。
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        w.cp.send("GET", "/api/billing/balance", Some(owner.cookie()), None),
    )
    .await
    .expect("balance must answer before the holder releases its lock");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["calculation"]["balance_micro_cny"], "0");
    held.rollback().await.expect("release the account lock");
}

#[tokio::test]
async fn balance_gross_totals_can_exceed_i64_while_interleaved_postings_keep_the_net_valid() {
    use aisix_control_plane::billing::ledger::{self, Posting, Source};
    use aisix_control_plane::billing::topups::{self, Origin};
    use aisix_control_plane::tenants;
    use aisix_pricing::RateCard;
    use support::world::{db_now, deliver, group_id, insert_baseline, price, seed_model, usage};
    let Some(w) = world().await else { return };
    let m = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, m).await;
    let card: RateCard = serde_json::from_value(json!({
        "default": {"base": {"input": "1000000", "output": "0"}}
    }))
    .expect("large historical rate");
    card.validate().expect("valid complete gross-total fixture");
    insert_baseline(&w.cp, m, db_now(&w.cp).await, card)
        .await
        .expect("set the large rate");
    let rate = w.cp.send(
        "POST",
        &format!("/admin/provider-groups/{}/rates", group_id(&w.cp, "shared").await),
        Some(w.admin.cookie()),
        Some(json!({"model_id": null, "cny_per_usd": "100", "effective_from": db_now(&w.cp).await})),
    ).await;
    assert_eq!(rate.status, 201, "{rate:?}");
    // 合法上限：USD 1M/百万 × 100 元/USD × 4B token = 4e17 微元。
    // 30 次交错入账与扣费保持每步净额为零，累计收支仍各为 12e18。
    for index in 0..30 {
        let event = format!("gross-{index}");
        let mut tx = w.cp.db().pool().begin().await.expect("begin credit");
        tenants::lock_billing(&mut tx, w.tenant)
            .await
            .expect("billing lock");
        // 直接调用真实入账函数覆盖存储范围；单次网页入账上限不是累计范围。
        let credit = topups::insert_approved(
            &mut tx,
            w.tenant,
            400_000_000_000_000_000,
            Origin::Operator,
            None,
            Some("累计精度合同"),
            "",
            w.admin.id,
        )
        .await
        .expect("create the approved credit");
        ledger::post(
            &mut tx,
            &[Posting {
                tenant_id: w.tenant,
                amount_micro_cny: 400_000_000_000_000_000,
                source: Source::Topup {
                    topup_id: credit.id,
                },
            }],
        )
        .await
        .expect("post the credit");
        tx.commit().await.expect("commit the credit");
        let r = deliver(
            &w.dp,
            Uuid::new_v4(),
            vec![usage(
                &w,
                &event,
                m,
                json!({"prompt_tokens": 4_000_000_000u32, "is_final_attempt": true}),
            )],
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");
        let balance: i64 = sqlx::query_scalar(
            "SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id = $1",
        )
        .bind(w.tenant)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("net balance");
        assert_eq!(balance, 0, "each real debit offsets its preceding credit");
    }
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let r =
        w.cp.send("GET", "/api/billing/balance", Some(owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    let f = &r.body["calculation"];
    assert_eq!(f["topups_micro_cny"], "12000000000000000000");
    assert_eq!(f["usage_micro_cny"], "12000000000000000000");
    assert_eq!(f["adjustments_micro_cny"], "0");
    assert_eq!(f["balance_micro_cny"], "0");
    let gross = |key: &str| {
        f[key]
            .as_str()
            .expect("exact string")
            .parse::<i128>()
            .expect("integer")
    };
    assert_eq!(
        gross("topups_micro_cny") - gross("usage_micro_cny") + gross("adjustments_micro_cny"),
        gross("balance_micro_cny")
    );
}

#[tokio::test]
async fn balance_available_overflow_keeps_the_saturated_figure_and_fail_closed_flag() {
    let Some(w) = world().await else { return };
    set_account(w.cp.db().pool(), w.tenant, "prepaid", i64::MAX, 1).await;
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    let r =
        w.cp.send("GET", "/api/billing/balance", Some(owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["available_micro_cny"], i64::MAX);
    assert_eq!(r.body["exhausted"], true);
    let f = &r.body["calculation"];
    assert_eq!(f["balance_micro_cny"], i64::MAX.to_string());
    assert_eq!(f["credit_limit_micro_cny"], "1");
    assert_eq!(f["available_micro_cny"], i64::MAX.to_string());
    assert_eq!(f.get("available_overflow"), Some(&json!(true)));
}
