//! Operator credits: an admin operator credits a tenant directly, with a
//! reason, as a top-up inserted already approved. The credit posts one
//! `topup` ledger entry, settles the balance mark and is audited, all in the
//! request's transaction, and the tenant reads it beside its own requests,
//! with its reason and without the operator.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::ledger::{self, Posting, Source};
use aisix_control_plane::billing::topups::{self, Origin};
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use sqlx::PgPool;
use support::members::MemberSession;
use support::world::{deliver, price, seed_key, seed_model, tokens_680, usage, World};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

const REASON: &str = "线下收款 4b7";

/// A control plane with an admin operator, an active prepaid customer
/// tenant at a balance of 0, and that tenant's owner, logged in.
struct Desk {
    cp: TestCp,
    admin: OperatorSession,
    tenant: Uuid,
    tenant_name: String,
    owner: MemberSession,
}

async fn desk() -> Option<Desk> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let tenant_name = format!("Credit Co {}", Uuid::new_v4().simple());
    let r = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(admin.cookie()),
            Some(json!({ "name": tenant_name })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let tenant = Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid");
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    Some(Desk {
        cp,
        admin,
        tenant,
        tenant_name,
        owner,
    })
}

/// `POST /admin/tenants/{tenant}/credits` with `body` as `op`.
async fn credit(cp: &TestCp, op: &OperatorSession, tenant: Uuid, body: Value) -> Resp {
    credit_at(cp, op, &tenant.to_string(), body).await
}

/// `POST /admin/tenants/{tenant}/credits` with `body` as `op`, for a tenant
/// id as it is written in the path.
async fn credit_at(cp: &TestCp, op: &OperatorSession, tenant: &str, body: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/tenants/{tenant}/credits"),
        Some(op.cookie()),
        Some(body),
    )
    .await
}

/// `tenant`'s ledger entries in posting order: `(kind, amount, balance
/// after, topup id)`.
async fn entries(pool: &PgPool, tenant: Uuid) -> Vec<(String, i64, i64, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT kind, amount_micro_cny, balance_after_micro_cny, topup_id FROM billing_ledger_cny \
         WHERE tenant_id = $1 ORDER BY seq",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
    .expect("read the ledger")
}

async fn balance(pool: &PgPool, tenant: Uuid) -> i64 {
    sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(pool)
        .await
        .expect("read the balance")
}

async fn topup_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM billing_topups_cny")
        .fetch_one(pool)
        .await
        .expect("count top-ups")
}

/// The stored, live balance document of `tenant`.
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

/// Every audit row of `action`: `(actor kind, actor id, tenant id, target
/// kind, target id, detail)`.
type AuditRow = (
    String,
    Option<Uuid>,
    Option<Uuid>,
    String,
    Option<String>,
    Value,
);

