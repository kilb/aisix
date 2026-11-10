//! Re-pricing: an operator previews a run over past charges at the prices in
//! force now, which posts nothing; an admin applies it, which records one
//! adjustment per changed charge, posts one `adjustment` ledger entry per
//! tenant and counts the corrections against cumulative quotas, in one
//! transaction, then settles each tenant's marks before it answers, and
//! refuses a stale preview.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it, in
//! rows of 680 µ$ at `card()` (`tokens_680`), except where a test needs more
//! rows than a gateway delivers quickly, which it writes by raw SQL as intake
//! stores a row on a model without a price.

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::budgets::{self, BudgetRow, BudgetScope, Kind, NewBudget, SetBy};
use aisix_control_plane::billing::ledger::{self, Posting, Source};
use aisix_control_plane::billing::repricing::{MAX_REPRICE_EVENTS, MAX_REPRICE_TENANTS};
use aisix_control_plane::db::Db;
use aisix_control_plane::pricing::store::{self, NewDiscount};
use aisix_control_plane::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use aisix_control_plane::tenants;
use aisix_core::models::BudgetPeriod;
use aisix_pricing::RateCard;
use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{
    db_now, deliver, group_id, insert_baseline, insert_unpriced_charges, price, seed_key,
    seed_key_in_group, seed_model, seed_tenant, tokens_680, usage, world, World, SHARED_GROUP,
};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// A world with an unpriced model (priced first when `priced`), and an admin
/// operator beside the world's operator.
struct R {
    w: World,
    model: Uuid,
    admin: OperatorSession,
}

async fn setup(priced: bool) -> Option<R> {
    let w = world().await?;
    let model = seed_model(&w.cp, &w.admin).await;
    if priced {
        price(&w.cp, model).await;
    }
    let admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    Some(R { w, model, admin })
}

/// `card()` scaled: USD per million for input, output and cache reads. The
/// 680 µ$ row costs 200 × input + 800 × cache + 200 × output.
fn card(input: &str, output: &str, cache_read: &str) -> RateCard {
    serde_json::from_value(json!({
        "default": {"base": {"input": input, "output": output, "cache_read": cache_read}}
    }))
    .expect("a card")
}

/// A new platform price for `model`, starting now.
async fn reprice_model(cp: &TestCp, model: Uuid, card: RateCard) {
    let now = db_now(cp).await;
    insert_baseline(cp, model, now, card)
        .await
        .expect("a new price");
}

/// The last hour, ending at the database's now.
async fn last_hour(cp: &TestCp) -> (DateTime<Utc>, DateTime<Utc>) {
    let to = db_now(cp).await;
    (to - TimeDelta::hours(1), to)
}

/// A run's body over `[from, to)`, with `extra` on top.
fn run_body(from: DateTime<Utc>, to: DateTime<Utc>, extra: Value) -> Value {
    let mut body = json!({"from": from, "to": to, "reason": "补录漏配的价格 7c1"});
    for (k, v) in extra.as_object().expect("an object") {
        body[k] = v.clone();
    }
    body
}

async fn preview(cp: &TestCp, by: &OperatorSession, body: Value) -> Resp {
    cp.send(
        "POST",
        "/admin/billing/repricing",
        Some(by.cookie()),
        Some(body),
    )
    .await
}

/// A preview that must answer 201; the run's id.
async fn previewed(cp: &TestCp, by: &OperatorSession, body: Value) -> (String, Value) {
    let r = preview(cp, by, body).await;
    assert_eq!(r.status, 201, "{r:?}");
    (r.body["id"].as_str().expect("a run id").to_string(), r.body)
}

async fn apply(cp: &TestCp, by: &OperatorSession, id: &str) -> Resp {
    cp.send(
        "POST",
        &format!("/admin/billing/repricing/{id}/apply"),
        Some(by.cookie()),
        Some(json!({})),
    )
    .await
}

