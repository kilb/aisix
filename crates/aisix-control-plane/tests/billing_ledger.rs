//! The billing ledger: every tenant has an account from its creation, and
//! each usage row charged above 0 debits its tenant's balance once, in the
//! transaction that prices it. Ingest locks accounts in ascending tenant
//! order and never a tenant row, so a held tenant does not stall intake and
//! two batches naming the same tenants cannot deadlock. The ledger itself
//! refuses every update and delete.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::billing::ledger;
use aisix_control_plane::housekeeping;
use aisix_control_plane::pricing::store::{self, NewDiscount};
use aisix_control_plane::resources::write::WriteCtx;
use aisix_control_plane::tenants;
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::world::{
    db_now, deliver, price, seed_key, seed_model, seed_tenant, tokens_680, usage, world, World,
};
use support::{Resp, TestCp, TEST_PASSWORD};
use tokio::sync::Barrier;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// One `billing_ledger` row, as far as these tests read it.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Entry {
    kind: String,
    amount: i64,
    balance_after: i64,
    event_id: Option<String>,
}

fn debit(amount: i64, balance_after: i64, event_id: &str) -> Entry {
    Entry {
        kind: "usage".to_string(),
        amount,
        balance_after,
        event_id: Some(event_id.to_string()),
    }
}

/// `tenant`'s ledger entries, in posting order.
async fn entries(cp: &TestCp, tenant: Uuid) -> Vec<Entry> {
    sqlx::query_as(
        "SELECT kind, amount_micro_usd AS amount, balance_after_micro_usd AS balance_after, \
         event_id FROM billing_ledger WHERE tenant_id = $1 ORDER BY seq",
    )
    .bind(tenant)
    .fetch_all(cp.db().pool())
    .await
    .expect("read the ledger")
}

/// `tenant`'s account balance; `None` when it has no account.
async fn balance(cp: &TestCp, tenant: Uuid) -> Option<i64> {
    sqlx::query_scalar("SELECT balance_micro_usd FROM billing_accounts WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_optional(cp.db().pool())
        .await
        .expect("read the balance")
}

/// A tenant beside the world's, with a member and that member's key.
struct Other {
    tenant: Uuid,
    member: Uuid,
    key: String,
}

async fn other_tenant(w: &World) -> Other {
    let tenant = seed_tenant(&w.cp, &w.admin).await;
    let member = w.cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&w.cp, &w.admin, tenant, Some(member)).await;
    Other {
        tenant,
        member,
        key,
    }
}

/// `usage(w, …)`, but of `o`'s tenant, member and key.
fn usage_of(w: &World, o: &Other, event_id: &str, model: Uuid, fields: Value) -> Value {
    let mut event = usage(w, event_id, model, fields);
    event["team_id"] = json!(o.tenant.to_string());
    event["api_key_id"] = json!(o.key);
    event["user_id"] = json!(o.member.to_string());
    event
}

/// `(status, charge)` of `event_id`'s charge.
async fn charge_of(cp: &TestCp, event_id: &str) -> Option<(String, Option<i64>)> {
    sqlx::query_as("SELECT status, charge_micro_usd FROM pricing_charges WHERE event_id = $1")
        .bind(event_id)
        .fetch_optional(cp.db().pool())
        .await
        .expect("read the charge")
}

#[tokio::test]
async fn a_charged_row_posts_one_debit_with_the_balance_after_it() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(-680));
}

#[tokio::test]
async fn rows_that_are_not_charged_post_nothing() {
    let Some(w) = world().await else { return };
    let priced = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, priced).await;
    let unpriced = seed_model(&w.cp, &w.admin).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(
                &w,
                "hit",
                priced,
                json!({"cache_status": "hit", "prompt_tokens": 500}),
            ),
            usage(&w, "zero", priced, json!({})),
            usage(&w, "no-price", unpriced, tokens_680()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(w.cp.count("pricing_charges").await, 3);
    assert_eq!(w.cp.count("billing_ledger").await, 0);
    assert_eq!(balance(&w.cp, w.tenant).await, Some(0));
}

#[tokio::test]
async fn a_zero_charge_row_posts_nothing_and_does_not_fail_its_batch() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let free = other_tenant(&w).await;
    let discount = NewDiscount {
        tenant_id: free.tenant,
        effective_from: db_now(&w.cp).await,
        discount_bps: 10_000,
    };
    w.cp.db()
        .tx(move |c| {
            async move { store::insert_discount(c, &discount, Actor::System).await }.boxed()
        })
        .await
        .expect("a 100 % discount");
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage_of(&w, &free, "free", model, tokens_680()),
            usage(&w, "paid", model, tokens_680()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "free").await,
        Some(("charged".to_string(), Some(0)))
    );
    assert_eq!(
        charge_of(&w.cp, "paid").await,
        Some(("charged".to_string(), Some(680)))
    );
    assert_eq!(w.cp.count("billing_ledger").await, 1);
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "paid")]
    );
    assert_eq!(balance(&w.cp, free.tenant).await, Some(0));
}