async fn audits(pool: &PgPool, action: &str) -> Vec<AuditRow> {
    sqlx::query_as(
        "SELECT actor_kind, actor_id, tenant_id, target_kind, target_id, detail FROM audit_log \
         WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .expect("read audit rows")
}

/// Sessions of this test's database waiting for a lock.
async fn lock_waiters(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(pool)
    .await
    .expect("count lock waiters")
}

async fn until_lock_waiters(pool: &PgPool, n: i64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while lock_waiters(pool).await < n {
        assert!(Instant::now() < deadline, "{n} lock waiters never appeared");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn an_operator_credit_needs_a_reason_and_posts_one_entry() {
    let Some(d) = desk().await else { return };
    let pool = d.cp.db().pool();
    let r = credit(&d.cp, &d.admin, d.tenant, json!({"amount_cny": "0.05"})).await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/reason", "{r:?}");
    assert!(entries(pool, d.tenant).await.is_empty());
    assert_eq!(topup_rows(pool).await, 0);
    assert!(audits(pool, "billing.topup.credit").await.is_empty());

    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": "0.05", "reason": REASON}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = r.body["id"].as_str().expect("an id").to_string();
    let topup = Uuid::parse_str(&id).expect("a uuid");
    let requested_at = r.body["requested_at"].clone();
    let decided_at = r.body["decided_at"].clone();
    assert!(requested_at.is_string(), "{r:?}");
    assert!(decided_at.is_string(), "{r:?}");
    assert_eq!(
        r.body,
        json!({
            "id": id,
            "origin": "operator",
            "amount_micro_cny": 50_000,
            "note": "",
            "reference": "",
            "status": "approved",
            "requested_by": null,
            "requested_at": requested_at,
            "decided_at": decided_at,
            "reject_reason": null,
            "credit_reason": REASON,
            "tenant_id": d.tenant,
            "tenant_name": d.tenant_name,
            "decided_by": {"id": d.admin.id, "email": d.admin.email},
        })
    );

    assert_eq!(
        entries(pool, d.tenant).await,
        vec![("topup".to_string(), 50_000, 50_000, Some(topup))]
    );
    assert_eq!(balance(pool, d.tenant).await, 50_000);
    assert_eq!(
        audits(pool, "billing.topup.credit").await,
        vec![(
            "operator".to_string(),
            Some(d.admin.id),
            Some(d.tenant),
            "topup".to_string(),
            Some(id.clone()),
            json!({"amount_micro_cny": 50_000, "reason": REASON}),
        )]
    );
    // The operators' queue lists it as an operator credit.
    let r =
        d.cp.send(
            "GET",
            "/admin/billing/topups?origin=operator",
            Some(d.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(1), "{r:?}");
    assert_eq!(r.body["items"][0]["id"], json!(id));
}

#[tokio::test]
async fn a_credit_reason_and_reference_are_bounded() {
    let Some(d) = desk().await else { return };
    let pool = d.cp.db().pool();
    for (body, path) in [
        (json!({"amount_cny": "1", "reason": ""}), "/reason"),
        (json!({"amount_cny": "1", "reason": "   "}), "/reason"),
        (
            json!({"amount_cny": "1", "reason": "因".repeat(501)}),
            "/reason",
        ),
        // A NUL, which Postgres cannot store, and a line break: refused as
        // input, not failed in the database.
        (
            json!({"amount_cny": "1", "reason": "线下\u{0}收款"}),
            "/reason",
        ),
        (
            json!({"amount_cny": "1", "reason": "线下\n收款"}),
            "/reason",
        ),
        (
            json!({"amount_cny": "1", "reason": REASON, "reference": "r".repeat(129)}),
            "/reference",
        ),
        (
            json!({"amount_cny": "1", "reason": REASON, "reference": "wire\n7c1"}),
            "/reference",
        ),
    ] {
        let r = credit(&d.cp, &d.admin, d.tenant, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{body}: {r:?}");
    }
    // Every problem of one body is reported at once.
    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": "0", "reference": "a\u{7}b"}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    let paths: Vec<&str> = r.body["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|d| d["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(paths, vec!["/amount_cny", "/reason", "/reference"], "{r:?}");
    // A field the route does not know is refused, not ignored.
    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": "1", "reason": REASON, "requested_by": d.owner.id}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert!(entries(pool, d.tenant).await.is_empty());
    assert_eq!(topup_rows(pool).await, 0);
    assert!(audits(pool, "billing.topup.credit").await.is_empty());

    // At the limits, counted in characters rather than bytes.
    let reason = "因".repeat(500);
    let reference = "r".repeat(128);
    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": "0.01", "reason": reason, "reference": reference}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["credit_reason"], json!(reason));
    assert_eq!(r.body["reference"], json!(reference));
    assert_eq!(
        audits(pool, "billing.topup.credit").await[0].5,
        json!({"amount_micro_cny": 10_000, "reason": reason})
    );
}

#[tokio::test]
async fn only_admins_post_credits() {
    let Some(d) = desk().await else { return };
    let pool = d.cp.db().pool();
    let body = json!({"amount_cny": "0.05", "reason": REASON});
    for role in [OperatorRole::Viewer, OperatorRole::Operator] {
        let op = d.cp.admin_login_seeded(role).await;
        let r = credit(&d.cp, &op, d.tenant, body.clone()).await;
        assert_eq!(
            (r.status, &r.body["code"]),
            (403, &json!("forbidden")),
            "{role:?}: {r:?}"
        );
    }
    let r =
        d.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", d.tenant),
            None,
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, 401, "{r:?}");
    // A tenant's own owner cannot credit its organization either.
    let r =
        d.cp.send(
            "POST",
            &format!("/admin/tenants/{}/credits", d.tenant),
            Some(d.owner.cookie()),
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, 401, "{r:?}");
    assert!(entries(pool, d.tenant).await.is_empty());
    assert_eq!(topup_rows(pool).await, 0);
    assert!(audits(pool, "billing.topup.credit").await.is_empty());

    let r = credit(&d.cp, &d.admin, d.tenant, body).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(entries(pool, d.tenant).await.len(), 1);
}

#[tokio::test]
async fn an_unknown_or_pending_tenant_takes_no_credit() {
    let Some(d) = desk().await else { return };
    let pool = d.cp.db().pool();
    let body = json!({"amount_cny": "0.05", "reason": REASON});
    let pending = d.cp.seed_tenant(TenantStatus::Pending).await;
    for tenant in [
        Uuid::new_v4().to_string(),
        "not-a-tenant".to_string(),
        pending.to_string(),
    ] {
        let r = credit_at(&d.cp, &d.admin, &tenant, body.clone()).await;
        assert_eq!(
            (r.status, &r.body["code"]),
            (404, &json!("not_found")),
            "{tenant}: {r:?}"
        );
    }
    assert!(entries(pool, pending).await.is_empty());
    assert_eq!(balance(pool, pending).await, 0);
    assert_eq!(topup_rows(pool).await, 0);
    assert!(audits(pool, "billing.topup.credit").await.is_empty());

    // A suspended tenant still takes one: an offline payment is recorded
    // whatever the tenant's state.
    let r =
        d.cp.send(
            "POST",
            &format!("/admin/tenants/{}/suspend", d.tenant),
            Some(d.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = credit(&d.cp, &d.admin, d.tenant, body).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(balance(pool, d.tenant).await, 50_000);
}

/// The balance document's `exhausted` mark, read until `want` holds; fails
/// after 10 s.
async fn until_exhausted_is(pool: &PgPool, tenant: Uuid, want: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let doc = balance_doc(pool, tenant).await;
        if doc
            .as_ref()
            .is_some_and(|d| d.get("exhausted").is_some() == want)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tenant {tenant}'s balance document is {doc:?}, not exhausted: {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_operator_credit_lifts_an_exhausted_postpaid_tenant_in_the_same_request() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let dp = cp.issue_dp_client("gw-credits-7c1").await;
    let r = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(admin.cookie()),
            Some(json!({
                "name": format!("Postpaid Credit Co {}", Uuid::new_v4().simple()),
                "billing_mode": "postpaid",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let tenant = Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid");
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&cp, &admin, tenant, Some(member)).await;
    let model = seed_model(&cp, &admin).await;
    price(&cp, model).await;
    let r = cp
        .send(
            "PATCH",
            &format!("/admin/tenants/{tenant}/billing"),
            Some(admin.cookie()),
            Some(json!({"credit_limit_cny": "0.01"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let w = World {
        cp,
        admin,
        dp,
        tenant,
        member,
        key,
    };
    let pool = w.cp.db().pool();

    // Fifteen rows of 680 µ$ spend 10 200 µ$, past the 10 000 µ$ limit.
    let rows: Vec<Value> = (1..=15)
        .map(|i| usage(&w, &format!("spent-{i}"), model, tokens_680()))
        .collect();
    let r = deliver(&w.dp, Uuid::new_v4(), rows).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(balance(pool, tenant).await, -10_200);
    until_exhausted_is(pool, tenant, true).await;

    let r = credit(
        &w.cp,
        &w.admin,
        tenant,
        json!({"amount_cny": "0.05", "reason": "线下收款 7c1"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    // Read right after the answer, without waiting for anything.
    let doc = balance_doc(pool, tenant).await.expect("a balance document");
    assert!(doc.get("exhausted").is_none(), "{doc}");
    let r =
        w.cp.send(
            "GET",
            &format!("/admin/tenants/{tenant}/billing"),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.body["balance_micro_cny"], 39_800, "{r:?}");
    assert_eq!(r.body["exhausted"], false, "{r:?}");
    let last = entries(pool, tenant).await.pop().expect("an entry");
    assert_eq!((last.0.as_str(), last.1, last.2), ("topup", 50_000, 39_800));
}

#[tokio::test]
async fn an_operator_credit_shows_to_the_tenant_with_its_reason() {
    let Some(d) = desk().await else { return };
    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": "0.05", "reason": REASON, "reference": "pay-4b7"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = r.body["id"].as_str().expect("an id").to_string();

    let r =
        d.cp.send("GET", "/api/billing/topups", Some(d.owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(1), "{r:?}");
    let item = &r.body["items"][0];
    assert_eq!(item["id"], json!(id));
    assert_eq!(item["origin"], "operator");
    assert_eq!(item["status"], "approved");
    assert_eq!(item["amount_micro_cny"], 50_000);
    assert_eq!(item["requested_by"], Value::Null);
    assert_eq!(item["credit_reason"], REASON);
    assert_eq!(item["reference"], "pay-4b7");
    for field in ["decided_by", "tenant_id", "tenant_name"] {
        assert!(item.get(field).is_none(), "{field}: {r:?}");
    }
    assert!(!r.body.to_string().contains(&d.admin.email), "{r:?}");

    let r =
        d.cp.send(
            "GET",
            &format!("/api/billing/topups/{id}"),
            Some(d.owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["credit_reason"], REASON);
    assert_eq!(r.body["requested_by"], Value::Null);
    assert!(!r.body.to_string().contains(&d.admin.email), "{r:?}");

    let r =
        d.cp.send("GET", "/api/billing/balance", Some(d.owner.cookie()), None)
            .await;
    assert_eq!(r.body["balance_micro_cny"], 50_000, "{r:?}");
    assert_eq!(r.body["exhausted"], false, "{r:?}");
}

#[tokio::test]
async fn a_credit_amount_is_yuan_and_fen_only() {
    let Some(d) = desk().await else { return };
    let pool = d.cp.db().pool();
    for bad in ["0", "0.001", "-1", "1e3", "100000000", "0.00", "", " 1"] {
        let r = credit(
            &d.cp,
            &d.admin,
            d.tenant,
            json!({"amount_cny": bad, "reason": REASON}),
        )
        .await;
        assert_eq!(r.status, 422, "{bad:?}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], "/amount_cny", "{bad:?}");
    }
    // A number rather than a dollar string is refused too.
    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": 5, "reason": REASON}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert!(entries(pool, d.tenant).await.is_empty());
    assert_eq!(topup_rows(pool).await, 0);
    assert!(audits(pool, "billing.topup.credit").await.is_empty());

    // The largest amount eight integer digits can type, ¥99,999,999.99.
    for (amount, micro) in [("0.01", 10_000_i64), ("99999999.99", 99_999_999_990_000)] {
        let r = credit(
            &d.cp,
            &d.admin,
            d.tenant,
            json!({"amount_cny": amount, "reason": REASON}),
        )
        .await;
        assert_eq!(r.status, 201, "{amount}: {r:?}");
        assert_eq!(r.body["amount_micro_cny"], micro, "{amount}");
    }
    assert_eq!(balance(pool, d.tenant).await, 100_000_000_000_000);
}

#[tokio::test]
async fn a_credit_does_not_count_as_a_pending_request() {
    let Some(d) = desk().await else { return };
    for _ in 0..20 {
        let r =
            d.cp.send(
                "POST",
                "/api/billing/topups",
                Some(d.owner.cookie()),
                Some(json!({"amount_cny": "0.01"})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
    }
    let r = credit(
        &d.cp,
        &d.admin,
        d.tenant,
        json!({"amount_cny": "0.05", "reason": REASON}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let r =
        d.cp.send("GET", "/api/billing/balance", Some(d.owner.cookie()), None)
            .await;
    assert_eq!(r.body["pending_topups"], 20, "{r:?}");
    assert_eq!(r.body["balance_micro_cny"], 50_000, "{r:?}");
    // And the tenant's own queue is still full.
    let r =
        d.cp.send(
            "POST",
            "/api/billing/topups",
            Some(d.owner.cookie()),
            Some(json!({"amount_cny": "0.01"})),
        )
        .await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (409, &json!("topup_limit")),
        "{r:?}"
    );
}

/// A credit posts under the tenant's billing lock. A transaction that has
/// moved the balance and still holds the account row (as any balance update
/// does) is waited for before the credit reads the balance, so the credit's
/// entry carries the balance after both. A foreign-key check alone would
/// not wait: it is compatible with that row lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_credit_waits_for_the_account_before_it_reads_the_balance() {
    let Some(d) = desk().await else { return };
    let cp = Arc::new(d.cp);
    let admin = Arc::new(d.admin);
    let pool = cp.db().pool().clone();

    let mut holder = pool.begin().await.expect("begin");
    let earlier = topups::insert_approved(
        &mut holder,
        d.tenant,
        10_000,
        Origin::Operator,
        None,
        Some("线下收款 7c1"),
        "",
        admin.id,
    )
    .await
    .expect("insert a credit");
    ledger::post(
        &mut holder,
        &[Posting {
            tenant_id: d.tenant,
            amount_micro_cny: 10_000,
            source: Source::Topup {
                topup_id: earlier.id,
            },
        }],
    )
    .await
    .expect("post the credit");

    let later = tokio::spawn({
        let (cp, admin, tenant) = (cp.clone(), admin.clone(), d.tenant);
        async move {
            credit(
                &cp,
                &admin,
                tenant,
                json!({"amount_cny": "0.05", "reason": REASON}),
            )
            .await
        }
    });
    until_lock_waiters(&pool, 1).await;
    holder.commit().await.expect("commit the earlier credit");
    let r = later.await.expect("join");
    assert_eq!(r.status, 201, "{r:?}");
    let topup = Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid");
    assert_eq!(
        entries(&pool, d.tenant).await,
        vec![
            ("topup".to_string(), 10_000, 10_000, Some(earlier.id)),
            ("topup".to_string(), 50_000, 60_000, Some(topup)),
        ]
    );
    assert_eq!(balance(&pool, d.tenant).await, 60_000);
}
