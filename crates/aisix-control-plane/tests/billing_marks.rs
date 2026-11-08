//! Balance marks: every customer tenant past pending has one `budgets`
//! document over its own team, marked exhausted while its money is out (a
//! prepaid balance at 0, a postpaid one at minus its credit limit). Tenant
//! projections write it right before the keys; usage intake only flags an
//! account whose balance crosses the floor, never waiting on the resources
//! lock, and the settler writes the mark after the batch commits, skipping
//! a tenant another transaction holds. The internal tenant is never marked.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it, in
//! rows of 680 µ$ (`tokens_680`). Credit limits and balances a test needs
//! are set with raw SQL: their routes come later.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::billing::ledger::Account;
use aisix_control_plane::billing::marks::{
    self, BALANCE_MESSAGE_POSTPAID, BALANCE_MESSAGE_PREPAID,
};
use aisix_control_plane::billing::{ledger, settler};
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use aisix_control_plane::tenants::{self, BillingMode, TenantStatus};
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::postgres::PgListener;
use support::world::{deliver, price, seed_key, seed_model, tokens_680, usage, world, World};
use support::{BareDatabase, OperatorSession, Resp, TestCp, TEST_MASTER_KEY, TEST_PASSWORD};
use uuid::Uuid;

/// Tenant `tenant`'s billing account.
async fn account(db: &Db, tenant: Uuid) -> Account {
    sqlx::query_as(
        "SELECT tenant_id, balance_micro_usd, credit_limit_micro_usd, balance_budget_id, \
         marked_exhausted, marks_dirty FROM billing_accounts WHERE tenant_id = $1",
    )
    .bind(tenant)
    .fetch_one(db.pool())
    .await
    .expect("read the account")
}

/// A stored `budgets` document.
#[derive(Debug)]
struct Mark {
    doc: Value,
    managed_by: Option<String>,
    deleted: bool,
    seq: i64,
}

