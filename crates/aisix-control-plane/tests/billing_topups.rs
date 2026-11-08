//! Top-ups: a tenant's owner, admin or billing member requests one, and an
//! operator approves it into the ledger, settling the balance mark in the
//! same transaction, or rejects it with a reason. Every step is audited, and
//! a top-up is decided once, however many approvals race for it.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::ledger::{self, Posting, Source};
use aisix_control_plane::billing::topups::{self, Origin};
use aisix_control_plane::error::ApiError;
use aisix_control_plane::tenants::{self, TenantStatus};
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use support::members::MemberSession;
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// A control plane with an operator, an active prepaid customer tenant at a
/// balance of 0 (so blocked), and that tenant's owner, logged in.
struct Shop {
    cp: TestCp,
    op: OperatorSession,
    tenant: Uuid,
    tenant_name: String,
    owner: MemberSession,
}

const OWNER_NAME: &str = "Wei Zhang";

async fn shop() -> Option<Shop> {
    let cp = TestCp::start().await?;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant_name = format!("Topup Co {}", Uuid::new_v4().simple());
    let (tenant, owner) = customer(&cp, &op, &tenant_name).await;
    Some(Shop {
        cp,
        op,
        tenant,
        tenant_name,
        owner,
    })
}

/// A tenant created by `op` as operators create one, and its owner, named
/// [`OWNER_NAME`], logged in.
async fn customer(cp: &TestCp, op: &OperatorSession, name: &str) -> (Uuid, MemberSession) {
    let r = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(op.cookie()),
            Some(json!({ "name": name })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let tenant = Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid");
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    sqlx::query("UPDATE members SET display_name = $2 WHERE id = $1")
        .bind(owner.id)
        .bind(OWNER_NAME)
        .execute(cp.db().pool())
        .await
        .expect("name the owner");
    (tenant, owner)
}

/// `POST /api/billing/topups` with `body` as `member`.
async fn request(cp: &TestCp, member: &MemberSession, body: Value) -> Resp {
    cp.send(
        "POST",
        "/api/billing/topups",
        Some(member.cookie()),
        Some(body),
    )
    .await
}

/// Requests `amount_cny` as `member`, which must answer 201; the top-up's id.
async fn requested(cp: &TestCp, member: &MemberSession, amount_cny: &str) -> String {
    let r = request(cp, member, json!({ "amount_cny": amount_cny })).await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body["id"].as_str().expect("an id").to_string()
}

async fn approve(cp: &TestCp, op: &OperatorSession, id: &str) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/billing/topups/{id}/approve"),
        Some(op.cookie()),
        Some(json!({})),
    )
    .await
}

async fn reject(cp: &TestCp, op: &OperatorSession, id: &str, body: Value) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/billing/topups/{id}/reject"),
        Some(op.cookie()),
        Some(body),
    )
    .await
}

