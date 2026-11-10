//! Balance marks: every customer tenant past pending has one `budgets`
//! document over its own team, marked exhausted while its money is out (the
//! balance plus its credit limit at 0, in either billing mode). Tenant
//! projections write it right before the keys; usage intake only flags an
//! account whose balance crosses the floor, never waiting on the resources
//! lock, and the settler writes the mark after the batch commits, skipping
//! a tenant another transaction holds. The internal tenant is never marked.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it, in
//! rows of 680 µ$ (`tokens_680`). Credit limits and balances a test needs
//! are set with raw SQL: their routes come later.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::billing::budgets::{self, BudgetScope, Kind, NewBudget, SetBy};
use aisix_control_plane::billing::ledger;
use aisix_control_plane::billing::ledger::Account;
use aisix_control_plane::billing::marks::{
    self, BALANCE_MESSAGE_POSTPAID, BALANCE_MESSAGE_PREPAID,
};
use aisix_control_plane::billing::settler::{self, Settler, Timing};
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::Db;
use aisix_control_plane::groups::{self, LockMode};
use aisix_control_plane::pricing::rates::{self, NewGroupRate};
use aisix_control_plane::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use aisix_control_plane::tenants::{self, BillingMode, TenantStatus};
use aisix_core::models::BudgetPeriod;
use aisix_pricing::CnyPerUsd;
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::postgres::PgListener;
use support::world::{
    deliver, insert_unpriced_charges, price, seed_key, seed_model, tokens_680, usage, world, World,
};
use support::{BareDatabase, OperatorSession, Resp, TestCp, TEST_MASTER_KEY, TEST_PASSWORD};
use uuid::Uuid;

/// Tenant `tenant`'s billing account.
async fn account(db: &Db, tenant: Uuid) -> Account {
    sqlx::query_as(
        "SELECT tenant_id, balance_micro_cny, credit_limit_micro_cny, balance_budget_id, \
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
        "UPDATE billing_accounts SET balance_micro_cny = $2, \
           credit_limit_micro_cny = COALESCE($3, credit_limit_micro_cny) \
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
async fn a_prepaid_tenant_draws_its_credit_limit_before_it_is_marked() {
    let Some(w) = world().await else { return };
    // ¥100 of credit over a zero balance: the limit is an overdraft, so the
    // tenant has money to spend and no mark is written.
    set_account(w.cp.db(), w.tenant, 0, Some(100_000_000)).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));
    assert_eq!(
        account(w.cp.db(), w.tenant).await.marked_exhausted,
        Some(false)
    );

    // One micro-CNY of credit left is still open.
    set_account(w.cp.db(), w.tenant, -99_999_999, None).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    // Spent to exactly the limit: marked, with the prepaid wording.
    set_account(w.cp.db(), w.tenant, -100_000_000, None).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );
    assert_eq!(
        account(w.cp.db(), w.tenant).await.marked_exhausted,
        Some(true)
    );
}

#[tokio::test]
async fn a_prepaid_tenant_without_credit_is_marked_at_zero_as_before() {
    let Some(w) = world().await else { return };
    // The compatibility statement: a credit limit of 0 leaves prepaid's old
    // floor in place, so a balance of 0 is exhausted and one micro-CNY above
    // is not.
    set_account(w.cp.db(), w.tenant, 1, Some(0)).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    set_account(w.cp.db(), w.tenant, 0, None).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );
}

#[tokio::test]
async fn a_billing_mode_change_rewrites_the_balance_mark_in_the_same_request() {
    let Some(w) = world().await else { return };
    // ¥50 of credit spent to the floor: exhausted, in either mode.
    set_account(w.cp.db(), w.tenant, -50_000_000, Some(50_000_000)).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );

    // The mode change rewrites the mark to the new mode's wording, and the
    // answer is not sent until it has.
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "postpaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["billing_mode"], "postpaid", "{r:?}");
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_POSTPAID)
    );
    // And back again, the same transaction each time.
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "prepaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );
}