/// The `budgets` document under tenant `tenant`'s balance id, if one was
/// ever written.
async fn mark(db: &Db, tenant: Uuid) -> Option<Mark> {
    let id = account(db, tenant).await.balance_budget_id;
    let raw: Option<(Value, Option<String>, bool, i64)> = sqlx::query_as(
        "SELECT doc, managed_by, deleted, seq FROM gw_resources \
         WHERE kind = 'budgets' AND id = $1",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .expect("read the balance document");
    raw.map(|(doc, managed_by, deleted, seq)| Mark {
        doc,
        managed_by,
        deleted,
        seq,
    })
}

/// The live balance document of `tenant`, which must exist.
async fn live_mark(db: &Db, tenant: Uuid) -> Mark {
    let mark = mark(db, tenant)
        .await
        .unwrap_or_else(|| panic!("tenant {tenant} has no balance document"));
    assert!(!mark.deleted, "{mark:?}");
    mark
}

/// The balance document marking `tenant` exhausted with `message`.
fn exhausted_doc(tenant: Uuid, message: &str) -> Value {
    json!({
        "scope": "team", "scope_ref": tenant.to_string(), "period": "day",
        "exhausted": {"reason": {
            "message": message, "scope": "team", "scope_ref": tenant.to_string(),
        }},
    })
}

/// The balance document of `tenant` with no mark.
fn open_doc(tenant: Uuid) -> Value {
    json!({"scope": "team", "scope_ref": tenant.to_string(), "period": "day"})
}

/// Waits until `tenant`'s balance document is `want` and its flag is
/// cleared; fails after 5 s.
async fn until_marked(db: &Db, tenant: Uuid, want: &Value) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let doc = mark(db, tenant).await.map(|m| m.doc);
        let dirty = account(db, tenant).await.marks_dirty;
        if doc.as_ref() == Some(want) && !dirty {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "tenant {tenant}'s balance document is {doc:?} (flagged: {dirty}), not {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Sets `tenant`'s balance (and its credit limit, when given) by raw SQL,
/// as an approved top-up or the credit-limit route will, without
/// flagging the account.
async fn set_account(db: &Db, tenant: Uuid, balance: i64, credit_limit: Option<i64>) {
    sqlx::query(
        "UPDATE billing_accounts SET balance_micro_usd = $2, \
           credit_limit_micro_usd = COALESCE($3, credit_limit_micro_usd) \
         WHERE tenant_id = $1",
    )
    .bind(tenant)
    .bind(balance)
    .bind(credit_limit)
    .execute(db.pool())
    .await
    .expect("set the account");
}

/// Flags `tenant`'s account by raw SQL, with no notification.
async fn flag(db: &Db, tenant: Uuid) {
    sqlx::query("UPDATE billing_accounts SET marks_dirty = true WHERE tenant_id = $1")
        .bind(tenant)
        .execute(db.pool())
        .await
        .expect("flag the account");
}

fn copy(ctx: &WriteCtx) -> WriteCtx {
    WriteCtx {
        mk: MasterKey(ctx.mk.0),
        env: ctx.env.clone(),
        cp_version: ctx.cp_version.clone(),
        stale_secs: ctx.stale_secs,
    }
}

async fn settle(cp: &TestCp, tenant: Uuid) {
    marks::settle(cp.db(), &WriteCtx::of(cp.state()), tenant)
        .await
        .expect("settle the tenant's marks");
}

/// `method path` as `admin`, with `If-Match: "<version>"` and a JSON body.
async fn send_at(
    cp: &TestCp,
    admin: &OperatorSession,
    method: reqwest::Method,
    path: &str,
    version: i64,
    body: Option<Value>,
) -> Resp {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let mut request = client
        .request(method, cp.url(path))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("send the request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("read the body");
    Resp {
        status,
        body: serde_json::from_str(&text).unwrap_or(Value::String(text)),
        set_cookie: None,
        headers: reqwest::header::HeaderMap::new(),
    }
}

/// `PATCH /admin/tenants/{id}` with `body`, at the tenant's current version.
async fn patch_tenant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, body: Value) -> Resp {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the version");
    let path = format!("/admin/tenants/{tenant}");
    send_at(
        cp,
        admin,
        reqwest::Method::PATCH,
        &path,
        version,
        Some(body),
    )
    .await
}

/// `POST /admin/tenants/{id}/{action}`, which must answer 200.
async fn act(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, action: &str) {
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/{action}"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{action} {tenant}: {r:?}");
}

/// A tenant signed up through `POST /api/signup`: pending.
async fn signed_up(cp: &TestCp, org: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/api/signup",
            None,
            Some(json!({
                "org_name": org,
                "email": format!("founder-{}@tenant.test", Uuid::new_v4().simple()),
                "password": TEST_PASSWORD,
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["tenant"]["id"].as_str().expect("a tenant id")).expect("a uuid")
}

/// Key `id`'s document's `disabled`, and the `seq` of its last write.
async fn key_state(db: &Db, id: &str) -> (Value, i64) {
    let (doc, seq): (Value, i64) = sqlx::query_as(
        "SELECT doc, seq FROM gw_resources WHERE kind = 'api_keys' AND id = $1::uuid",
    )
    .bind(id)
    .fetch_one(db.pool())
    .await
    .expect("read the key document");
    (doc["disabled"].clone(), seq)
}

/// `n` usage rows of the world's key, 680 µ$ each, named `<tag>-<i>`.
fn rows(w: &World, model: Uuid, tag: &str, n: usize) -> Vec<Value> {
    (1..=n)
        .map(|i| usage(w, &format!("{tag}-{i}"), model, tokens_680()))
        .collect()
}

/// A migrated database with the internal tenant and the tenant defaults, and
/// no control plane on it, so no settler runs but the test's own.
struct Bare {
    bare: BareDatabase,
    ctx: WriteCtx,
}

impl Bare {
    async fn new() -> Option<Bare> {
        let bare = BareDatabase::new().await?;
        let ctx = WriteCtx {
            mk: MasterKey(TEST_MASTER_KEY),
            env: bare.env.clone(),
            cp_version: aisix_core::HEARTBEAT_VERSION.to_string(),
            stale_secs: 60.0,
        };
        tenants::ensure_internal_tenant(&bare.db, &ctx)
            .await
            .expect("the internal tenant");
        Some(Bare { bare, ctx })
    }

    fn db(&self) -> &Db {
        &self.bare.db
    }

    /// An active prepaid tenant, created as an operator creates one, with
    /// `balance` µ$ and its mark settled for it.
    async fn tenant(&self, balance: i64) -> Uuid {
        let ctx = copy(&self.ctx);
        let created = self
            .bare
            .db
            .tx(move |conn| {
                async move {
                    let name = format!("marks-{}", Uuid::new_v4().simple());
                    tenants::create(
                        conn,
                        &ctx,
                        &name,
                        TenantStatus::Active,
                        BillingMode::Prepaid,
                        Actor::System,
                    )
                    .await
                }
                .boxed()
            })
            .await
            .expect("create a tenant");
        set_account(self.db(), created.id, balance, None).await;
        marks::settle(self.db(), &self.ctx, created.id)
            .await
            .expect("settle");
        created.id
    }

    async fn settle_dirty(&self) -> u64 {
        settler::settle_dirty(self.db(), &self.ctx)
            .await
            .expect("a settler pass")
    }
}

#[tokio::test]
async fn an_approved_prepaid_tenant_starts_exhausted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = signed_up(&cp, "marks-prepaid-7c1").await;
    assert!(
        mark(cp.db(), tenant).await.is_none(),
        "a pending tenant is marked"
    );
    act(&cp, &admin, tenant, "approve").await;
    let mark = live_mark(cp.db(), tenant).await;
    assert_eq!(mark.managed_by, Some(format!("billing:{tenant}")));
    assert_eq!(mark.doc, exhausted_doc(tenant, BALANCE_MESSAGE_PREPAID));
    let account = account(cp.db(), tenant).await;
    assert_eq!(
        (account.marked_exhausted, account.marks_dirty),
        (Some(true), false)
    );
}

#[tokio::test]
async fn an_approved_postpaid_tenant_with_no_credit_limit_starts_exhausted() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = signed_up(&cp, "marks-postpaid-7c1").await;
    let r = patch_tenant(&cp, &admin, tenant, json!({"billing_mode": "postpaid"})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        mark(cp.db(), tenant).await.is_none(),
        "a pending tenant is marked"
    );
    act(&cp, &admin, tenant, "approve").await;
    let mark = live_mark(cp.db(), tenant).await;
    assert_eq!(mark.managed_by, Some(format!("billing:{tenant}")));
    assert_eq!(mark.doc, exhausted_doc(tenant, BALANCE_MESSAGE_POSTPAID));
}

#[tokio::test]
async fn reactivating_a_tenant_writes_its_balance_mark_before_its_keys_open() {
    let Some(w) = world().await else { return };
    set_account(w.cp.db(), w.tenant, 1_000, None).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));
    act(&w.cp, &w.admin, w.tenant, "suspend").await;
    assert_eq!(key_state(w.cp.db(), &w.key).await.0, json!(true));
    // The money ran out while the tenant was suspended, and nothing has
    // settled its mark since.
    set_account(w.cp.db(), w.tenant, 0, None).await;

    act(&w.cp, &w.admin, w.tenant, "reactivate").await;
    let mark = live_mark(w.cp.db(), w.tenant).await;
    assert_eq!(mark.doc, exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID));
    let (disabled, key_seq) = key_state(w.cp.db(), &w.key).await;
    assert_eq!(disabled, json!(false));
    assert!(
        mark.seq < key_seq,
        "the key opened (seq {key_seq}) before the balance mark was written (seq {})",
        mark.seq
    );
}

