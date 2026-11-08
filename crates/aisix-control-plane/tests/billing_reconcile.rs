//! Reconciliation: `GET /admin/billing/reconciliation` shows, per tenant,
//! where its ledger disagrees with its account, its charges, its re-pricing
//! adjustments or its top-ups, and the first entry whose running balance
//! breaks; and, across the platform, the charged rows that have no entry,
//! the rows stored unpriced in the last day, and the usage events the
//! gateways heard from in the last day report they dropped.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it,
//! in rows of 680 µ$ at `card()` (`tokens_680`). Every drift is written by
//! raw SQL, as a bug or a hand edit would write it.

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::db::Db;
use aisix_control_plane::pricing::store::{self, NewDiscount};
use aisix_control_plane::tenants::TenantStatus;
use aisix_pricing::{RateCard, ENGINE_VERSION};
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{
    db_now, deliver, insert_sell, price, seed_key, seed_model, seed_tenant, tokens_680, usage,
    world, World,
};
use support::{Bundle, DpClient, OperatorSession, TestCp};
use uuid::Uuid;

const PATH: &str = "/admin/billing/reconciliation";

/// The reconciliation, for one tenant when `tenant` is given, as `by`; it
/// must answer 200.
async fn reconcile(cp: &TestCp, by: &OperatorSession, tenant: Option<Uuid>) -> Value {
    let path = match tenant {
        Some(tenant) => format!("{PATH}?tenant_id={tenant}"),
        None => PATH.to_string(),
    };
    let r = cp.get_with_cookie(&path, by.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body
}

/// A mismatch as `(tenant_id, check, expected, actual, seq)`.
type Row = (String, String, i64, i64, Value);

/// The answer's mismatches as [`Row`]s, sorted; each one's `tenant_name`
/// must be its tenant's.
async fn mismatches(db: &Db, body: &Value) -> Vec<Row> {
    let items = body["mismatches"]
        .as_array()
        .unwrap_or_else(|| panic!("mismatches in {body}"));
    let mut rows = Vec::with_capacity(items.len());
    for m in items {
        let tenant = m["tenant_id"].as_str().expect("a tenant id").to_string();
        let name: String = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1::uuid")
            .bind(&tenant)
            .fetch_one(db.pool())
            .await
            .expect("read the tenant's name");
        assert_eq!(m["tenant_name"], json!(name), "{m}");
        rows.push((
            tenant,
            m["check"].as_str().expect("a check").to_string(),
            m["expected_micro_usd"]
                .as_i64()
                .expect("an expected amount"),
            m["actual_micro_usd"].as_i64().expect("an actual amount"),
            m.get("seq").unwrap_or_else(|| panic!("seq in {m}")).clone(),
        ));
    }
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    rows
}

fn row(tenant: Uuid, check: &str, expected: i64, actual: i64, seq: Value) -> Row {
    (tenant.to_string(), check.to_string(), expected, actual, seq)
}

fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    rows
}

/// A tenant with one member and that member's key.
struct Payer {
    tenant: Uuid,
    member: Uuid,
    key: String,
}

async fn payer(w: &World) -> Payer {
    let tenant = seed_tenant(&w.cp, &w.admin).await;
    let member = w.cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&w.cp, &w.admin, tenant, Some(member)).await;
    Payer {
        tenant,
        member,
        key,
    }
}

/// `usage(w, …)`, but of `p`'s tenant, member and key.
fn usage_of(w: &World, p: &Payer, event_id: &str, model: Uuid, fields: Value) -> Value {
    let mut event = usage(w, event_id, model, fields);
    event["team_id"] = json!(p.tenant.to_string());
    event["api_key_id"] = json!(p.key);
    event["user_id"] = json!(p.member.to_string());
    event
}