#[tokio::test]
async fn the_credit_limit_decides_the_mark_in_prepaid_exactly_as_in_postpaid() {
    let Some(w) = world().await else { return };
    // The same money under each mode, written through the mode route, decides
    // the same mark. Prepaid used to ignore its limit entirely, so a prepaid
    // balance below 0 was marked however much credit it held.
    for mode in ["prepaid", "postpaid"] {
        set_account(w.cp.db(), w.tenant, -50_000_000, Some(100_000_000)).await;
        let r = patch_tenant(&w.cp, &w.admin, w.tenant, json!({"billing_mode": mode})).await;
        assert_eq!(r.status, 200, "{mode}: {r:?}");
        assert_eq!(
            live_mark(w.cp.db(), w.tenant).await.doc,
            open_doc(w.tenant),
            "{mode}: ¥50 of credit left is money to spend"
        );

        set_account(w.cp.db(), w.tenant, -100_000_000, None).await;
        let r = patch_tenant(&w.cp, &w.admin, w.tenant, json!({"billing_mode": mode})).await;
        assert_eq!(r.status, 200, "{mode}: {r:?}");
        assert!(
            live_mark(w.cp.db(), w.tenant).await.doc["exhausted"].is_object(),
            "{mode}"
        );
        assert_eq!(
            account(w.cp.db(), w.tenant).await.marked_exhausted,
            Some(true),
            "{mode}: ¥100 of credit spent to the floor is exhausted"
        );
    }
}

#[tokio::test]
async fn a_mark_the_new_floor_no_longer_justifies_is_lifted_in_the_same_request() {
    let Some(w) = world().await else { return };
    // Prepaid at 0 with no credit is exhausted...
    set_account(w.cp.db(), w.tenant, 0, Some(0)).await;
    settle(&w.cp, w.tenant).await;
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );
    // ...and a mode change on money that no longer justifies a mark lifts
    // it: both modes hold the same floor, so the mark follows the money
    // however it got there.
    set_account(w.cp.db(), w.tenant, 0, Some(1_000_000)).await;
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "postpaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));
    assert_eq!(
        account(w.cp.db(), w.tenant).await.marked_exhausted,
        Some(false)
    );
}

#[tokio::test]
async fn a_mode_change_never_marks_the_internal_tenant() {
    let Some(w) = world().await else { return };
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the internal tenant");
    // The platform's own tenant runs a negative balance postpaid and is
    // never marked; changing its mode must not change that.
    let before = account(w.cp.db(), internal).await;
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        internal,
        json!({"billing_mode": "prepaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let after = account(w.cp.db(), internal).await;
    assert_eq!(
        (after.balance_micro_cny, after.marks_dirty),
        (before.balance_micro_cny, false)
    );
    let owned: i64 = sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE managed_by = $1")
        .bind(format!("billing:{internal}"))
        .fetch_one(w.cp.db().pool())
        .await
        .expect("count the documents");
    assert_eq!(owned, 0);
    assert!(mark(w.cp.db(), internal).await.is_none());
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
        (within.balance_micro_cny, within.marks_dirty),
        (-9_520, false)
    );
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    let r = deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "past", 1)).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        account(w.cp.db(), w.tenant).await.balance_micro_cny,
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
async fn switching_to_prepaid_keeps_the_floor_the_credit_limit_gives() {
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
    // ¥0.01 of credit spent a little past: still inside it, so no mark.
    set_account(w.cp.db(), w.tenant, 0, Some(10_000)).await;
    settle(&w.cp, w.tenant).await;
    let r = deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "spent", 1)).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(account(w.cp.db(), w.tenant).await.balance_micro_cny, -680);
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));

    // The credit limit is an overdraft in either mode, so the switch to
    // prepaid leaves the tenant inside its money and unmarked. It used to
    // mark it at once, at a balance of 0 whatever the limit.
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "prepaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(live_mark(w.cp.db(), w.tenant).await.doc, open_doc(w.tenant));
    assert_eq!(
        account(w.cp.db(), w.tenant).await.marked_exhausted,
        Some(false)
    );

    // The same switch on the same money with no credit, however: the prepaid
    // floor of old is the limit-0 floor of new.
    set_account(w.cp.db(), w.tenant, -680, Some(0)).await;
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "postpaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_POSTPAID)
    );
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"billing_mode": "prepaid"}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
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
        (crossed.balance_micro_cny, crossed.marks_dirty),
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