#[tokio::test]
async fn a_postpaid_tenant_is_marked_once_it_spends_its_credit_limit() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "postpaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    set_account(w.cp.db(), w.tenant, 0, Some(10_000)).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    let r = deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "within", 14)).await;
    assert_eq!(r.status, 200, "{r:?}");
    let within = account(w.cp.db(), w.tenant).await;
    assert_eq!(
        (within.balance_micro_usd, within.marks_dirty),
        (-9_520, false)
    );
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    let r = deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "past", 1)).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        account(w.cp.db(), w.tenant).await.balance_micro_usd,
        -10_200
    );
    until_marked(
        w.cp.db(),
        w.tenant,
        &exhausted_doc(w.tenant, BALANCE_MESSAGE_POSTPAID),
    )
    .await;
}

#[tokio::test]
async fn a_batch_that_crosses_the_floor_wakes_the_settler() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    set_account(w.cp.db(), w.tenant, 1_000, None).await;
    settle(&w.cp, w.tenant).await;
    let mut listener = PgListener::connect(w.cp.database_url())
        .await
        .expect("connect a listener");
    listener
        .listen(marks::DIRTY_CHANNEL)
        .await
        .expect("listen for flags");
    let r = deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "cross", 2)).await;
    assert_eq!(r.status, 200, "{r:?}");
    let woken = tokio::time::timeout(Duration::from_secs(2), listener.recv())
        .await
        .expect("the flag is notified")
        .expect("a notification");
    assert_eq!(woken.channel(), marks::DIRTY_CHANNEL);
}