async fn balance(db: &Db, tenant: Uuid) -> i64 {
    sqlx::query_scalar("SELECT balance_micro_cny FROM billing_accounts WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(db.pool())
        .await
        .expect("read the balance")
}

/// Tenant `tenant`'s ledger: `(kind, amount, balance after, run)`, in order.
async fn entries(db: &Db, tenant: Uuid) -> Vec<(String, i64, i64, Option<Uuid>)> {
    sqlx::query_as(
        "SELECT kind, amount_micro_cny, balance_after_micro_cny, repricing_run_id \
         FROM billing_ledger_cny WHERE tenant_id = $1 ORDER BY seq",
    )
    .bind(tenant)
    .fetch_all(db.pool())
    .await
    .expect("read the ledger")
}

/// Every adjustment: `(run, event, delta)`, by event.
async fn adjustments(db: &Db) -> Vec<(Uuid, String, i64)> {
    sqlx::query_as(
        "SELECT run_id, event_id, delta_micro_cny FROM pricing_adjustments_cny \
         ORDER BY event_id, created_at",
    )
    .fetch_all(db.pool())
    .await
    .expect("read the adjustments")
}

/// A charge's `(status, error_code, tenant_id)`.
async fn charge(db: &Db, event_id: &str) -> (String, Option<String>, String) {
    sqlx::query_as(
        "SELECT status, error_code, tenant_id FROM pricing_charges_cny WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(db.pool())
    .await
    .expect("read the charge")
}

async fn send(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// Three rows of 680 µ$ on `model`, `<prefix>1` to `<prefix>3`.
async fn three_rows(w: &World, prefix: &str, model: Uuid) {
    send(
        w,
        (1..=3)
            .map(|n| usage(w, &format!("{prefix}{n}"), model, tokens_680()))
            .collect(),
    )
    .await;
}

fn uuid(v: &Value) -> Uuid {
    Uuid::parse_str(v.as_str().expect("a uuid string")).expect("a uuid")
}

async fn lock_waiters(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pg_stat_activity")
}

/// Returns once `n` backends wait for a lock.
async fn until_lock_waiters(cp: &TestCp, n: i64) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while lock_waiters(cp).await < n {
        assert!(Instant::now() < deadline, "{n} lock waiters never appeared");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn unpriced_rows_are_charged_by_a_run_at_the_current_price() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "up", r.model).await;
    for n in 1..=3 {
        let (status, code, _) = charge(db, &format!("up{n}")).await;
        assert_eq!(
            (status.as_str(), code.as_deref()),
            ("unpriced", Some("missing_price"))
        );
    }
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(
        cp,
        &r.w.admin,
        run_body(from, to, json!({"model_id": r.model})),
    )
    .await;
    assert_eq!(run["status"], "previewed");
    assert_eq!(run["events"], 3);
    assert_eq!(run["changed"], 3);
    assert_eq!(run["still_unpriced"], 0);
    assert_eq!(run["skipped_unattributed"], 0);
    assert_eq!(run["delta_micro_cny"], 2_040);
    assert_eq!(run["by_tenant"].as_array().expect("by tenant").len(), 1);
    assert_eq!(uuid(&run["by_tenant"][0]["tenant_id"]), r.w.tenant);
    assert_eq!(run["by_tenant"][0]["events"], 3);
    assert_eq!(run["by_tenant"][0]["changed"], 3);
    assert_eq!(run["by_tenant"][0]["delta_micro_cny"], 2_040);
    assert_eq!(run["filter"]["model_id"], r.model.to_string());
    assert_eq!(run["created_by"]["email"], r.w.admin.email);
    assert!(run["applied_at"].is_null());

    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    assert_eq!(applied.body["status"], "applied");
    assert_eq!(applied.body["applied_by"]["email"], r.admin.email);
    assert!(applied.body["applied_at"].is_string());
    let run_id = Uuid::parse_str(&id).expect("a uuid");
    assert_eq!(
        adjustments(db).await,
        (1..=3)
            .map(|n| (run_id, format!("up{n}"), 680))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        entries(db, r.w.tenant).await,
        vec![("adjustment".to_string(), -2_040, -2_040, Some(run_id))]
    );
    assert_eq!(balance(db, r.w.tenant).await, -2_040);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action IN ('billing.repricing.preview', \
         'billing.repricing.apply') AND target_id = $1",
    )
    .bind(&id)
    .fetch_one(db.pool())
    .await
    .expect("count audit rows");
    assert_eq!(audited, 2);
}

#[tokio::test]
async fn a_preview_changes_nothing() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "pv", r.model).await;
    price(cp, r.model).await;
    let before = balance(db, r.w.tenant).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["changed"], 3);
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant).await.is_empty());
    assert_eq!(balance(db, r.w.tenant).await, before);
    let r2 = cp
        .send(
            "GET",
            &format!("/admin/billing/repricing/{id}"),
            Some(r.w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r2.status, 200, "{r2:?}");
    assert_eq!(r2.body["status"], "previewed");
    assert_eq!(r2.body["delta_micro_cny"], 2_040);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn applying_twice_is_refused_and_posts_once() {
    let Some(r) = setup(false).await else { return };
    three_rows(&r.w, "tw", r.model).await;
    price(&r.w.cp, r.model).await;
    let (from, to) = last_hour(&r.w.cp).await;
    let (id, _) = previewed(&r.w.cp, &r.w.admin, run_body(from, to, json!({}))).await;
    let tenant = r.w.tenant;
    let cp = Arc::new(r.w.cp);
    let other = Arc::new(cp.restart().await);
    let admin = Arc::new(r.admin);
    let second_admin = Arc::new(other.admin_login_seeded(OperatorRole::Admin).await);

    // Both applies are in flight at once, on two control-plane instances:
    // each waits behind a holder of the tenant's row, and both are let go
    // together.
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the tenant");
    let first = tokio::spawn({
        let (cp, admin, id) = (cp.clone(), admin.clone(), id.clone());
        async move { apply(&cp, &admin, &id).await }
    });
    let second = tokio::spawn({
        let (other, admin, id) = (other.clone(), second_admin.clone(), id.clone());
        async move { apply(&other, &admin, &id).await }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release the tenant");
    let mut answers = vec![first.await.expect("join"), second.await.expect("join")];
    answers.sort_by_key(|a| a.status);
    assert_eq!(
        answers.iter().map(|a| a.status).collect::<Vec<_>>(),
        vec![200, 409],
        "{answers:?}"
    );
    assert_eq!(answers[1].body["code"], "run_state", "{answers:?}");
    let db = cp.db();
    assert_eq!(entries(db, tenant).await.len(), 1);
    assert_eq!(adjustments(db).await.len(), 3);
    assert_eq!(balance(db, tenant).await, -2_040);

    // A later apply, after the fact.
    let later = apply(&cp, &admin, &id).await;
    assert_eq!(
        (later.status, &later.body["code"]),
        (409, &json!("run_state")),
        "{later:?}"
    );
    assert_eq!(entries(db, tenant).await.len(), 1);
}

/// Intake posts to the account while an apply waits for it: the apply
/// takes the account before it reads the balance, so every entry's balance
/// after is the running sum, and the account ends at the sum of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apply_racing_intake_keeps_the_running_balance_exact() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "rc", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    // A charge outside the run's window, for intake's debit to name.
    let key = Uuid::parse_str(&r.w.key).expect("a key id");
    let long_ago = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("a time");
    insert_unpriced_charges(db, "rc-debit", 1, r.w.tenant, key, r.model, long_ago).await;

    // Intake, mid-batch: the account locked and a debit posted.
    let mut intake = db.pool().begin().await.expect("begin");
    ledger::lock_account(&mut intake, r.w.tenant)
        .await
        .expect("lock the account");
    ledger::post(
        &mut intake,
        &[Posting {
            tenant_id: r.w.tenant,
            amount_micro_cny: -500,
            source: Source::Usage {
                event_id: "rc-debit-1",
            },
        }],
    )
    .await
    .expect("post the debit");
    // The apply is sent while the batch holds the account, which commits
    // once the apply waits.
    let release = async {
        until_lock_waiters(cp, 1).await;
        intake.commit().await.expect("commit the batch");
    };
    let (applied, ()) = tokio::join!(apply(cp, &r.admin, &id), release);
    assert_eq!(applied.status, 200, "{applied:?}");

    let ledger = entries(db, r.w.tenant).await;
    assert_eq!(ledger.len(), 2, "{ledger:?}");
    let mut running = 0;
    for (kind, amount, after, _) in &ledger {
        running += amount;
        assert_eq!(*after, running, "{kind}: {ledger:?}");
    }
    assert_eq!(balance(db, r.w.tenant).await, -2_540);
}

#[tokio::test]
async fn a_price_written_after_the_preview_makes_apply_stale() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "st", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    // Any price write moves the price version, whichever model it prices.
    let unrelated = seed_model(cp, &r.w.admin).await;
    price(cp, unrelated).await;
    let refused = apply(cp, &r.admin, &id).await;
    assert_eq!(
        (refused.status, &refused.body["code"]),
        (409, &json!("stale_preview")),
        "{refused:?}"
    );
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant).await.is_empty());
    assert_eq!(balance(db, r.w.tenant).await, 0);
    let run = cp
        .send(
            "GET",
            &format!("/admin/billing/repricing/{id}"),
            Some(r.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(run.body["status"], "previewed", "{run:?}");

    // A fresh preview, at the prices now in force, applies.
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    assert_eq!(balance(db, r.w.tenant).await, -2_040);
}

#[tokio::test]
async fn a_row_arriving_after_the_preview_makes_apply_stale() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "la", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    // A gateway delivers late a row that occurred inside the window.
    let late = (to - TimeDelta::minutes(5)).to_rfc3339();
    send(
        &r.w,
        vec![usage(
            &r.w,
            "la-late",
            r.model,
            json!({"occurred_at": late, "prompt_tokens": 1000, "completion_tokens": 200}),
        )],
    )
    .await;
    let refused = apply(cp, &r.admin, &id).await;
    assert_eq!(
        (refused.status, &refused.body["code"]),
        (409, &json!("stale_preview")),
        "{refused:?}"
    );
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant)
        .await
        .iter()
        .all(|e| e.0 != "adjustment"));
}