#[tokio::test]
async fn a_re_sent_batch_and_a_repeated_event_post_once() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let (b, event) = (Uuid::new_v4(), usage(&w, "e1", model, tokens_680()));
    for _ in 0..2 {
        let r = deliver(&w.dp, b, vec![event.clone()]).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    let r = deliver(&w.dp, Uuid::new_v4(), vec![event.clone()]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(-680));
}

#[tokio::test]
async fn the_entries_of_one_batch_run_in_event_id_order_with_running_balances() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        ["e3", "e1", "e2"]
            .iter()
            .map(|id| usage(&w, id, model, tokens_680()))
            .collect(),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![
            debit(-680, -680, "e1"),
            debit(-680, -1360, "e2"),
            debit(-680, -2040, "e3"),
        ]
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(-2040));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_batches_over_the_same_tenants_neither_deadlock_nor_lose_a_debit() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let second = other_tenant(&w).await;
    const BATCHES: usize = 20;
    // Half the batches name the second tenant first, both in the order sent
    // and in event_id order.
    let bodies: Vec<Value> = (0..BATCHES)
        .map(|i| {
            let (a, b) = (format!("b{i:02}-a"), format!("b{i:02}-b"));
            let events = if i % 2 == 0 {
                vec![
                    usage(&w, &a, model, tokens_680()),
                    usage_of(&w, &second, &b, model, tokens_680()),
                ]
            } else {
                vec![
                    usage_of(&w, &second, &a, model, tokens_680()),
                    usage(&w, &b, model, tokens_680()),
                ]
            };
            json!({ "events": events })
        })
        .collect();
    let dp = Arc::new(w.dp);
    let barrier = Arc::new(Barrier::new(BATCHES));
    let sends: Vec<_> = bodies
        .into_iter()
        .map(|body| {
            let (dp, barrier) = (dp.clone(), barrier.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                dp.telemetry(Uuid::new_v4(), &body).await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for send in sends {
        answers.push(send.await.expect("a delivery task"));
    }
    assert!(answers.iter().all(|r| r.status == 200), "{answers:?}");
    for tenant in [w.tenant, second.tenant] {
        let posted = entries(&w.cp, tenant).await;
        assert_eq!(posted.len(), BATCHES, "{tenant}");
        let sum: i64 = posted.iter().map(|e| e.amount).sum();
        assert_eq!(sum, -680 * BATCHES as i64, "{tenant}");
        assert_eq!(balance(&w.cp, tenant).await, Some(sum), "{tenant}");
        assert_eq!(
            posted.last().map(|e| e.balance_after),
            Some(sum),
            "{tenant}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_never_waits_on_a_held_tenant_row() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    // An operator's long tenant edit, as the control plane takes it.
    let mut edit = w.cp.db().pool().begin().await.expect("begin");
    tenants::lock(&mut edit, w.tenant)
        .await
        .expect("lock the tenant");
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(
            &w.dp,
            Uuid::new_v4(),
            vec![usage(&w, "e1", model, tokens_680())],
        ),
    )
    .await
    .expect("the batch is answered while the tenant is locked");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
    edit.rollback().await.expect("end the edit");
}

/// Waits until a statement of the control plane's own database waits on a
/// lock while locking a billing account; fails when `delivery` finishes
/// first, or when nothing waits within 4 s (below the 5 s lock timeout).
async fn until_intake_waits_on_an_account(cp: &TestCp, delivery: &JoinHandle<Resp>) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        assert!(
            !delivery.is_finished(),
            "the batch was answered while its account was held"
        );
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() \
               AND wait_event_type = 'Lock' AND query LIKE '%FROM billing_accounts%'",
        )
        .fetch_one(cp.db().pool())
        .await
        .expect("read pg_stat_activity");
        if waiting > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "intake never waited on the account"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_billing_lock_holds_off_its_tenants_intake_until_it_ends() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let (batch, body) = (
        Uuid::new_v4(),
        json!({"events": [usage(&w, "e1", model, tokens_680())]}),
    );
    // A write that moves the tenant's money, as the control plane takes it.
    let mut write = w.cp.db().pool().begin().await.expect("begin");
    tenants::lock_billing(&mut write, w.tenant)
        .await
        .expect("lock the tenant for billing");
    let dp = Arc::new(w.dp);
    let delivery = tokio::spawn({
        let dp = dp.clone();
        async move { dp.telemetry(batch, &body).await }
    });
    until_intake_waits_on_an_account(&w.cp, &delivery).await;
    assert_eq!(entries(&w.cp, w.tenant).await, vec![]);
    write.commit().await.expect("end the write");
    let r = delivery.await.expect("the delivery task");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_account_never_makes_intake_wait_on_its_tenant() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    delete_account(&w.cp, w.tenant).await;
    let mut edit = w.cp.db().pool().begin().await.expect("begin");
    tenants::lock(&mut edit, w.tenant)
        .await
        .expect("lock the tenant");
    let batch = Uuid::new_v4();
    let events = vec![usage(&w, "e1", model, tokens_680())];
    // The account cannot be opened while the tenant is held: the batch is
    // refused at once, to be sent again.
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(&w.dp, batch, events.clone()),
    )
    .await
    .expect("the batch is answered while the tenant is locked");
    assert_eq!(r.status, 503, "{r:?}");
    edit.rollback().await.expect("end the edit");
    let r = deliver(&w.dp, batch, events).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(-680));
}