#[tokio::test]
async fn switching_to_prepaid_with_a_negative_balance_marks_the_tenant() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "postpaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    set_account(w.cp.db(), w.tenant, 0, Some(10_000)).await;
    settle(&w.cp, w.tenant).await;
    let r = deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "spent", 1)).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(account(w.cp.db(), w.tenant).await.balance_micro_usd, -680);
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "prepaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    // In that request: read right after the answer.
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );
}

#[tokio::test]
async fn the_sweep_settles_an_account_whose_notification_was_lost() {
    let Some(b) = Bare::new().await else { return };
    let tenant = b.tenant(1_000).await;
    assert_eq!(live_mark(b.db(), tenant).await.doc, open_doc(tenant));
    set_account(b.db(), tenant, -1, None).await;
    flag(b.db(), tenant).await;
    assert_eq!(b.settle_dirty().await, 1);
    assert_eq!(
        live_mark(b.db(), tenant).await.doc,
        exhausted_doc(tenant, BALANCE_MESSAGE_PREPAID)
    );
    let account = account(b.db(), tenant).await;
    assert_eq!(
        (account.marked_exhausted, account.marks_dirty),
        (Some(true), false)
    );
    assert_eq!(
        b.settle_dirty().await,
        0,
        "a settled account is settled again"
    );
}