/// The group a charge is filed under is referenced by a foreign key, so each
/// charge insert takes `FOR KEY SHARE` on its group's row. A write that holds
/// the row while it changes it — a retire queued behind the resources lock,
/// an edit, a rate write — must not make intake wait: the batch for a key in
/// that group is answered, and its charge filed, while the write is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_group_write_never_makes_intake_wait() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let shared = support::world::group_id(&w.cp, support::world::SHARED_GROUP).await;
    let filed = |event_id: String| {
        let db = w.cp.db().clone();
        async move {
            sqlx::query_as::<_, (String, Option<Uuid>)>(
                "SELECT status, group_id FROM pricing_charges_cny WHERE event_id = $1",
            )
            .bind(event_id)
            .fetch_one(db.pool())
            .await
            .expect("the charge")
        }
    };

    // A retire of the key's group, queued for the resources lock while it
    // holds the group's row.
    let mut writer = w.cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .expect("hold the resources lock");
    let path = format!("/admin/provider-groups/{shared}/retire");
    let retire =
        w.cp.send("POST", &path, Some(w.admin.cookie()), Some(json!({})));
    let probe = async {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !resources_lock_queued(w.cp.db()).await {
            assert!(
                Instant::now() < deadline,
                "the retire never queued for the resources lock"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let delivered = tokio::time::timeout(
            Duration::from_secs(2),
            deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "retiring", 1)),
        )
        .await;
        writer.rollback().await.expect("end the write");
        delivered
    };
    let (retired, delivered) = tokio::join!(retire, probe);
    let delivered = delivered.expect("the batch is answered while a retire holds its group");
    assert_eq!(delivered.status, 200, "{delivered:?}");
    // The retire then counts the tenant naming the group, and refuses.
    assert_eq!(retired.status, 409, "{retired:?}");
    assert_eq!(
        filed("retiring-1".to_string()).await,
        ("charged".to_string(), Some(shared))
    );

    // An edit's lock (a `PATCH`, and `make-default`'s mode), held open.
    let mut editor = w.cp.db().pool().begin().await.expect("begin");
    groups::lock_one(&mut editor, shared, LockMode::Update)
        .await
        .expect("hold the group as an edit does");
    let delivered = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "editing", 1)),
    )
    .await
    .expect("the batch is answered while an edit holds its group");
    assert_eq!(delivered.status, 200, "{delivered:?}");
    editor.rollback().await.expect("end the edit");
    assert_eq!(
        filed("editing-1".to_string()).await,
        ("charged".to_string(), Some(shared))
    );

    // A rate write on the group, written and not yet committed.
    let mut rater = w.cp.db().pool().begin().await.expect("begin");
    let row = NewGroupRate {
        group_id: shared,
        model_id: None,
        effective_from: support::world::db_now(&w.cp).await + chrono::TimeDelta::hours(1),
        cny_per_usd: Some(CnyPerUsd::parse("2").expect("a rate")),
        note: "held open".to_string(),
    };
    rates::insert_group_rate(&mut rater, &row, Actor::System)
        .await
        .expect("write a rate");
    let delivered = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "rating", 1)),
    )
    .await
    .expect("the batch is answered while a rate write holds its group");
    assert_eq!(delivered.status, 200, "{delivered:?}");
    rater.rollback().await.expect("end the rate write");
    assert_eq!(
        filed("rating-1".to_string()).await,
        ("charged".to_string(), Some(shared))
    );
}

/// What is logged at WARN and above on the thread that set it up.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the log buffer")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    /// Captures this thread's WARN and ERROR lines until the guard drops.
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || logs.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// The captured lines that contain every one of `parts`.
    fn lines_with(&self, parts: &[&str]) -> Vec<String> {
        let bytes = self.0.lock().expect("the log buffer").clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| parts.iter().all(|part| line.contains(part)))
            .map(str::to_string)
            .collect()
    }
}