/// A top-up request inserts a row that references its tenant's account,
/// whose foreign-key check key-shares the account. Intake's lock of the
/// account leaves that share free, so a request never waits on a batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_topup_request_never_waits_on_intakes_account_lock() {
    let Some(w) = world().await else { return };
    let owner = w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await;
    // The account as intake holds it while it prices a batch.
    let mut intake = w.cp.db().pool().begin().await.expect("begin");
    ledger::lock_account(&mut intake, w.tenant)
        .await
        .expect("lock the account");
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        w.cp.send(
            "POST",
            "/api/billing/topups",
            Some(owner.cookie()),
            Some(json!({"amount_usd": "5"})),
        ),
    )
    .await
    .expect("the request is answered while intake holds the account");
    assert_eq!(r.status, 201, "{r:?}");
    intake.rollback().await.expect("end intake");
}

#[tokio::test]
async fn a_row_attributed_to_no_tenant_posts_nothing() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let other = seed_tenant(&w.cp, &w.admin).await;
    let mut event = usage(&w, "e1", model, tokens_680());
    event["team_id"] = json!(other.to_string());
    let r = deliver(&w.dp, Uuid::new_v4(), vec![event]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(("unpriced".to_string(), None))
    );
    assert_eq!(entries(&w.cp, w.tenant).await, vec![]);
    assert_eq!(entries(&w.cp, other).await, vec![]);
    assert_eq!(balance(&w.cp, w.tenant).await, Some(0));
    assert_eq!(balance(&w.cp, other).await, Some(0));
}

#[tokio::test]
async fn the_ledger_refuses_update_and_delete() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    for statement in [
        "UPDATE billing_ledger SET amount_micro_usd = -1",
        "DELETE FROM billing_ledger",
    ] {
        let e = sqlx::query(statement)
            .execute(w.cp.db().pool())
            .await
            .expect_err(statement);
        assert!(e.to_string().contains("append-only"), "{statement}: {e}");
    }
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
}

#[tokio::test]
async fn the_ledger_refuses_truncate() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    for statement in [
        "TRUNCATE billing_ledger",
        "TRUNCATE billing_accounts CASCADE",
    ] {
        let e = sqlx::query(statement)
            .execute(w.cp.db().pool())
            .await
            .expect_err(statement);
        assert!(e.to_string().contains("append-only"), "{statement}: {e}");
    }
    assert_eq!(
        entries(&w.cp, w.tenant).await,
        vec![debit(-680, -680, "e1")]
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(-680));
}