#[tokio::test]
async fn a_running_control_plane_sweeps_without_a_notification() {
    let Some(w) = world().await else { return };
    set_account(w.cp.db(), w.tenant, 1_000, None).await;
    settle(&w.cp, w.tenant).await;
    set_account(w.cp.db(), w.tenant, -1, None).await;
    flag(w.cp.db(), w.tenant).await;
    until_marked(
        w.cp.db(),
        w.tenant,
        &exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_settler_skips_a_tenant_another_transaction_holds() {
    let Some(b) = Bare::new().await else { return };
    let (a, other) = (b.tenant(1_000).await, b.tenant(1_000).await);
    for t in [a, other] {
        set_account(b.db(), t, -1, None).await;
        flag(b.db(), t).await;
    }
    let mut edit = b.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(a)
        .execute(&mut *edit)
        .await
        .expect("hold the tenant");
    let settled = tokio::time::timeout(Duration::from_secs(1), b.settle_dirty())
        .await
        .expect("the pass does not wait on a held tenant");
    assert_eq!(settled, 1);
    assert_eq!(
        live_mark(b.db(), other).await.doc,
        exhausted_doc(other, BALANCE_MESSAGE_PREPAID)
    );
    assert!(
        account(b.db(), a).await.marks_dirty,
        "the held tenant was settled"
    );
    assert_eq!(live_mark(b.db(), a).await.doc, open_doc(a));

    edit.rollback().await.expect("end the edit");
    assert_eq!(b.settle_dirty().await, 1);
    assert_eq!(
        live_mark(b.db(), a).await.doc,
        exhausted_doc(a, BALANCE_MESSAGE_PREPAID)
    );
    assert!(!account(b.db(), a).await.marks_dirty);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_never_waits_on_the_resources_lock() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    set_account(w.cp.db(), w.tenant, 1_000, None).await;
    settle(&w.cp, w.tenant).await;

    // A long write of gateway documents, as every one takes the lock.
    let mut writer = w.cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .expect("hold the resources lock");
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "cross", 2)),
    )
    .await
    .expect("the batch is answered while the resources lock is held");
    assert_eq!(r.status, 200, "{r:?}");
    let crossed = account(w.cp.db(), w.tenant).await;
    assert_eq!(
        (crossed.balance_micro_usd, crossed.marks_dirty),
        (-360, true)
    );
    // The settler does not wait for the lock either, so it holds no account
    // that intake would then wait for.
    let settled = tokio::time::timeout(
        Duration::from_secs(2),
        settler::settle_dirty(w.cp.db(), &WriteCtx::of(w.cp.state())),
    )
    .await
    .expect("a settler pass does not wait on the resources lock")
    .expect("a settler pass");
    assert_eq!(settled, 0);
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "more", 1)),
    )
    .await
    .expect("a later batch is answered while the resources lock is held");
    assert_eq!(r.status, 200, "{r:?}");
    assert!(account(w.cp.db(), w.tenant).await.marks_dirty);

    writer.rollback().await.expect("end the write");
    until_marked(
        w.cp.db(),
        w.tenant,
        &exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID),
    )
    .await;
}

#[tokio::test]
async fn the_internal_tenant_is_never_marked() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the internal tenant");
    let key = seed_key(&w.cp, &w.admin, internal, None).await;
    let mut event = usage(&w, "ops-1", model, tokens_680());
    event["team_id"] = json!(internal.to_string());
    event["api_key_id"] = json!(key);
    event["user_id"] = json!("");
    let r = deliver(&w.dp, Uuid::new_v4(), vec![event]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let account = account(w.cp.db(), internal).await;
    assert_eq!(
        (account.balance_micro_usd, account.marks_dirty),
        (-680, false)
    );
    let owned: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE managed_by = $1")
        .bind(format!("billing:{internal}"))
        .fetch_one(w.cp.db().pool())
        .await
        .expect("count the documents");
    assert_eq!(owned, 0);
}

#[tokio::test]
async fn an_account_opened_late_gets_its_first_balance_mark() {
    let Some(b) = Bare::new().await else { return };
    let tenant = b.tenant(0).await;
    // A tenant an older release created: it has no account.
    sqlx::query("DELETE FROM billing_accounts WHERE tenant_id = $1")
        .bind(tenant)
        .execute(b.db().pool())
        .await
        .expect("delete the account");
    let mut conn = b.db().pool().acquire().await.expect("a connection");
    assert_eq!(
        ledger::backfill_accounts(&mut conn)
            .await
            .expect("backfill"),
        1
    );
    assert!(account(b.db(), tenant).await.marks_dirty);
    assert!(mark(b.db(), tenant).await.is_none());
    assert_eq!(b.settle_dirty().await, 1);
    assert_eq!(
        live_mark(b.db(), tenant).await.doc,
        exhausted_doc(tenant, BALANCE_MESSAGE_PREPAID)
    );
}

/// Sessions of this test's database waiting for a lock.
async fn lock_waiters(db: &Db) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(db.pool())
    .await
    .expect("count lock waiters")
}