/// Takes the resources lock in short transactions back to back, as steady
/// gateway-document writes do, until `stop` is set; how many it took.
async fn write_steadily(db: Db, stop: Arc<AtomicBool>) -> u64 {
    let mut writes = 0;
    while !stop.load(Ordering::Relaxed) {
        let mut tx = db.pool().begin().await.expect("begin");
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(RESOURCES_LOCK_KEY)
            .execute(&mut *tx)
            .await
            .expect("take the resources lock");
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.commit().await.expect("commit");
        writes += 1;
    }
    writes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mark_lands_while_document_writes_keep_the_resources_lock_busy() {
    let Some(b) = Bare::new().await else { return };
    let tenant = b.tenant(1_000).await;
    set_account(b.db(), tenant, -1, None).await;
    flag(b.db(), tenant).await;
    // Two writers taking turns: whichever commits, the other is already
    // queued and takes the lock at once, so a try never finds it free.
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..2)
        .map(|_| tokio::spawn(write_steadily(b.db().clone(), stop.clone())))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !resources_lock_queued(b.db()).await {
        assert!(Instant::now() < deadline, "the writers never queued");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let timing = Timing {
        escalate_after: Duration::from_secs(1),
        ..Timing::default()
    };
    let mut settler = Settler::new(b.db().clone(), copy(&b.ctx), timing);
    // The passes run on this thread; their log lines are captured.
    let logs = Logs::default();
    let capture = logs.capture();
    let started = Instant::now();
    while account(b.db(), tenant).await.marks_dirty {
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "the mark never landed while document writes kept the resources lock busy"
        );
        settler.pass().await.expect("a pass");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    drop(capture);
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        let writes = writer.await.expect("a writer");
        assert!(writes >= 10, "a writer took the lock only {writes} times");
    }
    // The settle that queued said so, once, naming the tenant.
    let warned = logs.lines_with(&["WARN", &tenant.to_string(), "now queues for it"]);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(
        live_mark(b.db(), tenant).await.doc,
        exhausted_doc(tenant, BALANCE_MESSAGE_PREPAID)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_escalated_settle_never_holds_up_intake() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    set_account(w.cp.db(), w.tenant, 1_000, None).await;
    settle(&w.cp, w.tenant).await;
    // A long document write, taken before the flag so that no settler
    // settles the tenant first.
    let mut writer = w.cp.db().pool().begin().await.expect("begin");
    let writer_pid = backend_pid(&mut writer).await;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .expect("hold the resources lock");
    set_account(w.cp.db(), w.tenant, -1, None).await;
    flag(w.cp.db(), w.tenant).await;

    let timing = Timing {
        escalate_after: Duration::ZERO,
        queued_wait: Duration::from_secs(5),
        ..Timing::default()
    };
    let mut settler = Settler::new(w.cp.db().clone(), WriteCtx::of(w.cp.state()), timing);
    assert_eq!(settler.pass().await.expect("a pass"), 0, "the lock is busy");
    // This pass queues for the lock.
    let queued = tokio::spawn(async move { settler.pass().await });
    let deadline = Instant::now() + Duration::from_secs(4);
    while blocked_by(w.cp.db(), writer_pid).await.is_empty() {
        assert!(
            !queued.is_finished() && Instant::now() < deadline,
            "the escalated settle never queued for the resources lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r = tokio::time::timeout(
        Duration::from_secs(1),
        deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "queued", 1)),
    )
    .await
    .expect("intake is answered while a settle is queued for the resources lock");
    assert_eq!(r.status, 200, "{r:?}");

    writer.commit().await.expect("end the write");
    let settled = queued.await.expect("the settle task").expect("a pass");
    assert_eq!(settled, 1);
    assert_eq!(
        live_mark(w.cp.db(), w.tenant).await.doc,
        exhausted_doc(w.tenant, BALANCE_MESSAGE_PREPAID)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_escalated_settle_never_waits_for_the_account_while_it_holds_the_resources_lock() {
    let Some(b) = Bare::new().await else { return };
    let tenant = b.tenant(1_000).await;
    set_account(b.db(), tenant, -1, None).await;
    flag(b.db(), tenant).await;
    let timing = Timing {
        escalate_after: Duration::ZERO,
        queued_wait: Duration::from_secs(5),
        ..Timing::default()
    };
    let mut settler = Settler::new(b.db().clone(), copy(&b.ctx), timing);
    let mut writer = b.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .expect("hold the resources lock");
    assert_eq!(settler.pass().await.expect("a pass"), 0, "the lock is busy");
    writer.rollback().await.expect("end the write");
    let mut intake = b.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *intake)
        .await
        .expect("hold the account");
    let started = Instant::now();
    assert_eq!(
        settler.pass().await.expect("a pass"),
        0,
        "intake holds the account"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the escalated settle waited {:?} for the account while it held the resources lock",
        started.elapsed()
    );
    intake.rollback().await.expect("end intake");
    assert_eq!(settler.pass().await.expect("a pass"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_left_flagged_never_keeps_a_pass_from_the_others() {
    let Some(b) = Bare::new().await else { return };
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(b.tenant(1_000).await);
    }
    ids.sort();
    for &t in &ids {
        set_account(b.db(), t, -1, None).await;
        flag(b.db(), t).await;
    }
    // The two lowest ids, a full page, cannot be settled now.
    let mut edit = b.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = ANY($1) FOR UPDATE")
        .bind(&ids[..2])
        .execute(&mut *edit)
        .await
        .expect("hold two tenants");
    let timing = Timing {
        page: 2,
        ..Timing::default()
    };
    let mut settler = Settler::new(b.db().clone(), copy(&b.ctx), timing);
    assert_eq!(settler.pass().await.expect("a pass"), 1);
    assert!(!account(b.db(), ids[2]).await.marks_dirty);
    for &t in &ids[..2] {
        assert!(account(b.db(), t).await.marks_dirty, "{t}");
    }
    edit.rollback().await.expect("end the edit");
    assert_eq!(settler.pass().await.expect("a pass"), 2);
}

#[tokio::test]
async fn the_internal_tenants_flag_is_cleared_without_a_mark() {
    let Some(b) = Bare::new().await else { return };
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(b.db().pool())
        .await
        .expect("the internal tenant");
    flag(b.db(), internal).await;
    assert_eq!(b.settle_dirty().await, 1);
    assert!(!account(b.db(), internal).await.marks_dirty);
    assert_eq!(b.settle_dirty().await, 0);
    assert!(mark(b.db(), internal).await.is_none());
}

/// A batch that crosses the floor flags the account, and still never waits
/// on its tenant's row: an operator's edit may hold that row across a long
/// write, and a settler holds it while it settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crossing_batch_never_waits_on_a_held_tenant_row() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    set_account(w.cp.db(), w.tenant, 1_000, None).await;
    settle(&w.cp, w.tenant).await;
    // An operator's long tenant edit, as the control plane takes it.
    let mut edit = w.cp.db().pool().begin().await.expect("begin");
    tenants::lock(&mut edit, w.tenant)
        .await
        .expect("lock the tenant");
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "cross", 2)),
    )
    .await
    .expect("the crossing batch is answered while the tenant is locked");
    assert_eq!(r.status, 200, "{r:?}");
    let crossed = account(w.cp.db(), w.tenant).await;
    assert_eq!(
        (crossed.balance_micro_cny, crossed.marks_dirty),
        (-360, true)
    );
    edit.rollback().await.expect("end the edit");
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
        (account.balance_micro_cny, account.marks_dirty),
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