#[tokio::test]
async fn a_tenant_mismatch_row_is_never_charged_by_a_run() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let a = r.w.tenant;
    let b = seed_tenant(cp, &r.w.admin).await;
    let b_key = seed_key(cp, &r.w.admin, b, None).await;
    let now = db_now(cp).await;
    cp.db()
        .tx(move |c| {
            async move {
                store::insert_discount(
                    c,
                    &NewDiscount {
                        tenant_id: a,
                        effective_from: now,
                        discount_bps: 5_000,
                    },
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("discount A");
    // A's team on B's key.
    let mut row = usage(&r.w, "mm1", r.model, tokens_680());
    row["api_key_id"] = json!(b_key);
    send(&r.w, vec![row]).await;
    let (status, code, filed) = charge(db, "mm1").await;
    assert_eq!(
        (status.as_str(), code.as_deref(), filed),
        ("unpriced", Some("tenant_mismatch"), b.to_string())
    );
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(
        cp,
        &r.w.admin,
        run_body(from, to, json!({"status": "unpriced"})),
    )
    .await;
    assert_eq!(run["events"], 1);
    assert_eq!(run["skipped_unattributed"], 1);
    assert_eq!(run["changed"], 0);
    assert_eq!(run["delta_micro_cny"], 0);
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, a).await.is_empty());
    assert!(entries(db, b).await.is_empty());
}

#[tokio::test]
async fn a_run_prices_at_the_charges_own_tenant_plan() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let b = seed_tenant(cp, &r.w.admin).await;
    let b_key = seed_key(cp, &r.w.admin, b, None).await;
    let now = db_now(cp).await;
    cp.db()
        .tx(move |c| {
            async move {
                store::insert_discount(
                    c,
                    &NewDiscount {
                        tenant_id: b,
                        effective_from: now,
                        discount_bps: 2_000,
                    },
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("discount B");
    let mut row = usage(&r.w, "own1", r.model, tokens_680());
    row["team_id"] = json!(b.to_string());
    row["api_key_id"] = json!(b_key);
    row["user_id"] = json!("");
    send(&r.w, vec![row]).await;
    let (status, code, filed) = charge(db, "own1").await;
    assert_eq!(
        (status.as_str(), code.as_deref(), filed),
        ("unpriced", Some("missing_price"), b.to_string())
    );
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    // 680 µ$ with B's 20 % off.
    assert_eq!(run["delta_micro_cny"], 544);
    assert_eq!(uuid(&run["by_tenant"][0]["tenant_id"]), b);
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    assert_eq!(balance(db, b).await, -544);
    assert!(entries(db, r.w.tenant).await.is_empty());
}

#[tokio::test]
async fn a_lower_price_credits_the_difference() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "lo", r.model).await;
    assert_eq!(balance(db, r.w.tenant).await, -2_040);
    // Half the price: 340 µ$ a row.
    reprice_model(cp, r.model, card("0.5", "1", "0.05")).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["changed"], 3);
    assert_eq!(run["delta_micro_cny"], -1_020);
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    let run_id = Uuid::parse_str(&id).expect("a uuid");
    let ledger = entries(db, r.w.tenant).await;
    assert_eq!(
        ledger.last(),
        Some(&("adjustment".to_string(), 1_020, -1_020, Some(run_id)))
    );
    assert_eq!(balance(db, r.w.tenant).await, -1_020);
}

/// Quota `id`'s spend now.
async fn spent(cp: &TestCp, tenant: Uuid, id: Uuid) -> i64 {
    let mut conn = cp.db().pool().acquire().await.expect("a connection");
    let now = budgets::db_now(&mut conn).await.expect("now");
    budgets::list(&mut conn, tenant, now)
        .await
        .expect("list the quotas")
        .into_iter()
        .find(|(b, _)| b.id == id)
        .map(|(_, spent)| spent)
        .expect("the quota")
}

async fn create_quota(cp: &TestCp, tenant: Uuid, new: NewBudget) -> BudgetRow {
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                budgets::create(conn, &ctx, &row, new, Actor::System).await
            }
            .boxed()
        })
        .await
        .expect("create the quota")
}

/// Quota `id`'s stored document.
async fn quota_doc(db: &Db, id: Uuid) -> Value {
    sqlx::query_scalar("SELECT doc FROM gw_resources WHERE kind = 'budgets' AND id = $1")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("read the quota document")
}

#[tokio::test]
async fn a_run_moves_a_cumulative_quota_and_not_a_periodic_one() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let key = Uuid::parse_str(&r.w.key).expect("a key id");
    let on_key = |kind: Kind, period: Option<BudgetPeriod>, limit: i64| NewBudget {
        scope: BudgetScope::ApiKey,
        scope_ref: key,
        set_by: SetBy::OrgAdmin,
        kind,
        period,
        limit_micro_cny: limit,
        created_by_kind: "system",
        created_by: None,
    };
    let cumulative = create_quota(cp, r.w.tenant, on_key(Kind::Cumulative, None, 3_000)).await;
    let periodic = create_quota(
        cp,
        r.w.tenant,
        on_key(Kind::Periodic, Some(BudgetPeriod::Day), 100_000),
    )
    .await;
    three_rows(&r.w, "q", r.model).await;
    assert_eq!(spent(cp, r.w.tenant, cumulative.id).await, 2_040);
    assert_eq!(spent(cp, r.w.tenant, periodic.id).await, 2_040);
    assert!(quota_doc(db, cumulative.id)
        .await
        .get("exhausted")
        .is_none());

    // Twice the price: each row rises by 680.
    reprice_model(cp, r.model, card("2", "4", "0.2")).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["delta_micro_cny"], 2_040);
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    assert_eq!(spent(cp, r.w.tenant, cumulative.id).await, 4_080);
    assert_eq!(spent(cp, r.w.tenant, periodic.id).await, 2_040);
    // Settled before the answer: the cumulative quota is now used up.
    assert!(
        quota_doc(db, cumulative.id)
            .await
            .get("exhausted")
            .is_some(),
        "{}",
        quota_doc(db, cumulative.id).await
    );
    assert!(quota_doc(db, periodic.id).await.get("exhausted").is_none());
}

#[tokio::test]
async fn a_row_the_engine_still_refuses_is_left_as_it_is() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    send(
        &r.w,
        vec![usage(
            &r.w,
            "bad1",
            r.model,
            json!({"prompt_tokens": 1000, "cached_prompt_tokens": 1200, "completion_tokens": 200}),
        )],
    )
    .await;
    let (status, code, _) = charge(db, "bad1").await;
    assert_eq!(
        (status.as_str(), code.as_deref()),
        ("unpriced", Some("inconsistent_usage"))
    );
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["events"], 1);
    assert_eq!(run["still_unpriced"], 1);
    assert_eq!(run["changed"], 0);
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant).await.is_empty());
}

#[tokio::test]
async fn a_second_run_starts_from_the_adjusted_charge() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    send(&r.w, vec![usage(&r.w, "two1", r.model, tokens_680())]).await;
    // Run 1 at twice the price: 680 → 1360, so +680.
    reprice_model(cp, r.model, card("2", "4", "0.2")).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["delta_micro_cny"], 680);
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    // Run 2 at three times: 2040 − (680 + 680).
    reprice_model(cp, r.model, card("3", "6", "0.3")).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["delta_micro_cny"], 680);
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    assert_eq!(
        adjustments(db)
            .await
            .iter()
            .map(|a| a.2)
            .collect::<Vec<_>>(),
        vec![680, 680]
    );
    assert_eq!(balance(db, r.w.tenant).await, -2_040);
    // Run 3 at the same price changes nothing.
    let (_, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["changed"], 0);
    assert_eq!(run["delta_micro_cny"], 0);
}

#[tokio::test]
async fn a_run_selects_at_most_the_cap() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let key = Uuid::parse_str(&r.w.key).expect("a key id");
    let from = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("a time");
    let to = from + TimeDelta::days(1);
    insert_unpriced_charges(
        db,
        "cap",
        MAX_REPRICE_EVENTS,
        r.w.tenant,
        key,
        r.model,
        from,
    )
    .await;
    let (_, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["events"], MAX_REPRICE_EVENTS);
    assert_eq!(run["still_unpriced"], MAX_REPRICE_EVENTS);
    insert_unpriced_charges(db, "cap-over", 1, r.w.tenant, key, r.model, from).await;
    let refused = preview(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(refused.status, 422, "{refused:?}");
    assert_eq!(refused.body["details"][0]["path"], "/to");
    let message = refused.body["details"][0]["message"]
        .as_str()
        .expect("a message");
    assert!(
        message.contains(&(MAX_REPRICE_EVENTS + 1).to_string()),
        "{message}"
    );
}