async fn send(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// `card()` doubled: the 680 µ$ row costs 1360 µ$.
fn doubled() -> RateCard {
    serde_json::from_value(json!({
        "default": {"base": {"input": "2", "output": "4", "cache_read": "0.2"}}
    }))
    .expect("a card")
}

/// A 100 % discount for `tenant`, starting now.
async fn free_of_charge(cp: &TestCp, tenant: Uuid) {
    let discount = NewDiscount {
        tenant_id: tenant,
        effective_from: db_now(cp).await,
        discount_bps: 10_000,
    };
    cp.db()
        .tx(move |c| {
            async move { store::insert_discount(c, &discount, Actor::System).await }.boxed()
        })
        .await
        .expect("a 100 % discount");
}

/// An operator credit of `amount_usd` to `tenant` by `admin`.
async fn credit(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, amount_usd: &str) {
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/credits"),
            Some(admin.cookie()),
            Some(json!({"amount_usd": amount_usd, "reason": "线下收款 r23"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// A top-up of `amount_usd` requested by an owner of `tenant`; its id.
async fn request_topup(cp: &TestCp, tenant: Uuid, amount_usd: &str) -> Uuid {
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let r = cp
        .send(
            "POST",
            "/api/billing/topups",
            Some(owner.cookie()),
            Some(json!({ "amount_usd": amount_usd })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("an id")).expect("a uuid")
}

/// `(kind, amount)` of every entry of `tenant`, in order.
async fn entries(db: &Db, tenant: Uuid) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT kind, amount_micro_usd FROM billing_ledger WHERE tenant_id = $1 ORDER BY seq",
    )
    .bind(tenant)
    .fetch_all(db.pool())
    .await
    .expect("read the ledger")
}

async fn charge_status(db: &Db, event_id: &str) -> (String, Option<i64>) {
    sqlx::query_as("SELECT status, charge_micro_usd FROM pricing_charges WHERE event_id = $1")
        .bind(event_id)
        .fetch_one(db.pool())
        .await
        .expect("read the charge")
}

// ---- drifts, by raw SQL ----

/// Moves `tenant`'s account by `by` with no entry.
async fn bump_balance(db: &Db, tenant: Uuid, by: i64) {
    sqlx::query(
        "UPDATE billing_accounts SET balance_micro_usd = balance_micro_usd + $2 \
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .bind(by)
    .execute(db.pool())
    .await
    .expect("move the balance");
}

/// Enters `amount` of `kind` for `tenant` from `event_id` or `topup_id`,
/// with a `balance_after` of the last entry's plus `amount` plus `off_by`,
/// and moves the account by `amount`, so only `off_by` breaks the running
/// balance; the entry's `seq`.
async fn raw_entry(
    db: &Db,
    tenant: Uuid,
    kind: &str,
    amount: i64,
    event_id: Option<&str>,
    topup_id: Option<Uuid>,
    off_by: i64,
) -> i64 {
    let mut tx = db.pool().begin().await.expect("begin");
    let last: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT balance_after_micro_usd FROM billing_ledger \
                          WHERE tenant_id = $1 ORDER BY seq DESC LIMIT 1), 0)",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .expect("read the last balance");
    let seq: i64 = sqlx::query_scalar(
        "INSERT INTO billing_ledger \
         (tenant_id, kind, amount_micro_usd, balance_after_micro_usd, event_id, topup_id) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING seq",
    )
    .bind(tenant)
    .bind(kind)
    .bind(amount)
    .bind(last + amount + off_by)
    .bind(event_id)
    .bind(topup_id)
    .fetch_one(&mut *tx)
    .await
    .expect("enter the entry");
    sqlx::query(
        "UPDATE billing_accounts SET balance_micro_usd = balance_micro_usd + $2 \
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .bind(amount)
    .execute(&mut *tx)
    .await
    .expect("move the balance");
    tx.commit().await.expect("commit");
    seq
}

/// A usage row of `p` with a `charged` charge of `charge` and no entry.
async fn raw_charged_without_entry(db: &Db, p: &Payer, model: Uuid, event_id: &str, charge: i64) {
    let mut tx = db.pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event) \
         VALUES ($1, $2, $2, now(), $3::text, $4, jsonb_build_object('event_id', $1::text))",
    )
    .bind(event_id)
    .bind(Uuid::new_v4())
    .bind(p.tenant)
    .bind(&p.key)
    .execute(&mut *tx)
    .await
    .expect("insert the usage row");
    sqlx::query(
        "INSERT INTO pricing_charges \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, price_instant, \
          status, charge_micro_usd, engine_version) \
         VALUES ($1, $2::text, $3, $4::text, $5::text, now(), now(), 'charged', $6, $7)",
    )
    .bind(event_id)
    .bind(p.tenant)
    .bind(&p.key)
    .bind(p.member)
    .bind(model)
    .bind(charge)
    .bind(i64::from(ENGINE_VERSION))
    .execute(&mut *tx)
    .await
    .expect("insert its charge");
    tx.commit().await.expect("commit");
}

/// Sets the pending top-up `id` approved, with no entry.
async fn raw_approve(db: &Db, id: Uuid) {
    let n = sqlx::query(
        "UPDATE billing_topups SET status = 'approved', decided_at = now() \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .execute(db.pool())
    .await
    .expect("approve the top-up")
    .rows_affected();
    assert_eq!(n, 1);
}

/// An approved operator credit of `amount` to `tenant`, with no entry; its id.
async fn raw_credit(db: &Db, tenant: Uuid, amount: i64, by: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_topups \
         (id, tenant_id, amount_micro_usd, origin, status, decided_by, decided_at, credit_reason) \
         VALUES ($1, $2, $3, 'operator', 'approved', $4, now(), 'r23')",
    )
    .bind(id)
    .bind(tenant)
    .bind(amount)
    .bind(by)
    .execute(db.pool())
    .await
    .expect("insert the credit");
    id
}

/// An applied re-pricing run that changed nothing; its id.
async fn raw_run(db: &Db, by: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO billing_repricing_runs \
         (id, status, filter, reason, pricing_version, engine_version, events, changed, \
          still_unpriced, skipped_unattributed, delta_micro_usd, by_tenant, created_by, \
          applied_by, applied_at) \
         VALUES ($1, 'applied', '{}', 'r23', 0, $2, 0, 0, 0, 0, 0, '[]', $3, $3, now())",
    )
    .bind(id)
    .bind(i64::from(ENGINE_VERSION))
    .bind(by)
    .execute(db.pool())
    .await
    .expect("insert the run");
    id
}

/// An adjustment of `delta` to `event_id`'s charge in run `run`, with no
/// entry.
async fn raw_adjustment(db: &Db, run: Uuid, event_id: &str, delta: i64, by: Uuid) {
    sqlx::query(
        "INSERT INTO pricing_adjustments \
         (run_id, event_id, delta_micro_usd, engine_version, reason, created_by) \
         VALUES ($1, $2, $3, $4, 'r23', $5)",
    )
    .bind(run)
    .bind(event_id)
    .bind(delta)
    .bind(i64::from(ENGINE_VERSION))
    .bind(by)
    .execute(db.pool())
    .await
    .expect("insert the adjustment");
}

// ---- gateways ----

/// A heartbeat from `instance` of `bundle`'s credential reporting `drops`.
async fn beat(gw: &DpClient, bundle: &Bundle, instance: &str, drops: Value) {
    let r = gw
        .heartbeat(json!({
            "dp_id": bundle.dp_id,
            "instance_id": instance,
            "hostname": format!("host-{instance}"),
            "uptime_seconds": 30,
            "version": "1.9.0+sha-abc1234",
            "supported_guardrail_kinds": [],
            "applied_revision": 3,
            "exporter_health": [],
            "usage_event_drops": drops,
        }))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// The listed instances by `instance_id`.
fn instances(body: &Value) -> BTreeMap<String, Value> {
    body["usage_loss"]["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("usage_loss.instances in {body}"))
        .iter()
        .map(|i| {
            (
                i["instance_id"]
                    .as_str()
                    .expect("an instance id")
                    .to_string(),
                i.clone(),
            )
        })
        .collect()
}

fn time(v: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(v.as_str().unwrap_or_else(|| panic!("a time, not {v}")))
        .expect("an RFC 3339 time")
        .with_timezone(&Utc)
}

#[tokio::test]
async fn a_consistent_ledger_reconciles() {
    let Some(w) = world().await else { return };
    let (cp, db) = (&w.cp, w.cp.db());
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let model = seed_model(cp, &w.admin).await;
    price(cp, model).await;
    let unpriced = seed_model(cp, &w.admin).await;
    let free = payer(&w).await;
    free_of_charge(cp, free.tenant).await;
    send(
        &w,
        vec![
            usage(&w, "u1", model, tokens_680()),
            usage(&w, "u2", model, tokens_680()),
            usage(&w, "u3", model, tokens_680()),
            usage_of(&w, &free, "free1", model, tokens_680()),
            usage(&w, "unpriced1", unpriced, tokens_680()),
        ],
    )
    .await;
    assert_eq!(
        charge_status(db, "free1").await,
        ("charged".to_string(), Some(0))
    );

    let topup = request_topup(cp, w.tenant, "2.00").await;
    let r = cp
        .send(
            "POST",
            &format!("/admin/billing/topups/{topup}/approve"),
            Some(w.admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    credit(cp, &admin, w.tenant, "1.50").await;
    // Money that never came in, and a limit that moves no money: a rejected
    // top-up, one still pending, and a credit-limit change.
    let rejected = request_topup(cp, w.tenant, "4.00").await;
    let r = cp
        .send(
            "POST",
            &format!("/admin/billing/topups/{rejected}/reject"),
            Some(w.admin.cookie()),
            Some(json!({"reason": "未到账 r23"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    request_topup(cp, w.tenant, "8.00").await;
    let r = cp
        .send(
            "PATCH",
            &format!("/admin/tenants/{}/billing", w.tenant),
            Some(w.admin.cookie()),
            Some(json!({"credit_limit_usd": "5.00"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["credit_limit_micro_usd"], json!(5_000_000), "{r:?}");

    // A run at doubled prices charges each of the three rows 680 µ$ more.
    insert_sell(cp, model, None, db_now(cp).await, Some(doubled()))
        .await
        .expect("a new price");
    let to = db_now(cp).await;
    let r = cp
        .send(
            "POST",
            "/admin/billing/repricing",
            Some(w.admin.cookie()),
            Some(json!({
                "from": to - TimeDelta::hours(1),
                "to": to,
                "model_id": model,
                "reason": "补录价格 r23",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let run = r.body["id"].as_str().expect("a run id").to_string();
    let r = cp
        .send(
            "POST",
            &format!("/admin/billing/repricing/{run}/apply"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    // The ledger holds every kind of entry, so there is something to check.
    assert_eq!(
        entries(db, w.tenant).await,
        vec![
            ("usage".to_string(), -680),
            ("usage".to_string(), -680),
            ("usage".to_string(), -680),
            ("topup".to_string(), 2_000_000),
            ("topup".to_string(), 1_500_000),
            ("adjustment".to_string(), -2_040),
        ]
    );
    assert!(entries(db, free.tenant).await.is_empty());

    let body = reconcile(cp, &viewer, None).await;
    assert_eq!(body["tenant_id"], Value::Null, "{body}");
    assert_eq!(body["mismatches"], json!([]), "{body}");
    assert_eq!(body["charged_without_entry"], json!(0), "{body}");
    time(&body["checked_at"]);
    // The unpriced row, counted as the pricing status counts it.
    let status = cp
        .get_with_cookie("/admin/pricing/status", viewer.cookie())
        .await;
    assert_eq!(status.status, 200, "{status:?}");
    assert_eq!(body["unpriced_last_24h"]["total"], json!(1), "{body}");
    assert_eq!(
        body["unpriced_last_24h"], status.body["unpriced_last_24h"],
        "{body}"
    );

    let body = reconcile(cp, &viewer, Some(w.tenant)).await;
    assert_eq!(body["tenant_id"], json!(w.tenant.to_string()), "{body}");
    assert_eq!(body["mismatches"], json!([]), "{body}");
    assert_eq!(body["charged_without_entry"], json!(0), "{body}");
}

#[tokio::test]
async fn each_check_catches_its_own_drift() {
    let Some(w) = world().await else { return };
    let (cp, db) = (&w.cp, w.cp.db());
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let model = seed_model(cp, &w.admin).await;
    price(cp, model).await;

    let bal = seed_tenant(cp, &w.admin).await;
    credit(cp, &admin, bal, "1.00").await;
    let use_ = payer(&w).await;
    let cwe = payer(&w).await;
    let top = seed_tenant(cp, &w.admin).await;
    let topup = request_topup(cp, top, "3.00").await;
    let adj = payer(&w).await;
    let run_t = seed_tenant(cp, &w.admin).await;
    credit(cp, &admin, run_t, "1.00").await;
    let mut hit = tokens_680();
    hit["cache_status"] = json!("hit");
    send(
        &w,
        vec![
            usage(&w, "w1", model, tokens_680()),
            usage_of(&w, &use_, "use1", model, tokens_680()),
            usage_of(&w, &use_, "use-hit", model, hit),
            usage_of(&w, &cwe, "cwe1", model, tokens_680()),
            usage_of(&w, &adj, "adj1", model, tokens_680()),
        ],
    )
    .await;
    assert_eq!(
        charge_status(db, "use-hit").await,
        ("exempt".to_string(), Some(0))
    );
    // Before any drift, everything reconciles.
    let body = reconcile(cp, &viewer, None).await;
    assert_eq!(body["mismatches"], json!([]), "{body}");
    assert_eq!(body["charged_without_entry"], json!(0), "{body}");

    bump_balance(db, bal, 1).await;
    raw_entry(db, use_.tenant, "usage", -500, Some("use-hit"), None, 0).await;
    raw_charged_without_entry(db, &cwe, model, "cwe-raw", 500).await;
    raw_approve(db, topup).await;
    let run = raw_run(db, admin.id).await;
    raw_adjustment(db, run, "adj1", 100, admin.id).await;
    let credit_id = raw_credit(db, run_t, 250_000, admin.id).await;
    let seq = raw_entry(db, run_t, "topup", 250_000, None, Some(credit_id), 7).await;
    // An entry posted after it follows the account, so it breaks too: the
    // first break is the one reported.
    credit(cp, &admin, run_t, "0.50").await;

    let body = reconcile(cp, &viewer, None).await;
    assert_eq!(body["tenant_id"], Value::Null, "{body}");
    assert_eq!(
        mismatches(db, &body).await,
        sorted(vec![
            row(bal, "balance", 1_000_000, 1_000_001, Value::Null),
            row(use_.tenant, "usage", -680, -1_180, Value::Null),
            // The charged row with no entry is the tenant's too.
            row(cwe.tenant, "usage", -1_180, -680, Value::Null),
            row(top, "topups", 3_000_000, 0, Value::Null),
            row(adj.tenant, "adjustments", -100, 0, Value::Null),
            row(run_t, "running_balance", 1_250_000, 1_250_007, json!(seq)),
        ]),
        "{body}"
    );
    assert_eq!(body["charged_without_entry"], json!(1), "{body}");

    // One tenant's: only its own drift.
    let body = reconcile(cp, &viewer, Some(run_t)).await;
    assert_eq!(body["tenant_id"], json!(run_t.to_string()), "{body}");
    assert_eq!(
        mismatches(db, &body).await,
        vec![row(
            run_t,
            "running_balance",
            1_250_000,
            1_250_007,
            json!(seq)
        )],
        "{body}"
    );
    assert_eq!(body["charged_without_entry"], json!(0), "{body}");
    let body = reconcile(cp, &viewer, Some(cwe.tenant)).await;
    assert_eq!(
        mismatches(db, &body).await,
        vec![row(cwe.tenant, "usage", -1_180, -680, Value::Null)],
        "{body}"
    );
    assert_eq!(body["charged_without_entry"], json!(1), "{body}");

    // A tenant id that names no tenant has nothing to reconcile, which is
    // not the same as reconciling.
    let r = cp
        .get_with_cookie(
            &format!("{PATH}?tenant_id={}", Uuid::new_v4()),
            viewer.cookie(),
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    assert_eq!(r.body["code"], "not_found", "{r:?}");
    let r = cp
        .get_with_cookie(&format!("{PATH}?tenant_id=not-a-uuid"), viewer.cookie())
        .await;
    assert_eq!(r.status, 422, "{r:?}");
}

#[tokio::test]
async fn usage_loss_sums_the_drops_of_instances_heard_from_today() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let a = cp.issue_bundle(&admin, "gw-loss-a").await;
    let b = cp.issue_bundle(&admin, "gw-loss-b").await;
    let (gw_a, gw_b) = (cp.dp_client(&a), cp.dp_client(&b));
    let (one, two, old) = (
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    );
    beat(
        &gw_a,
        &a,
        &one,
        json!({"sink_full": 3, "send_unconfirmed": 2}),
    )
    .await;
    beat(&gw_b, &b, &two, json!({"send_failed": 1})).await;
    beat(
        &gw_a,
        &a,
        &old,
        json!({"sink_full": 100, "send_unconfirmed": 50}),
    )
    .await;
    sqlx::query(
        "UPDATE dp_instances SET last_seen_at = now() - interval '25 hours' \
         WHERE instance_id = $1",
    )
    .bind(&old)
    .execute(cp.db().pool())
    .await
    .expect("age the old instance");

    let body = reconcile(&cp, &viewer, None).await;
    let loss = &body["usage_loss"];
    assert_eq!(loss["dropped_total"], json!(4), "{body}");
    assert_eq!(loss["unconfirmed_total"], json!(2), "{body}");
    assert_eq!(
        time(&body["checked_at"]) - time(&loss["since"]),
        TimeDelta::hours(24),
        "{body}"
    );
    let listed = instances(&body);
    assert_eq!(
        listed.keys().collect::<Vec<_>>(),
        {
            let mut ids = vec![&one, &two];
            ids.sort();
            ids
        },
        "{body}"
    );
    let first = &listed[&one];
    assert_eq!(first["dp_id"], json!(a.dp_id), "{first}");
    assert_eq!(first["credential_name"], "gw-loss-a", "{first}");
    assert_eq!(first["hostname"], json!(format!("host-{one}")), "{first}");
    assert_eq!(
        first["drops"],
        json!({"sink_full": 3, "send_unconfirmed": 2}),
        "{first}"
    );
    time(&first["last_seen_at"]);
    let second = &listed[&two];
    assert_eq!(second["dp_id"], json!(b.dp_id), "{second}");
    assert_eq!(second["credential_name"], "gw-loss-b", "{second}");
    assert_eq!(second["drops"], json!({"send_failed": 1}), "{second}");
}

#[tokio::test]
async fn drop_counts_past_a_signed_64_bit_total_saturate() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let a = cp.issue_bundle(&admin, "gw-loss-max").await;
    let gw = cp.dp_client(&a);
    let (one, two) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
    // The gateway counts in u64, and one more drop on top of the largest
    // count leaves even a u64 sum behind.
    beat(
        &gw,
        &a,
        &one,
        json!({"sink_full": u64::MAX, "send_unconfirmed": 1}),
    )
    .await;
    beat(
        &gw,
        &a,
        &two,
        json!({"send_failed": 1, "send_unconfirmed": u64::MAX}),
    )
    .await;

    let body = reconcile(&cp, &viewer, None).await;
    let loss = &body["usage_loss"];
    assert_eq!(loss["dropped_total"], json!(i64::MAX), "{body}");
    assert_eq!(loss["unconfirmed_total"], json!(i64::MAX), "{body}");
    // Each instance's own counts are shown as reported.
    let listed = instances(&body);
    assert_eq!(
        listed[&one]["drops"],
        json!({"sink_full": u64::MAX, "send_unconfirmed": 1}),
        "{body}"
    );
    assert_eq!(
        listed[&two]["drops"],
        json!({"send_failed": 1, "send_unconfirmed": u64::MAX}),
        "{body}"
    );
}

#[tokio::test]
async fn only_operators_read_reconciliation() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    reconcile(&cp, &viewer, None).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    for cookie in [Some(owner.cookie()), None] {
        let r = cp.send("GET", PATH, cookie, None).await;
        assert_eq!(r.status, 401, "{r:?}");
        assert_eq!(r.body["code"], "unauthenticated", "{r:?}");
    }
}

#[tokio::test]
async fn a_reconciliation_past_its_time_bound_answers_503() {
    // A lock wait longer than the bound, so the bound is what ends it.
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE billing_ledger IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold the ledger");
    let started = Instant::now();
    let r = cp.get_with_cookie(PATH, viewer.cookie()).await;
    let elapsed = started.elapsed();
    holder.rollback().await.expect("release the ledger");
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.body["code"], "unavailable", "{r:?}");
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(30),
        "{elapsed:?}"
    );
    // Released, it answers.
    reconcile(&cp, &viewer, None).await;
}

#[tokio::test]
async fn the_time_bound_covers_the_whole_reconciliation_not_each_read() {
    // Lock waits longer than the bound, so the bound is what ends them.
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let pool = cp.db().pool().clone();
    // The gateway credentials, which only the usage loss reads, are held
    // throughout; the ledger, which the first checks read, for 6 s. The
    // timer is the scenario, a first read that takes 6 s, not a wait for a
    // condition.
    let mut credentials = pool.begin().await.expect("begin");
    sqlx::query("LOCK TABLE dp_credentials IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *credentials)
        .await
        .expect("hold the credentials");
    let mut ledger = pool.begin().await.expect("begin");
    sqlx::query("LOCK TABLE billing_ledger IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *ledger)
        .await
        .expect("hold the ledger");
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(6)).await;
        ledger.rollback().await.expect("release the ledger");
    });
    let started = Instant::now();
    let r = cp.get_with_cookie(PATH, viewer.cookie()).await;
    let elapsed = started.elapsed();
    release.await.expect("the release ran");
    credentials
        .rollback()
        .await
        .expect("release the credentials");
    // 6 s on the ledger leaves 4 s for the rest: 503 at about 10 s, not
    // after a fresh 10 s for the read that waits on the credentials.
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.body["code"], "unavailable", "{r:?}");
    assert!(
        elapsed >= Duration::from_secs(9) && elapsed < Duration::from_secs(12),
        "{elapsed:?}"
    );
    reconcile(&cp, &viewer, None).await;
}

#[tokio::test]
async fn a_lower_configured_statement_timeout_still_binds_a_reconciliation() {
    // An operator's 2 s statement timeout, under the reconciliation's own
    // bound, with lock waits longer than both.
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.db_statement_timeout_ms = 2_000;
        cfg.db_lock_timeout_ms = 60_000;
    })
    .await
    else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let mut ledger = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE billing_ledger IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *ledger)
        .await
        .expect("hold the ledger");
    let started = Instant::now();
    let r = cp.get_with_cookie(PATH, viewer.cookie()).await;
    let elapsed = started.elapsed();
    ledger.rollback().await.expect("release the ledger");
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.body["code"], "unavailable", "{r:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}