/// `GET path` with `cookie`.
async fn get(cp: &TestCp, cookie: &str, path: &str) -> Resp {
    cp.send("GET", path, Some(cookie), None).await
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

/// The audit rows of `action` for `tenant`: `(actor kind, actor id, target
/// id, detail)`.
async fn audits(
    pool: &PgPool,
    tenant: Uuid,
    action: &str,
) -> Vec<(String, Option<Uuid>, Option<String>, Value)> {
    sqlx::query_as(
        "SELECT actor_kind, actor_id, target_id, detail FROM audit_log \
         WHERE action = $1 AND tenant_id = $2 ORDER BY id",
    )
    .bind(action)
    .bind(tenant)
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
async fn a_tenant_requests_a_top_up_and_both_sides_see_it_pending() {
    let Some(s) = shop().await else { return };
    let pool = s.cp.db().pool();
    let r = request(
        &s.cp,
        &s.owner,
        json!({"amount_cny": "0.05", "reference": "wire-7c1"}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = r.body["id"].as_str().expect("an id").to_string();
    let requested_at = r.body["requested_at"].clone();
    assert!(requested_at.is_string(), "{r:?}");
    assert_eq!(
        r.body,
        json!({
            "id": id,
            "origin": "tenant",
            "amount_micro_cny": 50_000,
            "note": "",
            "reference": "wire-7c1",
            "status": "pending",
            "requested_by": {"id": s.owner.id, "display_name": OWNER_NAME},
            "requested_at": requested_at,
            "decided_at": null,
            "reject_reason": null,
            "credit_reason": null,
        })
    );

    let r =
        s.cp.send("GET", "/api/billing/topups", Some(s.owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(1), "{r:?}");
    assert_eq!(r.body["items"][0]["id"], json!(id));
    assert_eq!(r.body["items"][0]["status"], "pending");
    assert_eq!(r.body["next_before"], Value::Null);

    let r =
        s.cp.send(
            "GET",
            "/admin/billing/topups?status=pending",
            Some(s.op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let item = r.body["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["id"] == json!(id)))
        .unwrap_or_else(|| panic!("the request is listed: {r:?}"))
        .clone();
    assert_eq!(item["tenant_id"], json!(s.tenant));
    assert_eq!(item["tenant_name"], json!(s.tenant_name));
    assert_eq!(item["status"], "pending");
    assert_eq!(item["amount_micro_cny"], 50_000);
    assert_eq!(item["decided_by"], Value::Null);
    assert_eq!(
        item["requested_by"],
        json!({"id": s.owner.id, "display_name": OWNER_NAME})
    );

    assert_eq!(
        audits(pool, s.tenant, "billing.topup.request").await,
        vec![(
            "member".to_string(),
            Some(s.owner.id),
            Some(id.clone()),
            json!({"amount_micro_cny": 50_000})
        )]
    );
    // Requesting moves no money.
    assert!(entries(pool, s.tenant).await.is_empty());
}

#[tokio::test]
async fn approving_credits_the_balance_once_in_the_same_transaction() {
    let Some(s) = shop().await else { return };
    let pool = s.cp.db().pool();
    let id = requested(&s.cp, &s.owner, "0.05").await;
    let topup = Uuid::parse_str(&id).expect("a uuid");

    let r = approve(&s.cp, &s.op, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["id"], json!(id));
    assert_eq!(r.body["status"], "approved");
    assert_eq!(r.body["tenant_id"], json!(s.tenant));
    assert_eq!(r.body["tenant_name"], json!(s.tenant_name));
    assert_eq!(
        r.body["decided_by"],
        json!({"id": s.op.id, "email": s.op.email})
    );
    assert!(r.body["decided_at"].is_string(), "{r:?}");
    assert_eq!(r.body["reject_reason"], Value::Null);

    assert_eq!(
        entries(pool, s.tenant).await,
        vec![("topup".to_string(), 50_000, 50_000, Some(topup))]
    );
    assert_eq!(balance(pool, s.tenant).await, 50_000);
    let r =
        s.cp.send(
            "GET",
            &format!("/admin/tenants/{}/billing", s.tenant),
            Some(s.op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.body["balance_micro_cny"], 50_000, "{r:?}");
    assert_eq!(r.body["exhausted"], false, "{r:?}");
    assert_eq!(
        audits(pool, s.tenant, "billing.topup.approve").await,
        vec![(
            "operator".to_string(),
            Some(s.op.id),
            Some(id.clone()),
            json!({"amount_micro_cny": 50_000})
        )]
    );

    // The tenant sees the decision, and no operator.
    let r =
        s.cp.send(
            "GET",
            &format!("/api/billing/topups/{id}"),
            Some(s.owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["status"], "approved");
    assert!(r.body["decided_at"].is_string(), "{r:?}");
    for field in ["decided_by", "tenant_id", "tenant_name"] {
        assert!(r.body.get(field).is_none(), "{field}: {r:?}");
    }
    assert!(!r.body.to_string().contains(&s.op.email), "{r:?}");
}

/// Review focus 3: a double click, two operators, or two control-plane
/// instances approving the same top-up credit the tenant once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approving_twice_or_racing_two_approvals_credits_once() {
    let Some(s) = shop().await else { return };
    let cp = Arc::new(s.cp);
    let other = Arc::new(cp.restart().await);
    let pool = cp.db().pool().clone();
    let second_op = Arc::new(other.admin_login_seeded(OperatorRole::Operator).await);
    let op = Arc::new(s.op);
    let id = requested(&cp, &s.owner, "0.05").await;
    let topup = Uuid::parse_str(&id).expect("a uuid");

    // Both approvals are in flight at once: each waits behind a holder of
    // the tenant's row, and both are let go together.
    let mut holder = pool.begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(s.tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the tenant");
    let first = tokio::spawn({
        let (cp, op, id) = (cp.clone(), op.clone(), id.clone());
        async move { approve(&cp, &op, &id).await }
    });
    let second = tokio::spawn({
        let (other, op, id) = (other.clone(), second_op.clone(), id.clone());
        async move { approve(&other, &op, &id).await }
    });
    until_lock_waiters(&pool, 2).await;
    holder.rollback().await.expect("release the tenant");
    let mut answers = vec![first.await.expect("join"), second.await.expect("join")];
    answers.sort_by_key(|r| r.status);
    assert_eq!(
        answers.iter().map(|r| r.status).collect::<Vec<_>>(),
        vec![200, 409],
        "{answers:?}"
    );
    assert_eq!(answers[1].body["code"], "topup_state", "{answers:?}");
    assert_eq!(
        entries(&pool, s.tenant).await,
        vec![("topup".to_string(), 50_000, 50_000, Some(topup))]
    );
    assert_eq!(balance(&pool, s.tenant).await, 50_000);
    assert_eq!(
        audits(&pool, s.tenant, "billing.topup.approve").await.len(),
        1
    );

    // A third approval, after the fact.
    let r = approve(&cp, &op, &id).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (409, &json!("topup_state")),
        "{r:?}"
    );

    // A double click: two approvals from one instance, nothing held.
    let id = requested(&cp, &s.owner, "0.03").await;
    let (a, b) = tokio::join!(approve(&cp, &op, &id), approve(&cp, &op, &id));
    let mut statuses = vec![a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, vec![200, 409], "{a:?} {b:?}");
    assert_eq!(entries(&pool, s.tenant).await.len(), 2);
    assert_eq!(balance(&pool, s.tenant).await, 80_000);
}

#[tokio::test]
async fn rejecting_needs_a_reason_and_credits_nothing() {
    let Some(s) = shop().await else { return };
    let pool = s.cp.db().pool();
    let id = requested(&s.cp, &s.owner, "0.05").await;
    for body in [
        json!({}),
        json!({"reason": ""}),
        json!({"reason": "   "}),
        json!({"reason": "因".repeat(501)}),
        // A NUL, which Postgres cannot store, and a line break: refused as
        // input, not failed in the database.
        json!({"reason": "未收到\u{0}款项"}),
        json!({"reason": "未收到\n款项"}),
    ] {
        let r = reject(&s.cp, &s.op, &id, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], "/reason", "{body}: {r:?}");
    }
    assert!(audits(pool, s.tenant, "billing.topup.reject")
        .await
        .is_empty());

    let r = reject(&s.cp, &s.op, &id, json!({"reason": "未收到款项"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["status"], "rejected");
    assert_eq!(r.body["reject_reason"], "未收到款项");
    assert_eq!(
        r.body["decided_by"],
        json!({"id": s.op.id, "email": s.op.email})
    );

    let r =
        s.cp.send(
            "GET",
            &format!("/api/billing/topups/{id}"),
            Some(s.owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["status"], "rejected");
    assert_eq!(r.body["reject_reason"], "未收到款项");
    assert!(r.body["decided_at"].is_string(), "{r:?}");

    assert!(entries(pool, s.tenant).await.is_empty());
    assert_eq!(balance(pool, s.tenant).await, 0);
    assert_eq!(
        audits(pool, s.tenant, "billing.topup.reject").await,
        vec![(
            "operator".to_string(),
            Some(s.op.id),
            Some(id.clone()),
            json!({"reason": "未收到款项"})
        )]
    );

    // Decided once: neither an approval nor another rejection takes it.
    let r = approve(&s.cp, &s.op, &id).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (409, &json!("topup_state")),
        "{r:?}"
    );
    let r = reject(&s.cp, &s.op, &id, json!({"reason": "再次驳回"})).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (409, &json!("topup_state")),
        "{r:?}"
    );
    assert!(entries(pool, s.tenant).await.is_empty());
    assert_eq!(balance(pool, s.tenant).await, 0);
}

#[tokio::test]
async fn an_approval_that_lifts_the_floor_clears_the_mark_in_the_same_request() {
    let Some(s) = shop().await else { return };
    let pool = s.cp.db().pool();
    // A prepaid tenant at 0 is marked from its creation.
    let doc = balance_doc(pool, s.tenant)
        .await
        .expect("a balance document");
    assert!(doc.get("exhausted").is_some(), "{doc}");

    let id = requested(&s.cp, &s.owner, "0.05").await;
    let r = approve(&s.cp, &s.op, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    // Read right after the answer, without waiting for anything.
    let doc = balance_doc(pool, s.tenant)
        .await
        .expect("a balance document");
    assert!(doc.get("exhausted").is_none(), "{doc}");
}

#[tokio::test]
async fn a_top_up_amount_is_dollars_and_cents_only() {
    let Some(s) = shop().await else { return };
    let pool = s.cp.db().pool();
    for bad in ["0", "0.00", "0.001", "-1", "1e3", "1000000.01", "", " 1"] {
        let r = request(&s.cp, &s.owner, json!({ "amount_cny": bad })).await;
        assert_eq!(r.status, 422, "{bad:?}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], "/amount_cny", "{bad:?}");
    }
    // A note and a reference are bounded and printable.
    for (body, path) in [
        (
            json!({"amount_cny": "1", "note": "备".repeat(501)}),
            "/note",
        ),
        (json!({"amount_cny": "1", "note": "bell\u{7}"}), "/note"),
        (
            json!({"amount_cny": "1", "reference": "r".repeat(129)}),
            "/reference",
        ),
        (
            json!({"amount_cny": "1", "reference": "wire\n7c1"}),
            "/reference",
        ),
    ] {
        let r = request(&s.cp, &s.owner, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{body}");
    }
    assert!(audits(pool, s.tenant, "billing.topup.request")
        .await
        .is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM billing_topups")
            .fetch_one(pool)
            .await
            .expect("count top-ups"),
        0
    );

    let r = request(&s.cp, &s.owner, json!({"amount_cny": "1000000.00"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["amount_micro_cny"], 1_000_000_000_000_i64);
    // At the limits, counted in characters rather than bytes.
    let note = "备".repeat(500);
    let reference = "r".repeat(128);
    let r = request(
        &s.cp,
        &s.owner,
        json!({"amount_cny": "0.01", "note": note, "reference": reference}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["note"], json!(note));
    assert_eq!(r.body["reference"], json!(reference));
    assert_eq!(r.body["amount_micro_cny"], 10_000);
}

#[tokio::test]
async fn another_tenants_top_up_id_is_not_found() {
    let Some(s) = shop().await else { return };
    let (_, other_owner) = customer(&s.cp, &s.op, "Other Topup Co").await;
    let theirs = requested(&s.cp, &other_owner, "0.05").await;
    let ours = requested(&s.cp, &s.owner, "0.02").await;

    let get_one = |id: String| {
        let (cp, owner) = (&s.cp, &s.owner);
        async move { get(cp, owner.cookie(), &format!("/api/billing/topups/{id}")).await }
    };
    let foreign = get_one(theirs.clone()).await;
    let random = get_one(Uuid::new_v4().to_string()).await;
    let malformed = get_one("not-a-uuid".to_string()).await;
    assert_eq!(foreign.status, 404, "{foreign:?}");
    assert_eq!(
        (foreign.status, &foreign.body),
        (random.status, &random.body)
    );
    assert_eq!(
        (foreign.status, &foreign.body),
        (malformed.status, &malformed.body)
    );
    assert_eq!(get_one(ours.clone()).await.status, 200);

    let r =
        s.cp.send("GET", "/api/billing/topups", Some(s.owner.cookie()), None)
            .await;
    let ids: Vec<&Value> = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| &i["id"])
        .collect();
    assert_eq!(ids, vec![&json!(ours)], "{r:?}");
}

#[tokio::test]
async fn a_tenant_holds_at_most_twenty_pending_requests() {
    let Some(s) = shop().await else { return };
    let pool = s.cp.db().pool();
    let mut first = None;
    for _ in 0..20 {
        let id = requested(&s.cp, &s.owner, "0.01").await;
        first.get_or_insert(id);
    }
    let r = request(&s.cp, &s.owner, json!({"amount_cny": "0.01"})).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (409, &json!("topup_limit")),
        "{r:?}"
    );
    assert_eq!(
        audits(pool, s.tenant, "billing.topup.request").await.len(),
        20
    );

    let r = approve(&s.cp, &s.op, &first.expect("a first request")).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = request(&s.cp, &s.owner, json!({"amount_cny": "0.01"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = request(&s.cp, &s.owner, json!({"amount_cny": "0.01"})).await;
    assert_eq!(r.status, 409, "{r:?}");
}

#[tokio::test]
async fn members_cannot_request_or_read_top_ups() {
    let Some(s) = shop().await else { return };
    let id = requested(&s.cp, &s.owner, "0.05").await;
    let member = s.cp.member_login_seeded(s.tenant, MemberRole::Member).await;
    let billing =
        s.cp.member_login_seeded(s.tenant, MemberRole::Billing)
            .await;

    let r = request(&s.cp, &member, json!({"amount_cny": "0.05"})).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    for path in [
        "/api/billing/topups".to_string(),
        format!("/api/billing/topups/{id}"),
    ] {
        let r = s.cp.send("GET", &path, Some(member.cookie()), None).await;
        assert_eq!(
            (r.status, &r.body["code"]),
            (403, &json!("forbidden")),
            "{path}: {r:?}"
        );
        let r = s.cp.send("GET", &path, Some(billing.cookie()), None).await;
        assert_eq!(r.status, 200, "{path}: {r:?}");
    }
    let r = request(&s.cp, &billing, json!({"amount_cny": "0.05"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["requested_by"]["id"], json!(billing.id));

    // A tenant that is not active requests nothing.
    let pending = s.cp.seed_tenant(TenantStatus::Pending).await;
    let owner = s.cp.member_login_seeded(pending, MemberRole::Owner).await;
    let r = request(&s.cp, &owner, json!({"amount_cny": "0.05"})).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("tenant_not_active")),
        "{r:?}"
    );
}

/// Requires that tenant and operator both read `n` pending top-ups on the
/// account.
async fn pending_on_both_sides(s: &Shop, n: i64) {
    let r = get(&s.cp, s.owner.cookie(), "/api/billing/balance").await;
    assert_eq!(r.body["pending_topups"], n, "{r:?}");
    let path = format!("/admin/tenants/{}/billing", s.tenant);
    let r = get(&s.cp, s.op.cookie(), &path).await;
    assert_eq!(r.body["pending_topups"], n, "{r:?}");
    let r = get(&s.cp, s.op.cookie(), "/admin/billing/accounts").await;
    let account = r.body["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["tenant_id"] == json!(s.tenant)))
        .unwrap_or_else(|| panic!("the tenant is listed: {r:?}"));
    assert_eq!(account["pending_topups"], n, "{r:?}");
}

#[tokio::test]
async fn a_pending_top_up_shows_on_the_balance() {
    let Some(s) = shop().await else { return };
    pending_on_both_sides(&s, 0).await;
    let id = requested(&s.cp, &s.owner, "0.05").await;
    pending_on_both_sides(&s, 1).await;
    let r = approve(&s.cp, &s.op, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    pending_on_both_sides(&s, 0).await;
    let id = requested(&s.cp, &s.owner, "0.05").await;
    pending_on_both_sides(&s, 1).await;
    let r = reject(&s.cp, &s.op, &id, json!({"reason": "金额不符"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    pending_on_both_sides(&s, 0).await;
}

/// An approval posts under the tenant's billing lock: when a posting that
/// holds the account (as usage intake does) commits first, the approval's
/// entry carries the balance after it, and the account the sum of both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_waits_for_the_account_before_it_reads_the_balance() {
    let Some(s) = shop().await else { return };
    let cp = Arc::new(s.cp);
    let op = Arc::new(s.op);
    let pool = cp.db().pool().clone();
    let id = requested(&cp, &s.owner, "0.05").await;
    let topup = Uuid::parse_str(&id).expect("a uuid");

    // The account only, not the tenant row, and an entry posted under it.
    let mut holder = pool.begin().await.expect("begin");
    ledger::lock_account(&mut holder, s.tenant)
        .await
        .expect("hold the account");
    let credit = topups::insert_approved(
        &mut holder,
        s.tenant,
        10_000,
        Origin::Operator,
        None,
        Some("线下收款 7c1"),
        "",
        op.id,
    )
    .await
    .expect("insert a credit");
    ledger::post(
        &mut holder,
        &[Posting {
            tenant_id: s.tenant,
            amount_micro_cny: 10_000,
            source: Source::Topup {
                topup_id: credit.id,
            },
        }],
    )
    .await
    .expect("post the credit");

    let approval = tokio::spawn({
        let (cp, op, id) = (cp.clone(), op.clone(), id.clone());
        async move { approve(&cp, &op, &id).await }
    });
    until_lock_waiters(&pool, 1).await;
    holder.commit().await.expect("commit the credit");
    let r = approval.await.expect("join");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&pool, s.tenant).await,
        vec![
            ("topup".to_string(), 10_000, 10_000, Some(credit.id)),
            ("topup".to_string(), 50_000, 60_000, Some(topup)),
        ]
    );
    assert_eq!(balance(&pool, s.tenant).await, 60_000);
}

/// An operator credit reaches the tenant's list with its reason and without
/// the operator, beside the tenant's own requests.
#[tokio::test]
async fn an_operator_credit_shows_to_the_tenant_with_its_reason_and_no_operator() {
    let Some(s) = shop().await else { return };
    let (tenant, op_id) = (s.tenant, s.op.id);
    let credit =
        s.cp.db()
            .tx(move |conn| {
                async move {
                    tenants::lock_billing(&mut *conn, tenant).await?;
                    let row = topups::insert_approved(
                        &mut *conn,
                        tenant,
                        50_000,
                        Origin::Operator,
                        None,
                        Some("线下收款 4b7"),
                        "pay-4b7",
                        op_id,
                    )
                    .await?;
                    ledger::post(
                        &mut *conn,
                        &[Posting {
                            tenant_id: tenant,
                            amount_micro_cny: 50_000,
                            source: Source::Topup { topup_id: row.id },
                        }],
                    )
                    .await?;
                    Ok::<_, ApiError>(row)
                }
                .boxed()
            })
            .await
            .expect("credit the tenant");
    let admin = topups::to_admin(&credit);
    assert_eq!(admin["origin"], "operator");
    assert_eq!(admin["status"], "approved");
    assert_eq!(admin["credit_reason"], "线下收款 4b7");
    assert_eq!(
        admin["decided_by"],
        json!({"id": op_id, "email": s.op.email})
    );
    assert_eq!(admin["requested_by"], Value::Null);

    let r =
        s.cp.send("GET", "/api/billing/topups", Some(s.owner.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    let item = &r.body["items"][0];
    assert_eq!(item["id"], json!(credit.id));
    assert_eq!(item["origin"], "operator");
    assert_eq!(item["requested_by"], Value::Null);
    assert_eq!(item["credit_reason"], "线下收款 4b7");
    assert_eq!(item["reference"], "pay-4b7");
    assert_eq!(item["status"], "approved");
    assert!(item.get("decided_by").is_none(), "{r:?}");
    assert!(!r.body.to_string().contains(&s.op.email), "{r:?}");
    // A credit is not a request awaiting a decision.
    let r =
        s.cp.send("GET", "/api/billing/balance", Some(s.owner.cookie()), None)
            .await;
    assert_eq!(r.body["pending_topups"], 0, "{r:?}");
    assert_eq!(r.body["balance_micro_cny"], 50_000, "{r:?}");
}

#[tokio::test]
async fn the_lists_page_newest_first_and_filter() {
    let Some(s) = shop().await else { return };
    let (other, other_owner) = customer(&s.cp, &s.op, "Paging Topup Co").await;
    let a = requested(&s.cp, &s.owner, "0.01").await;
    let b = requested(&s.cp, &s.owner, "0.02").await;
    let c = requested(&s.cp, &s.owner, "0.03").await;
    let theirs = requested(&s.cp, &other_owner, "0.04").await;
    let r = approve(&s.cp, &s.op, &b).await;
    assert_eq!(r.status, 200, "{r:?}");

    let ids = |r: &Resp| -> Vec<String> {
        r.body["items"]
            .as_array()
            .unwrap_or_else(|| panic!("items: {r:?}"))
            .iter()
            .map(|i| i["id"].as_str().expect("an id").to_string())
            .collect()
    };
    let r = get(&s.cp, s.owner.cookie(), "/api/billing/topups?limit=2").await;
    assert_eq!(ids(&r), vec![c.clone(), b.clone()], "{r:?}");
    let before = r.body["next_before"].as_i64().expect("another page");
    let r = get(
        &s.cp,
        s.owner.cookie(),
        &format!("/api/billing/topups?limit=2&before={before}"),
    )
    .await;
    assert_eq!(ids(&r), vec![a.clone()], "{r:?}");
    assert_eq!(r.body["next_before"], Value::Null);
    let r = get(
        &s.cp,
        s.owner.cookie(),
        "/api/billing/topups?status=pending",
    )
    .await;
    assert_eq!(ids(&r), vec![c.clone(), a.clone()], "{r:?}");
    let r = get(&s.cp, s.owner.cookie(), "/api/billing/topups?status=done").await;
    assert_eq!(r.status, 422, "{r:?}");

    let r = get(&s.cp, s.op.cookie(), "/admin/billing/topups").await;
    assert_eq!(
        ids(&r),
        vec![theirs.clone(), c.clone(), b.clone(), a.clone()]
    );
    let r = get(
        &s.cp,
        s.op.cookie(),
        &format!("/admin/billing/topups?tenant_id={other}"),
    )
    .await;
    assert_eq!(ids(&r), vec![theirs.clone()], "{r:?}");
    let r = get(
        &s.cp,
        s.op.cookie(),
        "/admin/billing/topups?status=approved",
    )
    .await;
    assert_eq!(ids(&r), vec![b.clone()], "{r:?}");
    let path = format!(
        "/admin/billing/topups?status=pending&tenant_id={}&limit=1",
        s.tenant
    );
    let r = get(&s.cp, s.op.cookie(), &path).await;
    assert_eq!(ids(&r), vec![c.clone()], "{r:?}");
    assert!(r.body["next_before"].is_i64(), "{r:?}");
    let r = get(
        &s.cp,
        s.op.cookie(),
        "/admin/billing/topups?origin=operator",
    )
    .await;
    assert_eq!(ids(&r), Vec::<String>::new(), "{r:?}");
    let r = get(&s.cp, s.op.cookie(), "/admin/billing/topups?origin=tenant").await;
    assert_eq!(ids(&r).len(), 4, "{r:?}");
    for bad in ["status=done", "origin=bank", "tenant_id=nope"] {
        let r = get(
            &s.cp,
            s.op.cookie(),
            &format!("/admin/billing/topups?{bad}"),
        )
        .await;
        assert_eq!(r.status, 422, "{bad}: {r:?}");
    }
}

#[tokio::test]
async fn only_operators_decide_top_ups() {
    let Some(s) = shop().await else { return };
    let viewer = s.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let id = requested(&s.cp, &s.owner, "0.05").await;
    let r = approve(&s.cp, &viewer, &id).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    let r = reject(&s.cp, &viewer, &id, json!({"reason": "未收到款项"})).await;
    assert_eq!(
        (r.status, &r.body["code"]),
        (403, &json!("forbidden")),
        "{r:?}"
    );
    let r =
        s.cp.send("GET", "/admin/billing/topups", Some(viewer.cookie()), None)
            .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(entries(s.cp.db().pool(), s.tenant).await.is_empty());
    let r = approve(&s.cp, &s.op, &Uuid::new_v4().to_string()).await;
    assert_eq!(r.status, 404, "{r:?}");
    let r = approve(&s.cp, &s.op, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
}
