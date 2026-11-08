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