#[tokio::test]
async fn a_preview_is_refused_for_a_bad_filter_or_reason() {
    let Some(r) = setup(false).await else { return };
    let cp = &r.w.cp;
    let now = db_now(cp).await;
    let hour = TimeDelta::hours(1);
    for (body, path) in [
        (run_body(now - hour, now, json!({"reason": ""})), "/reason"),
        (
            run_body(now - hour, now, json!({"reason": "x".repeat(501)})),
            "/reason",
        ),
        (
            run_body(now - hour, now, json!({"status": "billed"})),
            "/status",
        ),
        (run_body(now, now - hour, json!({})), "/to"),
        (run_body(now - hour, now + hour, json!({})), "/to"),
    ] {
        let refused = preview(cp, &r.w.admin, body.clone()).await;
        assert_eq!(refused.status, 422, "{body}: {refused:?}");
        assert_eq!(refused.body["details"][0]["path"], path, "{body}");
    }
    let (_, run) = previewed(
        cp,
        &r.w.admin,
        run_body(
            now - hour,
            now,
            json!({"reason": "x".repeat(500), "status": "charged"}),
        ),
    )
    .await;
    assert_eq!(run["events"], 0);
    assert_eq!(cp.count("billing_repricing_runs_cny").await, 1);
}

#[tokio::test]
async fn the_adjustments_refuse_update_and_delete() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    send(&r.w, vec![usage(&r.w, "ao1", r.model, tokens_680())]).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    for sql in [
        "UPDATE pricing_adjustments_cny SET delta_micro_cny = 1",
        "DELETE FROM pricing_adjustments_cny",
        "TRUNCATE pricing_adjustments_cny",
    ] {
        let err = sqlx::query(sql)
            .execute(db.pool())
            .await
            .expect_err("refused");
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    assert_eq!(adjustments(db).await.len(), 1);
}

/// The world's tenant's owner.
async fn owner_get(r: &R, path: &str) -> Resp {
    let owner =
        r.w.cp
            .member_login_seeded(r.w.tenant, MemberRole::Owner)
            .await;
    r.w.cp.send("GET", path, Some(owner.cookie()), None).await
}