/// The sessions waiting for a lock the session `holder` holds.
async fn blocked_by(db: &Db, holder: i32) -> Vec<i32> {
    sqlx::query_scalar("SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))")
        .bind(holder)
        .fetch_all(db.pool())
        .await
        .expect("read the blocked sessions")
}

/// Whether the session `pid` holds the resources lock: an advisory lock on
/// one `bigint` key is listed as its high and low halves.
async fn holds_resources_lock(db: &Db, pid: i32) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
           AND pid = $1 AND classid::bigint = $2 AND objid::bigint = $3 AND objsubid = 1)",
    )
    .bind(pid)
    .bind(RESOURCES_LOCK_KEY >> 32)
    .bind(RESOURCES_LOCK_KEY & 0xffff_ffff)
    .fetch_one(db.pool())
    .await
    .expect("read the session's locks")
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

/// The backend pid of the session `conn` runs on.
async fn backend_pid(conn: &mut sqlx::PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(conn)
        .await
        .expect("read the backend pid")
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
    let holder_pid = backend_pid(&mut holder).await;
    sqlx::query("SELECT 1 FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the account");
    let request = tokio::spawn(request);
    let deadline = Instant::now() + Duration::from_secs(20);
    // Only a session the held account blocks: a waiter elsewhere in the
    // database says nothing about this request.
    let waiting = loop {
        if let Some(&pid) = blocked_by(cp.db(), holder_pid).await.first() {
            break pid;
        }
        assert!(
            !request.is_finished() && Instant::now() < deadline,
            "{what} never waited for the account"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let inverted = holds_resources_lock(cp.db(), waiting).await;
    holder.rollback().await.expect("release the account");
    let r = request.await.expect("join");
    assert!(
        !inverted,
        "{what} took the resources lock before the account: the lock order is inverted"
    );
    r.status
}

/// A quota write the lock-order census sends.
enum QuotaWrite {
    Create(NewBudget),
    SetLimit(Uuid),
    Delete(Uuid),
}

/// Every path that projects a tenant decides its balance mark, and every
/// quota write its quota marks, so it takes the tenant's account before the
/// resources lock, which the projection's first write takes. Each request
/// below is sent while the account is held elsewhere.
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
    // A quota write, as its route will send it: in a transaction that has
    // taken only the tenant's row (`tenants::lock`) before it, answered with
    // the write's status.
    let organization_quota = |period: BudgetPeriod| NewBudget {
        scope: BudgetScope::Team,
        scope_ref: tenant,
        set_by: SetBy::Operator,
        kind: Kind::Periodic,
        period: Some(period),
        limit_micro_cny: 10_000,
        created_by_kind: "system",
        created_by: None,
    };
    let quota_write = |write: QuotaWrite| -> Request {
        let cp = cp.clone();
        Box::pin(async move {
            let ctx = WriteCtx::of(cp.state());
            let result = cp
                .db()
                .tx(move |conn| {
                    async move {
                        let row = tenants::lock(&mut *conn, tenant).await?;
                        match write {
                            QuotaWrite::Create(new) => {
                                budgets::create(conn, &ctx, &row, new, Actor::System)
                                    .await
                                    .map(drop)
                            }
                            QuotaWrite::SetLimit(id) => {
                                budgets::set_limit(conn, &ctx, &row, id, 20_000, Actor::System)
                                    .await
                                    .map(drop)
                            }
                            QuotaWrite::Delete(id) => {
                                budgets::delete(conn, &ctx, &row, id, Actor::System).await
                            }
                        }
                    }
                    .boxed()
                })
                .await;
            Resp {
                status: result.map_or_else(|e| e.status.as_u16(), |()| 200),
                body: Value::Null,
                set_cookie: None,
                headers: reqwest::header::HeaderMap::new(),
            }
        })
    };
    let existing = {
        let ctx = WriteCtx::of(cp.state());
        let new = organization_quota(BudgetPeriod::Month);
        cp.db()
            .tx(move |conn| {
                async move {
                    let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                    budgets::create(conn, &ctx, &row, new, Actor::System).await
                }
                .boxed()
            })
            .await
            .expect("a quota to edit")
            .id
    };
    // The operator routes' own quotas: one each for the PATCH and the
    // DELETE, a week and a cumulative one, so no route's write meets the
    // quotas the cases above create (a day and a month).
    let (for_patch, for_delete) = {
        let ctx = WriteCtx::of(cp.state());
        let week = organization_quota(BudgetPeriod::Week);
        let total = NewBudget {
            kind: Kind::Cumulative,
            period: None,
            ..organization_quota(BudgetPeriod::Week)
        };
        cp.db()
            .tx(move |conn| {
                async move {
                    let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                    let a = budgets::create(&mut *conn, &ctx, &row, week, Actor::System).await?;
                    let b = budgets::create(&mut *conn, &ctx, &row, total, Actor::System).await?;
                    Ok::<_, aisix_control_plane::error::ApiError>((a.id, b.id))
                }
                .boxed()
            })
            .await
            .expect("quotas for the routes")
    };
    let for_post = support::world::seed_tenant(&cp, &admin).await;
    let route = |method: &'static str, path: String, body: Option<Value>| -> Request {
        let (cp, admin) = (cp.clone(), admin.clone());
        Box::pin(async move { cp.send(method, &path, Some(admin.cookie()), body).await })
    };
    // The tenant's owner and the quotas its routes edit: a member quota for
    // the POST to create, and a week and a cumulative one on a key.
    let owner = Arc::new(cp.member_login_seeded(tenant, MemberRole::Owner).await);
    let own_member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let own_key =
        Uuid::parse_str(&support::world::seed_key(&cp, &admin, tenant, Some(owner.id)).await)
            .expect("a key id");
    let (own_patch, own_delete) = {
        let ctx = WriteCtx::of(cp.state());
        let make = |kind, period| NewBudget {
            scope: BudgetScope::ApiKey,
            scope_ref: own_key,
            set_by: SetBy::OrgAdmin,
            kind,
            period,
            limit_micro_cny: 10_000,
            created_by_kind: "system",
            created_by: None,
        };
        let (week, total) = (
            make(Kind::Periodic, Some(BudgetPeriod::Week)),
            make(Kind::Cumulative, None),
        );
        cp.db()
            .tx(move |conn| {
                async move {
                    let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                    let a = budgets::create(&mut *conn, &ctx, &row, week, Actor::System).await?;
                    let b = budgets::create(&mut *conn, &ctx, &row, total, Actor::System).await?;
                    Ok::<_, aisix_control_plane::error::ApiError>((a.id, b.id))
                }
                .boxed()
            })
            .await
            .expect("quotas for the member routes")
    };
    let member_route = |method: &'static str, path: String, body: Option<Value>| -> Request {
        let (cp, owner) = (cp.clone(), owner.clone());
        Box::pin(async move { cp.send(method, &path, Some(owner.cookie()), body).await })
    };
    // A re-pricing run that charges one of the tenant's unpriced rows,
    // previewed now and applied by an admin as the last case.
    let occurred = chrono::Utc::now() - chrono::TimeDelta::minutes(10);
    insert_unpriced_charges(
        cp.db(),
        "marks-order-7c1",
        1,
        tenant,
        Uuid::new_v4(),
        model,
        occurred,
    )
    .await;
    price(&cp, model).await;
    let r = cp
        .send(
            "POST",
            "/admin/billing/repricing",
            Some(admin.cookie()),
            Some(json!({
                "from": occurred - chrono::TimeDelta::minutes(1),
                "to": occurred + chrono::TimeDelta::minutes(1),
                "reason": "补录价格 7c1",
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["changed"], 1, "{r:?}");
    let run = r.body["id"].as_str().expect("a run id").to_string();
    let applier = Arc::new(cp.admin_login_seeded(OperatorRole::Admin).await);
    // A tenant edit that moves no money and a model delete take no account
    // (`no_edit_that_moves_no_money_waits_on_a_held_account`); their lock
    // order is in `every_tenant_write_takes_the_tenant_before_the_resources_lock`.
    let cases: Vec<(&str, Uuid, Request, u16)> = vec![
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
        (
            "budgets::create",
            tenant,
            quota_write(QuotaWrite::Create(organization_quota(BudgetPeriod::Day))),
            200,
        ),
        (
            "budgets::set_limit",
            tenant,
            quota_write(QuotaWrite::SetLimit(existing)),
            200,
        ),
        (
            "budgets::delete",
            tenant,
            quota_write(QuotaWrite::Delete(existing)),
            200,
        ),
        // The operators' quota routes. The POST has a tenant of its own, so
        // no earlier case frees the slot it takes.
        (
            "POST /admin/tenants/{id}/budgets",
            for_post,
            route(
                "POST",
                format!("/admin/tenants/{for_post}/budgets"),
                Some(json!({"limit_cny": "0.01"})),
            ),
            201,
        ),
        (
            "PATCH /admin/tenants/{id}/budgets/{bid}",
            tenant,
            route(
                "PATCH",
                format!("/admin/tenants/{tenant}/budgets/{for_patch}"),
                Some(json!({"limit_cny": "0.02"})),
            ),
            200,
        ),
        (
            "DELETE /admin/tenants/{id}/budgets/{bid}",
            tenant,
            route(
                "DELETE",
                format!("/admin/tenants/{tenant}/budgets/{for_delete}"),
                None,
            ),
            204,
        ),
        // The tenant's own quota routes, as its owner. Each has a target and
        // a quota of its own, so no case meets another's.
        (
            "POST /api/budgets",
            tenant,
            member_route(
                "POST",
                "/api/budgets".to_string(),
                Some(json!({"scope": "member", "scope_ref": own_member, "limit_cny": "0.01"})),
            ),
            201,
        ),
        (
            "PATCH /api/budgets/{id}",
            tenant,
            member_route(
                "PATCH",
                format!("/api/budgets/{own_patch}"),
                Some(json!({"limit_cny": "0.02"})),
            ),
            200,
        ),
        (
            "DELETE /api/budgets/{id}",
            tenant,
            member_route("DELETE", format!("/api/budgets/{own_delete}"), None),
            204,
        ),
        // A re-pricing run posts to the ledger and settles the marks.
        (
            "POST /admin/billing/repricing/{id}/apply",
            tenant,
            {
                let (cp, applier) = (cp.clone(), applier.clone());
                let path = format!("/admin/billing/repricing/{run}/apply");
                Box::pin(async move {
                    cp.send("POST", &path, Some(applier.cookie()), Some(json!({})))
                        .await
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

/// A tenant edit that moves no money and decides no mark (a rename, a grant,
/// its limits or its groups), and a model delete, which only withdraws a
/// model from the tenants that hold it, leave the tenant's billing account
/// to usage intake: each is answered while another transaction holds it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_edit_that_moves_no_money_waits_on_a_held_account() {
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = support::world::seed_tenant(&cp, &admin).await;
    let model = seed_model(&cp, &admin).await;
    let r = patch_tenant(&cp, &admin, tenant, json!({"allowed_model_ids": [model]})).await;
    assert_eq!(r.status, 200, "{r:?}");
    seed_key(&cp, &admin, tenant, None).await;
    let before = live_mark(cp.db(), tenant).await.doc;
    // A group edit names only live groups.
    support::seed_group(&cp, &admin, "extra-7c1").await;

    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM billing_accounts WHERE tenant_id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the account");
    for (what, body) in [
        ("a rename", json!({"name": "renamed-7c1"})),
        ("a grant", json!({"allowed_model_ids": [model]})),
        ("limits", json!({"limits": {"rpm": 600}})),
        (
            "groups",
            json!({"allowed_provider_groups": ["shared", "extra-7c1"]}),
        ),
    ] {
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            patch_tenant(&cp, &admin, tenant, body),
        )
        .await
        .unwrap_or_else(|_| panic!("PATCH /admin/tenants/{{id}} ({what}) waited for the account"));
        assert_eq!(r.status, 200, "{what}: {r:?}");
    }
    let path = format!("/admin/resources/models/{model}");
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        send_at(&cp, &admin, reqwest::Method::DELETE, &path, 1, None),
    )
    .await
    .expect("DELETE /admin/resources/models/{id} waited for the account");
    assert_eq!(r.status, 200, "{r:?}");
    holder.rollback().await.expect("release the account");
    assert_eq!(live_mark(cp.db(), tenant).await.doc, before);
}

/// While a model delete waits for the resources lock behind another write,
/// a batch charging a tenant that holds the model is stored: the delete
/// holds the tenant's row, which intake never waits on, and not its
/// account.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_delete_never_makes_intake_wait() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = patch_tenant(
        &w.cp,
        &w.admin,
        w.tenant,
        json!({"allowed_model_ids": [model]}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    set_account(w.cp.db(), w.tenant, 1_000_000, None).await;
    settle(&w.cp, w.tenant).await;

    // A long write of gateway documents, as every one takes the lock.
    let mut writer = w.cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .expect("hold the resources lock");
    let path = format!("/admin/resources/models/{model}");
    let delete = send_at(&w.cp, &w.admin, reqwest::Method::DELETE, &path, 1, None);
    let probe = async {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !resources_lock_queued(w.cp.db()).await {
            assert!(
                Instant::now() < deadline,
                "the model delete never queued for the resources lock"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let delivered = tokio::time::timeout(
            Duration::from_secs(2),
            deliver(&w.dp, Uuid::new_v4(), rows(&w, model, "deleting", 1)),
        )
        .await;
        writer.rollback().await.expect("end the write");
        delivered
    };
    let (deleted, delivered) = tokio::join!(delete, probe);
    let delivered = delivered.expect("the batch waited behind the model delete");
    assert_eq!(delivered.status, 200, "{delivered:?}");
    assert_eq!(deleted.status, 200, "{deleted:?}");
    assert_eq!(
        account(w.cp.db(), w.tenant).await.balance_micro_cny,
        1_000_000 - 680
    );
}