/// Whether some other transaction holds the resources lock right now.
async fn resources_lock_held(db: &Db) -> bool {
    let mut probe = db.pool().begin().await.expect("begin");
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .fetch_one(&mut *probe)
        .await
        .expect("try the resources lock");
    probe.rollback().await.expect("end the probe");
    !free
}

/// Sends `request` while another transaction holds `tenant`'s billing
/// account, and requires that it waits for the account, and that it holds
/// no resources lock while it does: an account is taken right after its
/// tenant's row, before anything that writes a gateway document. Then lets
/// it finish; its status.
async fn waits_for_the_account_first(
    cp: &TestCp,
    tenant: Uuid,
    what: &str,
    request: impl std::future::Future<Output = Resp> + Send + 'static,
) -> u16 {
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the account");
    let request = tokio::spawn(request);
    let deadline = Instant::now() + Duration::from_secs(20);
    while lock_waiters(cp.db()).await < 1 {
        assert!(
            !request.is_finished() && Instant::now() < deadline,
            "{what} never waited for the account"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let inverted = resources_lock_held(cp.db()).await;
    holder.rollback().await.expect("release the account");
    let r = request.await.expect("join");
    assert!(
        !inverted,
        "{what} took the resources lock before the account: the lock order is inverted"
    );
    r.status
}

/// Every path that projects a tenant decides its balance mark, so it takes
/// the tenant's account before the resources lock, which the projection's
/// first write takes. Each request below is sent while the account is held
/// elsewhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_marking_path_takes_the_account_before_the_resources_lock() {
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let cp = Arc::new(cp);
    let admin = Arc::new(cp.admin_login_seeded(OperatorRole::Operator).await);
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let pending = signed_up(&cp, "marks-order-7c1").await;
    let model = seed_model(&cp, &admin).await;
    let r = patch_tenant(&cp, &admin, tenant, json!({"allowed_model_ids": [model]})).await;
    assert_eq!(r.status, 200, "{r:?}");

    type Request = std::pin::Pin<Box<dyn std::future::Future<Output = Resp> + Send>>;
    let patch = |body: Value| -> Request {
        let (cp, admin) = (cp.clone(), admin.clone());
        Box::pin(async move { patch_tenant(&cp, &admin, tenant, body).await })
    };
    let post = |path: String| -> Request {
        let (cp, admin) = (cp.clone(), admin.clone());
        Box::pin(async move { cp.send("POST", &path, Some(admin.cookie()), None).await })
    };
    let cases: Vec<(&str, Uuid, Request, u16)> = vec![
        // A grant change takes the resources lock before it projects.
        (
            "PATCH /admin/tenants/{id} (grants)",
            tenant,
            patch(json!({"allowed_model_ids": [model]})),
            200,
        ),
        (
            "PATCH /admin/tenants/{id} (billing mode)",
            tenant,
            patch(json!({"billing_mode": "postpaid"})),
            200,
        ),
        (
            "POST /admin/tenants/{id}/suspend",
            tenant,
            post(format!("/admin/tenants/{tenant}/suspend")),
            200,
        ),
        (
            "POST /admin/tenants/{id}/reactivate",
            tenant,
            post(format!("/admin/tenants/{tenant}/reactivate")),
            200,
        ),
        (
            "POST /admin/tenants/{id}/approve",
            pending,
            post(format!("/admin/tenants/{pending}/approve")),
            200,
        ),
        // A model delete re-projects every tenant it is withdrawn from.
        (
            "DELETE /admin/resources/models/{id}",
            tenant,
            {
                let (cp, admin) = (cp.clone(), admin.clone());
                let path = format!("/admin/resources/models/{model}");
                Box::pin(async move {
                    send_at(&cp, &admin, reqwest::Method::DELETE, &path, 1, None).await
                })
            },
            200,
        ),
    ];
    for (what, held, request, expected) in cases {
        let status = waits_for_the_account_first(&cp, held, what, request).await;
        assert_eq!(status, expected, "{what}");
    }
}