#[tokio::test]
async fn every_tenant_has_its_account_from_creation() {
    let Some(w) = world().await else { return };
    let org = format!("signup-{}", Uuid::new_v4().simple());
    let r =
        w.cp.send(
            "POST",
            "/api/signup",
            None,
            Some(json!({
                "org_name": org,
                "email": format!("{org}@tenant.test"),
                "password": TEST_PASSWORD,
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let pool = w.cp.db().pool();
    let signed_up: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE name = $1")
        .bind(&org)
        .fetch_one(pool)
        .await
        .expect("the signed-up tenant");
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(signed_up)
        .fetch_one(pool)
        .await
        .expect("its status");
    assert_eq!(status, "pending");
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(pool)
        .await
        .expect("the internal tenant");
    for tenant in [w.tenant, signed_up, internal] {
        assert_eq!(balance(&w.cp, tenant).await, Some(0), "{tenant}");
    }
}

async fn delete_account(cp: &TestCp, tenant: Uuid) {
    let deleted = sqlx::query("DELETE FROM billing_accounts WHERE tenant_id = $1")
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("delete the account")
        .rows_affected();
    assert_eq!(deleted, 1);
}

#[tokio::test]
async fn the_backfill_restores_a_missing_account() {
    let Some(w) = world().await else { return };
    delete_account(&w.cp, w.tenant).await;
    assert_eq!(balance(&w.cp, w.tenant).await, None);
    let mut conn = w.cp.db().pool().acquire().await.expect("a connection");
    assert_eq!(
        ledger::backfill_accounts(&mut conn)
            .await
            .expect("backfill"),
        1
    );
    assert_eq!(
        ledger::backfill_accounts(&mut conn)
            .await
            .expect("backfill"),
        0
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_backfill_skips_a_held_tenant_and_opens_it_later() {
    let Some(w) = world().await else { return };
    delete_account(&w.cp, w.tenant).await;
    let mut edit = w.cp.db().pool().begin().await.expect("begin");
    tenants::lock(&mut edit, w.tenant)
        .await
        .expect("lock the tenant");
    let mut conn = w.cp.db().pool().acquire().await.expect("a connection");
    let opened = tokio::time::timeout(Duration::from_secs(2), ledger::backfill_accounts(&mut conn))
        .await
        .expect("the backfill does not wait on a held tenant")
        .expect("backfill");
    assert_eq!(opened, 0);
    edit.rollback().await.expect("end the edit");
    assert_eq!(
        ledger::backfill_accounts(&mut conn)
            .await
            .expect("backfill"),
        1
    );
    assert_eq!(balance(&w.cp, w.tenant).await, Some(0));
}

#[tokio::test]
async fn the_boot_and_every_housekeeping_tick_run_the_backfill() {
    let Some(w) = world().await else { return };
    delete_account(&w.cp, w.tenant).await;
    housekeeping::tick(w.cp.db(), &WriteCtx::of(w.cp.state())).await;
    assert_eq!(balance(&w.cp, w.tenant).await, Some(0));
    delete_account(&w.cp, w.tenant).await;
    let restarted = w.cp.restart().await;
    assert_eq!(balance(&restarted, w.tenant).await, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_billing_lock_holds_the_account() {
    let Some(w) = world().await else { return };
    let mut other = PgConnection::connect(w.cp.database_url())
        .await
        .expect("connect");
    let grab = "SELECT 1 FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE NOWAIT";

    let mut tx = w.cp.db().pool().begin().await.expect("begin");
    tenants::lock(&mut tx, w.tenant)
        .await
        .expect("lock the tenant");
    sqlx::query(grab)
        .bind(w.tenant)
        .fetch_one(&mut other)
        .await
        .expect("a plain tenant lock leaves the account free");
    tx.rollback().await.expect("rollback");

    let mut tx = w.cp.db().pool().begin().await.expect("begin");
    let (tenant, account) = tenants::lock_billing(&mut tx, w.tenant)
        .await
        .expect("lock the tenant for billing");
    assert_eq!((tenant.id, account.tenant_id), (w.tenant, w.tenant));
    assert_eq!(account.balance_micro_usd, 0);
    let e = sqlx::query(grab)
        .bind(w.tenant)
        .fetch_one(&mut other)
        .await
        .expect_err("the billing lock holds the account");
    let code = match &e {
        sqlx::Error::Database(db) => db.code().map(|c| c.to_string()),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("55P03"), "{e}");
    tx.rollback().await.expect("rollback");
    // Free again once the transaction ends.
    sqlx::query(grab)
        .bind(w.tenant)
        .fetch_one(&mut other)
        .await
        .expect("the account is free");
}

#[tokio::test]
async fn a_billing_lock_opens_a_missing_account_under_its_own_tenant_lock() {
    let Some(w) = world().await else { return };
    delete_account(&w.cp, w.tenant).await;
    let mut tx = w.cp.db().pool().begin().await.expect("begin");
    let (_, account) = tenants::lock_billing(&mut tx, w.tenant)
        .await
        .expect("its own tenant lock does not refuse the account insert");
    assert_eq!(account.balance_micro_usd, 0);
    tx.commit().await.expect("commit");
    assert_eq!(balance(&w.cp, w.tenant).await, Some(0));
}