fn around_now() -> String {
    let now = Utc::now();
    format!(
        "group=day&from={}&to={}",
        (now - TimeDelta::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        (now + TimeDelta::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[tokio::test]
async fn a_tenants_call_log_shows_the_adjusted_charge() {
    let Some(r) = setup(false).await else { return };
    let cp = &r.w.cp;
    // Two calls, and a failover attempt the upstream billed on a model with
    // no price yet: hidden while unpriced, since nothing was charged.
    let mut retried = usage(&r.w, "cl3", r.model, tokens_680());
    retried["is_final_attempt"] = json!(false);
    let mut events = vec![retried];
    for n in 1..=2 {
        let mut e = usage(&r.w, &format!("cl{n}"), r.model, tokens_680());
        e["is_final_attempt"] = json!(true);
        events.push(e);
    }
    send(&r.w, events).await;
    let calls = owner_get(&r, "/api/usage/calls").await;
    assert_eq!(calls.status, 200, "{calls:?}");
    let items = calls.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{calls:?}");
    for c in items {
        assert_eq!(c["charge"]["status"], "pending");
        assert_eq!(c["charge"]["adjusted_micro_cny"], 0);
    }

    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    let calls = owner_get(&r, "/api/usage/calls").await;
    let items = calls.body["items"].as_array().expect("items");
    // The billed attempt is now charged by the run, and listed.
    assert_eq!(items.len(), 3, "{calls:?}");
    for c in items {
        assert_eq!(c["charge"]["status"], "charged", "{c}");
        assert_eq!(c["charge"]["adjusted_micro_cny"], 680, "{c}");
        assert!(c["charge"]["amount_micro_cny"].is_null(), "{c}");
    }
    let retried = items
        .iter()
        .find(|c| c["event_id"] == "cl3")
        .expect("the billed attempt");
    assert_eq!(retried["status"], "retried");
}

#[tokio::test]
async fn a_tenants_usage_summary_counts_the_corrected_charge() {
    let Some(r) = setup(true).await else { return };
    let cp = &r.w.cp;
    three_rows(&r.w, "su", r.model).await;
    let summary = owner_get(&r, &format!("/api/usage/summary?{}", around_now())).await;
    assert_eq!(summary.status, 200, "{summary:?}");
    assert_eq!(summary.body["items"][0]["charged_micro_cny"], 2_040);
    reprice_model(cp, r.model, card("0.5", "1", "0.05")).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    let summary = owner_get(&r, &format!("/api/usage/summary?{}", around_now())).await;
    assert_eq!(summary.body["items"].as_array().expect("items").len(), 1);
    assert_eq!(summary.body["items"][0]["requests"], 3);
    assert_eq!(
        summary.body["items"][0]["charged_micro_cny"], 1_020,
        "{summary:?}"
    );
    let calls = owner_get(&r, "/api/usage/calls").await;
    for c in calls.body["items"].as_array().expect("items") {
        assert_eq!(c["charge"]["amount_micro_cny"], 680, "{c}");
        assert_eq!(c["charge"]["adjusted_micro_cny"], -340, "{c}");
    }
    // Operators read the same corrected figure.
    let admin_summary = cp
        .send(
            "GET",
            &format!(
                "/admin/tenants/{}/usage/summary?{}",
                r.w.tenant,
                around_now()
            ),
            Some(r.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(admin_summary.body["items"][0]["charged_micro_cny"], 1_020);
}

#[tokio::test]
async fn operators_preview_and_only_admins_apply() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "ro", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let refused = preview(cp, &viewer, run_body(from, to, json!({}))).await;
    assert_eq!(refused.status, 403, "{refused:?}");
    assert_eq!(cp.count("billing_repricing_runs_cny").await, 0);
    // The world's operator previews, and may not apply.
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    let refused = apply(cp, &r.w.admin, &id).await;
    assert_eq!(refused.status, 403, "{refused:?}");
    assert!(entries(db, r.w.tenant).await.is_empty());
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");
    // A viewer reads runs.
    let listed = cp
        .send(
            "GET",
            "/admin/billing/repricing",
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(listed.body["items"][0]["id"], id);
    assert_eq!(listed.body["items"][0]["status"], "applied");
    assert_eq!(
        listed.body["items"][0]["by_tenant"][0]["tenant_name"],
        r.w.cp
            .send(
                "GET",
                &format!("/admin/tenants/{}", r.w.tenant),
                Some(viewer.cookie()),
                None
            )
            .await
            .body["name"]
    );
    assert!(listed.body["next_before"].is_null());
    for (path, status) in [
        (format!("/admin/billing/repricing/{id}"), 200),
        (format!("/admin/billing/repricing/{}", Uuid::new_v4()), 404),
        ("/admin/billing/repricing/not-a-uuid".to_string(), 404),
    ] {
        let got = cp.send("GET", &path, Some(viewer.cookie()), None).await;
        assert_eq!(got.status, status, "{path}: {got:?}");
    }
    let unknown = apply(cp, &r.admin, &Uuid::new_v4().to_string()).await;
    assert_eq!(unknown.status, 404, "{unknown:?}");
}

#[tokio::test]
async fn the_runs_list_pages_newest_first() {
    let Some(r) = setup(false).await else { return };
    let cp = &r.w.cp;
    let (from, to) = last_hour(cp).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(
            previewed(cp, &r.w.admin, run_body(from, to, json!({})))
                .await
                .0,
        );
    }
    let first = cp
        .send(
            "GET",
            "/admin/billing/repricing?limit=2",
            Some(r.w.admin.cookie()),
            None,
        )
        .await;
    let page: Vec<&Value> = first.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .collect();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["id"], ids[2]);
    assert_eq!(page[1]["id"], ids[1]);
    let before = first.body["next_before"].as_i64().expect("another page");
    let second = cp
        .send(
            "GET",
            &format!("/admin/billing/repricing?limit=2&before={before}"),
            Some(r.w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(second.body["items"].as_array().expect("items").len(), 1);
    assert_eq!(second.body["items"][0]["id"], ids[0]);
    assert!(second.body["next_before"].is_null());
}

/// Times one apply of `MAX_REPRICE_EVENTS` charges spread over
/// `MAX_REPRICE_TENANTS` tenants, on a debug build. The apply locks every
/// tenant it changes for its transaction, and intake for those tenants waits
/// on their accounts while it runs; it then settles each one's marks in a
/// transaction of its own. Run with `--ignored`; the report records the
/// figure.
#[tokio::test]
#[ignore]
async fn a_full_run_applies_within_three_seconds() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let from = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("a time");
    let tenant_count = i64::try_from(MAX_REPRICE_TENANTS).expect("a count");
    let per_tenant = MAX_REPRICE_EVENTS / tenant_count;
    let mut tenants = Vec::new();
    for n in 0..MAX_REPRICE_TENANTS {
        let tenant = seed_tenant(cp, &r.w.admin).await;
        let key = Uuid::parse_str(&seed_key(cp, &r.w.admin, tenant, None).await).expect("a key");
        insert_unpriced_charges(
            db,
            &format!("full{n}x"),
            per_tenant,
            tenant,
            key,
            r.model,
            from,
        )
        .await;
        tenants.push(tenant);
    }
    price(cp, r.model).await;
    let (id, run) = previewed(
        cp,
        &r.w.admin,
        run_body(from, from + TimeDelta::days(1), json!({})),
    )
    .await;
    assert_eq!(run["changed"], per_tenant * tenant_count);
    assert_eq!(
        run["by_tenant"].as_array().map(Vec::len),
        Some(MAX_REPRICE_TENANTS)
    );
    let started = Instant::now();
    let applied = apply(cp, &r.admin, &id).await;
    let took = started.elapsed();
    assert_eq!(applied.status, 200, "{applied:?}");
    println!(
        "apply of {} charges over {MAX_REPRICE_TENANTS} tenants took {took:?}",
        per_tenant * tenant_count
    );
    for tenant in tenants {
        assert_eq!(balance(db, tenant).await, -680 * per_tenant);
    }
    assert!(took < Duration::from_secs(3), "apply took {took:?}");
}

/// `count` tenants that do not exist, each with one unpriced charge on
/// `model` occurring at `at`, filed under it as intake files a row: rows a
/// preview selects and prices, for counts of tenants a test cannot create
/// quickly.
async fn phantom_tenants(db: &Db, prefix: &str, count: usize, model: Uuid, at: DateTime<Utc>) {
    for n in 0..count {
        insert_unpriced_charges(
            db,
            &format!("{prefix}{n}x"),
            1,
            Uuid::new_v4(),
            Uuid::new_v4(),
            model,
            at,
        )
        .await;
    }
}

/// The 422 at `/tenant_id` that names `count` tenants.
fn assert_tenant_cap_refusal(r: &Resp, count: usize) {
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/tenant_id", "{r:?}");
    let message = r.body["details"][0]["message"].as_str().expect("a message");
    assert!(message.contains(&count.to_string()), "{message}");
}

#[tokio::test]
async fn a_preview_changing_more_tenants_than_the_cap_is_refused() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let from = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("a time");
    let to = from + TimeDelta::days(1);
    phantom_tenants(db, "tc", MAX_REPRICE_TENANTS, r.model, from).await;
    // Exactly the cap previews.
    let (_, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(
        run["by_tenant"].as_array().map(Vec::len),
        Some(MAX_REPRICE_TENANTS)
    );
    // One tenant more is refused, naming how many the run would change.
    phantom_tenants(db, "tc-over", 1, r.model, from).await;
    let refused = preview(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_tenant_cap_refusal(&refused, MAX_REPRICE_TENANTS + 1);
    // Narrowed to one tenant, the same window previews.
    let narrowed = preview(
        cp,
        &r.w.admin,
        run_body(from, to, json!({"tenant_id": r.w.tenant})),
    )
    .await;
    assert_eq!(narrowed.status, 201, "{narrowed:?}");
}

#[tokio::test]
async fn an_apply_refuses_a_run_that_grew_past_the_tenant_cap() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "gr", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["by_tenant"].as_array().map(Vec::len), Some(1));
    // Rows of as many other tenants again arrive late inside the window.
    phantom_tenants(
        db,
        "gr-late",
        MAX_REPRICE_TENANTS,
        r.model,
        to - TimeDelta::minutes(30),
    )
    .await;
    let refused = apply(cp, &r.admin, &id).await;
    assert_tenant_cap_refusal(&refused, MAX_REPRICE_TENANTS + 1);
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant).await.is_empty());
    let stored = cp
        .send(
            "GET",
            &format!("/admin/billing/repricing/{id}"),
            Some(r.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(stored.body["status"], "previewed", "{stored:?}");
}

/// A run stored before the cap existed may change more tenants than it
/// allows: apply refuses it before it locks any of them.
#[tokio::test]
async fn an_apply_refuses_a_stored_run_past_the_tenant_cap() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "sr", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    let mut by_tenant: Vec<Value> = (0..MAX_REPRICE_TENANTS)
        .map(|_| json!({"tenant_id": Uuid::new_v4(), "events": 1, "changed": 1, "delta_micro_cny": 1}))
        .collect();
    by_tenant.push(json!({
        "tenant_id": r.w.tenant, "events": 3, "changed": 3, "delta_micro_cny": 2_040,
    }));
    sqlx::query("UPDATE billing_repricing_runs_cny SET by_tenant = $2 WHERE id = $1")
        .bind(Uuid::parse_str(&id).expect("a run id"))
        .bind(sqlx::types::Json(by_tenant))
        .execute(db.pool())
        .await
        .expect("store an over-wide run");
    let refused = apply(cp, &r.admin, &id).await;
    assert_tenant_cap_refusal(&refused, MAX_REPRICE_TENANTS + 1);
    assert!(adjustments(db).await.is_empty());
}

/// Tenant `tenant`'s balance document, as stored.
async fn balance_doc(db: &Db, tenant: Uuid) -> Value {
    sqlx::query_scalar(
        "SELECT r.doc FROM gw_resources r JOIN billing_accounts a ON a.balance_budget_id = r.id \
         WHERE a.tenant_id = $1 AND r.kind = 'budgets' AND NOT r.deleted",
    )
    .bind(tenant)
    .fetch_one(db.pool())
    .await
    .expect("read the balance document")
}

#[tokio::test]
async fn a_run_changing_two_tenants_posts_and_settles_each() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let a = r.w.tenant;
    let b = seed_tenant(cp, &r.w.admin).await;
    let b_key = seed_key(cp, &r.w.admin, b, None).await;
    let now = db_now(cp).await;
    cp.db()
        .tx(move |c| {
            async move {
                store::insert_discount(
                    c,
                    &NewDiscount {
                        tenant_id: b,
                        effective_from: now,
                        discount_bps: 2_000,
                    },
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("discount B");
    // Each prepaid tenant holds a credit of $0.01.
    for tenant in [a, b] {
        let r = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/credits"),
                Some(r.admin.cookie()),
                Some(json!({"amount_cny": "0.01", "reason": "重新定价测试 7c1"})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
    }
    // 20 000 input tokens each, on a model with no price yet.
    let mut on_b = usage(&r.w, "pair-b", r.model, json!({"prompt_tokens": 20_000}));
    on_b["team_id"] = json!(b.to_string());
    on_b["api_key_id"] = json!(b_key);
    on_b["user_id"] = json!("");
    send(
        &r.w,
        vec![
            usage(&r.w, "pair-a", r.model, json!({"prompt_tokens": 20_000})),
            on_b,
        ],
    )
    .await;
    for tenant in [a, b] {
        assert_eq!(balance(db, tenant).await, 10_000);
        assert!(balance_doc(db, tenant).await["exhausted"].is_null());
    }

    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    let parts: HashMap<Uuid, i64> = run["by_tenant"]
        .as_array()
        .expect("by_tenant")
        .iter()
        .map(|t| {
            (
                uuid(&t["tenant_id"]),
                t["delta_micro_cny"].as_i64().expect("a delta"),
            )
        })
        .collect();
    // 20 000 µ$ for A, and for B with its 20 % off.
    assert_eq!(parts, HashMap::from([(a, 20_000), (b, 16_000)]));
    assert_eq!(run["delta_micro_cny"], 36_000);
    let applied = apply(cp, &r.admin, &id).await;
    assert_eq!(applied.status, 200, "{applied:?}");

    let run_id = Some(Uuid::parse_str(&id).expect("a run id"));
    for (tenant, delta, after) in [(a, 20_000, -10_000), (b, 16_000, -6_000)] {
        let ledger = entries(db, tenant).await;
        let adjusted: Vec<_> = ledger.iter().filter(|e| e.0 == "adjustment").collect();
        assert_eq!(
            adjusted,
            [&("adjustment".to_string(), -delta, after, run_id)],
            "{ledger:?}"
        );
        assert_eq!(balance(db, tenant).await, after);
        // Each tenant's balance mark is settled exhausted by the apply.
        let doc = balance_doc(db, tenant).await;
        assert!(doc["exhausted"].is_object(), "{tenant}: {doc}");
    }
}

/// Whether some session is queued for the resources lock.
async fn resources_lock_queued(db: &Db) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
           AND classid::bigint = $1 AND objid::bigint = $2 AND objsubid = 1)",
    )
    .bind(RESOURCES_LOCK_KEY >> 32)
    .bind(RESOURCES_LOCK_KEY & 0xffff_ffff)
    .fetch_one(db.pool())
    .await
    .expect("read the queued locks")
}

/// A run that changes two tenants holds neither one's usage intake while it
/// writes the other's marks: while the gateway documents are busy, and the
/// marks of the tenant settled first wait for them, a batch charging the
/// other tenant is stored. Both marks are still settled by the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_apply_holds_no_other_tenants_intake_while_it_settles_marks() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    let a = r.w.tenant;
    let b = seed_tenant(cp, &r.w.admin).await;
    let b_key = seed_key(cp, &r.w.admin, b, None).await;
    // A usage row of tenant `t` on `model`, as the gateway records it.
    let row_of = |t: Uuid, id: &str, model: Uuid, fields: Value| {
        let mut row = usage(&r.w, id, model, fields);
        if t == b {
            row["team_id"] = json!(b.to_string());
            row["api_key_id"] = json!(b_key);
            row["user_id"] = json!("");
        }
        row
    };
    // Each prepaid tenant holds $0.01 and spends 20 000 µ$ on a model with
    // no price yet, so the run takes each one below its floor.
    for tenant in [a, b] {
        let credited = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/credits"),
                Some(r.admin.cookie()),
                Some(json!({"amount_cny": "0.01", "reason": "重新定价测试 7c1"})),
            )
            .await;
        assert_eq!(credited.status, 201, "{credited:?}");
    }
    send(
        &r.w,
        vec![
            row_of(a, "hold-a", r.model, json!({"prompt_tokens": 20_000})),
            row_of(b, "hold-b", r.model, json!({"prompt_tokens": 20_000})),
        ],
    )
    .await;
    // A second model, priced before the preview (a price written after it
    // makes it stale), for a batch that is not in the run.
    let other = seed_model(cp, &r.w.admin).await;
    price(cp, other).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["changed"], 2, "{run}");
    // The tenants are settled in ascending id order: the later one is free
    // while the first one's marks wait.
    let later = a.max(b);

    let mut writer = db.pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .expect("hold the resources lock");
    let applying = apply(cp, &r.admin, &id);
    let probe = async {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !resources_lock_queued(db).await {
            assert!(
                Instant::now() < deadline,
                "the apply never queued for the resources lock"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let delivered = tokio::time::timeout(
            Duration::from_secs(2),
            deliver(
                &r.w.dp,
                Uuid::new_v4(),
                vec![row_of(later, "during-apply", other, tokens_680())],
            ),
        )
        .await;
        writer.rollback().await.expect("end the write");
        delivered
    };
    let (applied, delivered) = tokio::join!(applying, probe);
    let delivered = delivered.expect("the batch waited behind the apply");
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(applied.status, 200, "{applied:?}");
    for tenant in [a, b] {
        let doc = balance_doc(db, tenant).await;
        assert!(doc["exhausted"].is_object(), "{tenant}: {doc}");
    }
}

/// A price write that has moved the price version but not committed when
/// apply reads it: apply waits for it and then finds its preview stale,
/// rather than reading the version from before the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_price_write_in_flight_at_apply_makes_it_stale() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    three_rows(&r.w, "fl", r.model).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;

    let mut writer = db.pool().begin().await.expect("begin");
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1), updated_at = now() \
         WHERE key = 'pricing_version'",
    )
    .execute(&mut *writer)
    .await
    .expect("bump pricing_version");
    let done = AtomicBool::new(false);
    let applying = async {
        let answer = apply(cp, &r.admin, &id).await;
        done.store(true, Ordering::SeqCst);
        answer
    };
    // The write commits once apply waits for it, or as soon as apply has
    // answered without waiting.
    let release = async {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !done.load(Ordering::SeqCst) && lock_waiters(cp).await < 1 {
            assert!(
                Instant::now() < deadline,
                "apply neither waited nor answered"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        writer.commit().await.expect("commit the write");
    };
    let (applied, ()) = tokio::join!(applying, release);
    assert_eq!(
        (applied.status, &applied.body["code"]),
        (409, &json!("stale_preview")),
        "{applied:?}"
    );
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant).await.is_empty());
}

/// Two runs previewed over the same rows: the first applied corrects them,
/// and the second, recomputed from the corrected charges, finds its preview
/// stale, so the rows are corrected once.
#[tokio::test]
async fn two_runs_previewed_over_the_same_rows_correct_them_once() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    send(&r.w, vec![usage(&r.w, "dbl1", r.model, tokens_680())]).await;
    assert_eq!(balance(db, r.w.tenant).await, -680);
    // Twice the price: 680 → 1360, so each preview says +680.
    reprice_model(cp, r.model, card("2", "4", "0.2")).await;
    let (from, to) = last_hour(cp).await;
    let (first, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["delta_micro_cny"], 680);
    let (second, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["delta_micro_cny"], 680);

    assert_eq!(apply(cp, &r.admin, &first).await.status, 200);
    let refused = apply(cp, &r.admin, &second).await;
    assert_eq!(
        (refused.status, &refused.body["code"]),
        (409, &json!("stale_preview")),
        "{refused:?}"
    );
    let first_id = Uuid::parse_str(&first).expect("a run id");
    assert_eq!(
        adjustments(db).await,
        vec![(first_id, "dbl1".to_string(), 680)]
    );
    let adjusted: Vec<_> = entries(db, r.w.tenant)
        .await
        .into_iter()
        .filter(|e| e.0 == "adjustment")
        .collect();
    assert_eq!(adjusted.len(), 1, "{adjusted:?}");
    assert_eq!(balance(db, r.w.tenant).await, -1_360);
    // A fresh preview starts from the corrected charge: nothing is left.
    let (_, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["changed"], 0);
    assert_eq!(run["delta_micro_cny"], 0);
}

/// Group `group`'s default rate becomes `cny_per_usd`, starting now: written
/// through the route an operator uses, which is the only writer of a rate.
async fn set_rate(cp: &TestCp, admin: &OperatorSession, group: Uuid, cny_per_usd: &str) {
    let now = db_now(cp).await;
    let r = cp
        .send(
            "POST",
            &format!("/admin/provider-groups/{group}/rates"),
            Some(admin.cookie()),
            Some(json!({"model_id": null, "cny_per_usd": cny_per_usd, "effective_from": now})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// `shared`'s default rate becomes `cny_per_usd`, starting now.
async fn set_shared_rate(cp: &TestCp, admin: &OperatorSession, cny_per_usd: &str) {
    set_rate(cp, admin, group_id(cp, SHARED_GROUP).await, cny_per_usd).await;
}

/// `PATCH /admin/tenants/{id}` with `body` at the tenant's current version.
async fn patch_tenant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, body: Value) -> Resp {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the tenant");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let response = client
        .patch(cp.url(&format!("/admin/tenants/{tenant}")))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&body)
        .send()
        .await
        .expect("send");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    Resp {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
        set_cookie: None,
        headers: reqwest::header::HeaderMap::new(),
    }
}

/// `PATCH /admin/keys/{id}` with `body` at the key's current version.
async fn patch_key(cp: &TestCp, admin: &OperatorSession, key: &str, body: Value) -> Resp {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id::text = $1")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let response = client
        .patch(cp.url(&format!("/admin/keys/{key}")))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&body)
        .send()
        .await
        .expect("send");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    Resp {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
        set_cookie: None,
        headers: reqwest::header::HeaderMap::new(),
    }
}

/// A model with a baseline and no rate row anywhere is stored `missing_rate`,
/// not `missing_price`, and a run charges it once the rate exists.
#[tokio::test]
async fn a_missing_rate_row_is_charged_by_a_run_once_the_rate_is_set() {
    let Some(r) = setup(false).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    // The baseline in force and no rate at all: `shared` has none either, and
    // a brand-new deployment is exactly that state.
    let now = db_now(cp).await;
    insert_baseline(cp, r.model, now, card("1", "2", "0.1"))
        .await
        .expect("the baseline");
    send(&r.w, vec![usage(&r.w, "mr1", r.model, tokens_680())]).await;
    let (status, code, _) = charge(db, "mr1").await;
    assert_eq!(
        (status.as_str(), code.as_deref()),
        ("unpriced", Some("missing_rate")),
        "no rate row is `missing_rate`, never a zero charge"
    );
    // A run while the rate is still missing changes nothing.
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["events"], 1);
    assert_eq!(run["still_unpriced"], 1);
    assert_eq!(run["changed"], 0);
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    assert!(adjustments(db).await.is_empty());
    assert!(entries(db, r.w.tenant).await.is_empty());

    // The operator sets the rate, and the next run charges the row at it.
    set_shared_rate(cp, &r.admin, "1").await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["events"], 1);
    assert_eq!(run["still_unpriced"], 0);
    assert_eq!(run["changed"], 1);
    assert_eq!(run["delta_micro_cny"], 680);
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    let run_id = Uuid::parse_str(&id).expect("a uuid");
    assert_eq!(
        adjustments(db).await,
        vec![(run_id, "mr1".to_string(), 680)]
    );
    assert_eq!(
        entries(db, r.w.tenant).await,
        vec![("adjustment".to_string(), -680, -680, Some(run_id))]
    );
    assert_eq!(balance(db, r.w.tenant).await, -680);
}

/// `RepriceFilter.group_id` selects on the group the row is billed at, so a
/// run over one group leaves every other group's rows alone.
#[tokio::test]
async fn a_run_filtered_by_group_touches_only_that_group() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    // A second group at 2 CNY per USD, and a key of the same tenant in it.
    let other = support::seed_group(
        cp,
        &r.w.admin,
        &format!("grp-rep-{}", Uuid::new_v4().simple()),
    )
    .await;
    let shared = group_id(cp, SHARED_GROUP).await;
    let name: String = sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
        .bind(other)
        .fetch_one(db.pool())
        .await
        .expect("the other group's name");
    // A tenant's groups are routing names, so the assignment names both.
    let assigned = patch_tenant(
        cp,
        &r.w.admin,
        r.w.tenant,
        json!({"allowed_provider_groups": [SHARED_GROUP, name]}),
    )
    .await;
    assert_eq!(assigned.status, 200, "{assigned:?}");
    let key = seed_key_in_group(cp, &r.w.admin, r.w.tenant, None, &name).await;
    set_rate(cp, &r.w.admin, other, "2").await;
    // `shared` keeps the rate `price` gave it, 1: each row is charged at
    // intake at its own group's rate.
    let mut in_other = usage(&r.w, "g2", r.model, tokens_680());
    in_other["api_key_id"] = json!(key);
    send(
        &r.w,
        vec![usage(&r.w, "g1", r.model, tokens_680()), in_other],
    )
    .await;
    for (what, id, amount) in [
        ("the shared group's", "g1", 680_i64),
        ("the other group's", "g2", 1_360_i64),
    ] {
        let stored: Option<i64> = sqlx::query_scalar(
            "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = $1",
        )
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("read the charge");
        assert_eq!(stored, Some(amount), "{what} row is charged at its rate");
    }
    // The platform price doubles: 680 µ$ → 1 360 µ$ is +680 µ¥ at `shared`
    // and +1 360 µ¥ at the other group's 2.
    reprice_model(cp, r.model, card("2", "4", "0.2")).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(
        cp,
        &r.w.admin,
        run_body(from, to, json!({"group_id": other})),
    )
    .await;
    assert_eq!(run["events"], 1, "only the other group's row is selected");
    assert_eq!(run["changed"], 1);
    assert_eq!(run["delta_micro_cny"], 1_360);
    assert_eq!(run["filter"]["group_id"], other.to_string());
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    let run_id = Uuid::parse_str(&id).expect("a uuid");
    assert_eq!(
        adjustments(db).await,
        vec![(run_id, "g2".to_string(), 1_360)],
        "the shared group's row is untouched"
    );
    // And the shared row still needs its own run, which selects it alone.
    let (_, run) = previewed(
        cp,
        &r.w.admin,
        run_body(from, to, json!({"group_id": shared})),
    )
    .await;
    assert_eq!(run["events"], 1);
    assert_eq!(run["delta_micro_cny"], 680);
    assert_eq!(run["changed"], 1);
}

/// Review Focus 1: a legacy internal key's rows are stored `no_group` while
/// it has none, and a run after the operator gives it one charges them at
/// that group's rate — through `BILLED_GROUP`'s fallback to the key's group,
/// since the charge itself records none.
#[tokio::test]
async fn a_legacy_internal_keys_rows_are_charged_at_the_group_it_takes_later() {
    let Some(r) = setup(true).await else { return };
    let (cp, db) = (&r.w.cp, r.w.cp.db());
    set_shared_rate(cp, &r.admin, "7.2").await;
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(db.pool())
        .await
        .expect("the internal tenant");
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{internal}/keys"),
            Some(r.w.admin.cookie()),
            Some(json!({"label": "legacy-7c1"})),
        )
        .await;
    // The internal tenant has no single group to fall back on, so its key
    // must choose one; this key then has it cleared, as an older replica left
    // it.
    assert_eq!(created.status, 422, "{created:?}");
    let created = cp
        .send(
            "POST",
            &format!("/admin/tenants/{internal}/keys"),
            Some(r.w.admin.cookie()),
            Some(json!({"label": "legacy-7c1", "group_id": group_id(cp, SHARED_GROUP).await})),
        )
        .await;
    assert_eq!(created.status, 201, "{created:?}");
    let key = created.body["key"]["id"]
        .as_str()
        .expect("a key id")
        .to_string();
    sqlx::query("UPDATE tenant_keys SET group_id = NULL WHERE id::text = $1")
        .bind(&key)
        .execute(db.pool())
        .await
        .expect("clear the key's group");

    let mut row = usage(&r.w, "e-legacy", r.model, tokens_680());
    row["team_id"] = json!(internal.to_string());
    row["api_key_id"] = json!(key);
    row["user_id"] = json!("");
    let delivered = deliver(&r.w.dp, Uuid::new_v4(), vec![row]).await;
    assert_eq!(delivered.status, 200, "{delivered:?}");
    let (status, code, _) = charge(db, "e-legacy").await;
    assert_eq!(
        (status.as_str(), code.as_deref()),
        ("unpriced", Some("no_group")),
        "the key has no group, so its row is stored no_group"
    );
    let filed: Option<Uuid> =
        sqlx::query_scalar("SELECT group_id FROM pricing_charges_cny WHERE event_id = 'e-legacy'")
            .fetch_one(db.pool())
            .await
            .expect("read the charge's group");
    assert_eq!(filed, None, "the charge records no group");

    // Still no group: a run charges nothing.
    let (from, to) = last_hour(cp).await;
    let (_, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["still_unpriced"], 1);
    assert_eq!(run["changed"], 0);

    // The operator gives the key its group; the next run bills the row at it.
    let patched = patch_key(
        cp,
        &r.w.admin,
        &key,
        json!({"group_id": group_id(cp, SHARED_GROUP).await}),
    )
    .await;
    assert_eq!(patched.status, 200, "{patched:?}");
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["still_unpriced"], 0, "the row is now priced");
    assert_eq!(run["changed"], 1);
    // 680 µ$ × 7.2 at `shared`.
    assert_eq!(run["delta_micro_cny"], 4_896);
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    let run_id = Uuid::parse_str(&id).expect("a uuid");
    assert_eq!(
        adjustments(db).await,
        vec![(run_id, "e-legacy".to_string(), 4_896)]
    );
    assert_eq!(
        entries(db, internal).await,
        vec![("adjustment".to_string(), -4_896, -4_896, Some(run_id))],
        "the internal tenant pays like any other"
    );
}

/// R-084: a run that charges a row stored unpriced explains the charge with
/// the quantities it billed. The row's own charge keeps its empty lines (a
/// charge is written once); the call log, the usage summary and the
/// gateway's request-cost lookup read the lines of the run that corrected
/// it, so a corrected call is never shown as charged for nothing. A row
/// priced at intake, the control, keeps showing its own lines.
#[tokio::test]
async fn a_corrected_charge_shows_the_quantities_the_run_billed() {
    let Some(r) = setup(false).await else { return };
    let cp = &r.w.cp;
    let priced_model = seed_model(cp, &r.w.admin).await;
    price(cp, priced_model).await;
    let mut events = Vec::new();
    for (id, model) in [("ql1", r.model), ("ql2", r.model), ("qlp", priced_model)] {
        let mut e = usage(&r.w, id, model, tokens_680());
        e["is_final_attempt"] = json!(true);
        events.push(e);
    }
    send(&r.w, events).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (id, run) = previewed(
        cp,
        &r.w.admin,
        run_body(from, to, json!({"model_id": r.model})),
    )
    .await;
    assert_eq!(run["changed"], 2, "{run}");
    assert_eq!(apply(cp, &r.admin, &id).await.status, 200);
    // The charges themselves are as intake stored them.
    for event in ["ql1", "ql2"] {
        let lines: Value =
            sqlx::query_scalar("SELECT lines FROM pricing_charges_cny WHERE event_id = $1")
                .bind(event)
                .fetch_one(cp.db().pool())
                .await
                .expect("read the charge's lines");
        assert_eq!(lines, json!([]), "{event}");
    }

    // 1000 prompt tokens, 800 of them cache hits, and 200 completion tokens.
    let billed = json!({"input": 200, "cache_read": 800, "output": 200});
    let calls = owner_get(&r, "/api/usage/calls").await;
    assert_eq!(calls.status, 200, "{calls:?}");
    let items = calls.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 3, "{calls:?}");
    for c in items {
        let quantities: serde_json::Map<String, Value> = c["charge"]["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|l| {
                (
                    l["dimension"].as_str().expect("a dimension").to_string(),
                    l["quantity"].clone(),
                )
            })
            .collect();
        assert_eq!(Value::Object(quantities), billed, "{c}");
        // At `shared`'s rate of 1 CNY per USD: the card's own figures.
        for l in c["charge"]["lines"].as_array().expect("lines") {
            assert!(l["rate_cny"].is_string(), "{c}");
        }
    }

    let summary = owner_get(&r, &format!("/api/usage/summary?{}", around_now())).await;
    assert_eq!(summary.status, 200, "{summary:?}");
    // The tenant holds no grant, so both models share one row of three calls.
    let rows = summary.body["items"].as_array().expect("items");
    assert_eq!(rows.len(), 1, "{summary:?}");
    assert_eq!(rows[0]["requests"], 3, "{summary:?}");
    assert_eq!(
        rows[0]["quantities"],
        json!({"input": 600, "cache_read": 2_400, "output": 600}),
        "{summary:?}"
    );
    assert_eq!(rows[0]["charged_micro_cny"], 2_040, "{summary:?}");

    let lookup =
        r.w.dp
            .send(
                "GET",
                &format!("/dp/v1/keys/{}/requests?id=req-ql1", r.w.key),
                None,
            )
            .await;
    assert_eq!(lookup.status, 200, "{lookup:?}");
    assert_eq!(lookup.body["items"][0]["charge_status"], "charged");
    assert_eq!(
        lookup.body["items"][0]["quantities"],
        json!({"input": 200, "input_cache_read": 800, "output": 200}),
        "{lookup:?}"
    );
}

/// The input line's CNY rate of the one call in the owner's call log.
async fn input_rate(r: &R) -> Value {
    let calls = owner_get(r, "/api/usage/calls").await;
    assert_eq!(calls.status, 200, "{calls:?}");
    let lines = calls.body["items"][0]["charge"]["lines"].clone();
    lines
        .as_array()
        .expect("lines")
        .iter()
        .find(|l| l["dimension"] == "input")
        .unwrap_or_else(|| panic!("no input line in {lines}"))["rate_cny"]
        .clone()
}

/// A charge two runs corrected shows the lines of the later one: the price
/// its corrected amount was computed at.
#[tokio::test]
async fn a_charge_corrected_twice_shows_the_later_runs_lines() {
    let Some(r) = setup(false).await else { return };
    let cp = &r.w.cp;
    let mut e = usage(&r.w, "tw1", r.model, tokens_680());
    e["is_final_attempt"] = json!(true);
    send(&r.w, vec![e]).await;
    price(cp, r.model).await;
    let (from, to) = last_hour(cp).await;
    let (first, _) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(apply(cp, &r.admin, &first).await.status, 200);
    let at_first = input_rate(&r).await;
    assert!(at_first.is_string(), "{at_first}");

    // Half the price: the second run refunds 340 µ¥.
    reprice_model(cp, r.model, card("0.5", "1", "0.05")).await;
    let (from, to) = last_hour(cp).await;
    let (second, run) = previewed(cp, &r.w.admin, run_body(from, to, json!({}))).await;
    assert_eq!(run["delta_micro_cny"], -340, "{run}");
    assert_eq!(apply(cp, &r.admin, &second).await.status, 200);
    let at_second = input_rate(&r).await;
    assert_ne!(at_second, at_first, "the first run's rate is still shown");
    let calls = owner_get(&r, "/api/usage/calls").await;
    assert_eq!(calls.body["items"][0]["charge"]["adjusted_micro_cny"], 340);
}
