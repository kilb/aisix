//! Quotas: an operator's organization quota, an owner's or admin's member
//! quota, and key quotas from an owner or admin and from the key's member,
//! each periodic (a UTC day, week or month) or cumulative, each its own
//! `budgets` document. Usage intake counts every charge against the quotas
//! it falls under and flags a crossing in its one update of the account,
//! and the settler writes the document with its `exhausted` mark. No level
//! is compared with another.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it, in
//! rows of 680 µ$ (`tokens_680`). Quotas are written through the domain
//! writes; their routes come later. Each tenant is funded through a top-up
//! its owner requests and an operator approves, so its balance never marks
//! it.

mod support;

use std::time::{Duration, Instant};

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::billing::budgets::{
    self, BudgetRow, BudgetScope, ChargedRow, CountMode, Kind, NewBudget, SetBy,
};
use aisix_control_plane::billing::marks;
use aisix_control_plane::db::Db;
use aisix_control_plane::error::{ApiError, Detail};
use aisix_control_plane::resources::write::WriteCtx;
use aisix_control_plane::tenants;
use aisix_core::models::{validate_budget, validate_budget_lenient, Budget, BudgetPeriod};
use chrono::{DateTime, Datelike, Days, NaiveTime, SecondsFormat, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{
    db_now, deliver, insert_sell_backdated, price, seed_key, seed_model, seed_tenant, tokens_680,
    usage, world, World,
};
use support::TestCp;
use uuid::Uuid;

use BudgetPeriod::{Day, Month, Week};

/// A priced world whose tenant is funded: the world's member M and its key.
struct Q {
    w: World,
    model: Uuid,
    key: Uuid,
}

async fn setup() -> Option<Q> {
    let w = world().await?;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    fund(&w.cp, &w, w.tenant).await;
    let key = Uuid::parse_str(&w.key).expect("a key id");
    Some(Q { w, model, key })
}

/// Funds `tenant` with $100: its owner requests a top-up and the world's
/// operator approves it.
async fn fund(cp: &TestCp, w: &World, tenant: Uuid) {
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let r = cp
        .send(
            "POST",
            "/api/billing/topups",
            Some(owner.cookie()),
            Some(json!({"amount_usd": "100"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = r.body["id"].as_str().expect("a top-up id").to_string();
    let r = cp
        .send(
            "POST",
            &format!("/admin/billing/topups/{id}/approve"),
            Some(w.admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// A quota of `limit` µ$: periodic over `period`, or cumulative with none.
fn quota(
    scope: BudgetScope,
    scope_ref: Uuid,
    set_by: SetBy,
    period: Option<BudgetPeriod>,
    limit: i64,
) -> NewBudget {
    NewBudget {
        scope,
        scope_ref,
        set_by,
        kind: match period {
            Some(_) => Kind::Periodic,
            None => Kind::Cumulative,
        },
        period,
        limit_micro_usd: limit,
        created_by_kind: "system",
        created_by: None,
    }
}

/// `budgets::create` in a transaction of its own, under the tenant's
/// billing lock.
async fn create(cp: &TestCp, tenant: Uuid, new: NewBudget) -> Result<BudgetRow, ApiError> {
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
}

async fn created(cp: &TestCp, tenant: Uuid, new: NewBudget) -> BudgetRow {
    create(cp, tenant, new).await.expect("create the quota")
}

async fn set_limit(cp: &TestCp, tenant: Uuid, id: Uuid, limit: i64) -> BudgetRow {
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                budgets::set_limit(conn, &ctx, &row, id, limit, Actor::System).await
            }
            .boxed()
        })
        .await
        .expect("set the limit")
}

async fn delete(cp: &TestCp, tenant: Uuid, id: Uuid) {
    let ctx = WriteCtx::of(cp.state());
    cp.db()
        .tx(move |conn| {
            async move {
                let (row, _) = tenants::lock_billing(&mut *conn, tenant).await?;
                budgets::delete(conn, &ctx, &row, id, Actor::System).await
            }
            .boxed()
        })
        .await
        .expect("delete the quota")
}

/// Quota `id`'s spend now: its current period's for a periodic quota.
async fn spent(cp: &TestCp, tenant: Uuid, id: Uuid) -> i64 {
    let mut conn = cp.db().pool().acquire().await.expect("a connection");
    let row = budgets::get(&mut conn, tenant, id)
        .await
        .expect("read the quota")
        .expect("a live quota");
    budgets::spent(&mut conn, &row)
        .await
        .expect("read its spend")
}

/// The stored `budgets` document `id`: its body, whether it is deleted, and
/// the `seq` of its last write.
async fn doc(db: &Db, id: Uuid) -> Option<(Value, bool, i64)> {
    sqlx::query_as("SELECT doc, deleted, seq FROM gw_resources WHERE kind = 'budgets' AND id = $1")
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .expect("read the document")
}

/// The live document `id`, which must exist.
async fn live(db: &Db, id: Uuid) -> Value {
    let (body, deleted, _) = doc(db, id)
        .await
        .unwrap_or_else(|| panic!("quota {id} has no document"));
    assert!(!deleted, "quota {id}'s document is deleted: {body}");
    body
}

/// Waits until the live document `id` is exhausted, and returns it; fails
/// after 5 s.
async fn until_exhausted(db: &Db, id: Uuid) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((body, false, _)) = doc(db, id).await {
            if body.get("exhausted").is_some() {
                return body;
            }
        }
        assert!(
            Instant::now() < deadline,
            "quota {id}'s document is not exhausted: {:?}",
            doc(db, id).await
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The document of a quota that is not exhausted.
fn open(scope: &str, scope_ref: Uuid, period: &str) -> Value {
    json!({"scope": scope, "scope_ref": scope_ref.to_string(), "period": period})
}

/// Whether `tenant`'s account is flagged for the settler.
async fn flagged(db: &Db, tenant: Uuid) -> bool {
    sqlx::query_scalar("SELECT marks_dirty FROM billing_accounts WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(db.pool())
        .await
        .expect("read the account")
}

/// `tenant`'s balance document: its body and the `seq` of its last write.
async fn balance_doc(db: &Db, tenant: Uuid) -> (Value, i64) {
    let id: Uuid =
        sqlx::query_scalar("SELECT balance_budget_id FROM billing_accounts WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(db.pool())
            .await
            .expect("read the account");
    let (body, _, seq) = doc(db, id).await.expect("a balance document");
    (body, seq)
}

/// UTC midnight of the database's today, as a document writes it.
async fn today(cp: &TestCp) -> DateTime<Utc> {
    db_now(cp)
        .await
        .date_naive()
        .and_time(NaiveTime::MIN)
        .and_utc()
}

fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `n` rows of 680 µ$ on `key`, bound to `member`, named `<tag>-<i>`,
/// occurring at `at`, or now.
fn rows(
    q: &Q,
    key: Uuid,
    member: Option<Uuid>,
    tag: &str,
    n: usize,
    at: Option<DateTime<Utc>>,
) -> Vec<Value> {
    (1..=n)
        .map(|i| {
            let mut event = usage(&q.w, &format!("{tag}-{i}"), q.model, tokens_680());
            event["api_key_id"] = json!(key.to_string());
            event["user_id"] = json!(member.map(|m| m.to_string()).unwrap_or_default());
            if let Some(at) = at {
                event["occurred_at"] = json!(at.to_rfc3339_opts(SecondsFormat::Millis, true));
            }
            event
        })
        .collect()
}

async fn send(q: &Q, events: Vec<Value>) {
    let r = deliver(&q.w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// A key of the world's tenant bound to `member`.
async fn key_of(q: &Q, member: Option<Uuid>) -> Uuid {
    let id = seed_key(&q.w.cp, &q.w.admin, q.w.tenant, member).await;
    Uuid::parse_str(&id).expect("a key id")
}

/// The rate card of `support::world::card()`, for a backdated price.
fn card_json() -> Value {
    json!({"default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}})
}

#[tokio::test]
async fn a_key_quota_marks_that_key_only() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            10_000,
        ),
    )
    .await;
    let balance = balance_doc(db, q.w.tenant).await;
    assert_eq!(live(db, b.id).await, open("api_key", q.key, "day"));

    send(&q, rows(&q, q.key, Some(q.w.member), "within", 14, None)).await;
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 9_520);
    assert!(
        !flagged(db, q.w.tenant).await,
        "a quota within its limit flagged"
    );
    assert_eq!(live(db, b.id).await, open("api_key", q.key, "day"));

    send(&q, rows(&q, q.key, Some(q.w.member), "past", 1, None)).await;
    let marked = until_exhausted(db, b.id).await;
    assert_eq!(marked["scope_ref"], json!(q.key.to_string()));
    assert_eq!(
        marked["exhausted"]["reason"]["scope_ref"],
        json!(q.key.to_string())
    );
    assert_eq!(
        marked["exhausted"]["period_start"],
        json!(stamp(today(cp).await))
    );
    assert_eq!(
        balance_doc(db, q.w.tenant).await,
        balance,
        "the balance document moved"
    );
}

#[tokio::test]
async fn the_quota_document_carries_its_limit_and_spend_exactly() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            10_000,
        ),
    )
    .await;
    send(&q, rows(&q, q.key, Some(q.w.member), "r", 15, None)).await;
    let marked = until_exhausted(db, b.id).await;
    let key = q.key.to_string();
    assert_eq!(
        marked,
        json!({
            "scope": "api_key", "scope_ref": key, "period": "day",
            "exhausted": {
                "period_start": stamp(today(cp).await),
                "reason": {
                    "message": "Budget exceeded: the daily limit of 0.01 USD is reached.",
                    "scope": "api_key", "scope_ref": key,
                    "limit_usd": "0.010000", "spent_usd": "0.010200",
                    "remaining_usd": "-0.000200", "period": "day",
                },
            },
        })
    );
}

#[tokio::test]
async fn an_organization_quota_counts_every_key() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let other = key_of(&q, None).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::Team,
            q.w.tenant,
            SetBy::Operator,
            Some(Month),
            10_000,
        ),
    )
    .await;
    send(&q, rows(&q, q.key, Some(q.w.member), "k1", 8, None)).await;
    send(&q, rows(&q, other, None, "k2", 6, None)).await;
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 9_520);
    assert_eq!(live(db, b.id).await, open("team", q.w.tenant, "month"));

    send(&q, rows(&q, other, None, "k2-past", 1, None)).await;
    let marked = until_exhausted(db, b.id).await;
    assert_eq!(marked["scope"], json!("team"));
    assert_eq!(marked["scope_ref"], json!(q.w.tenant.to_string()));
    assert_eq!(marked["period"], json!("month"));
    let first = today(cp).await.with_day(1).expect("the first of the month");
    assert_eq!(marked["exhausted"]["period_start"], json!(stamp(first)));
}

#[tokio::test]
async fn a_member_quota_counts_every_key_of_that_member() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let m = q.w.member;
    let k2 = key_of(&q, Some(m)).await;
    let n = cp.seed_member(q.w.tenant, MemberRole::Member).await.id;
    let kn = key_of(&q, Some(n)).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(BudgetScope::Member, m, SetBy::OrgAdmin, Some(Day), 10_000),
    )
    .await;

    send(&q, rows(&q, kn, Some(n), "n", 10, None)).await;
    assert_eq!(
        spent(cp, q.w.tenant, b.id).await,
        0,
        "another member's rows counted"
    );

    send(&q, rows(&q, q.key, Some(m), "k1", 8, None)).await;
    send(&q, rows(&q, k2, Some(m), "k2", 6, None)).await;
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 9_520);
    assert_eq!(live(db, b.id).await, open("member", m, "day"));

    send(&q, rows(&q, k2, Some(m), "k2-past", 1, None)).await;
    let marked = until_exhausted(db, b.id).await;
    assert_eq!(marked["scope"], json!("member"));
    assert_eq!(marked["scope_ref"], json!(m.to_string()));
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 10_200);
}

#[tokio::test]
async fn an_owner_cap_and_a_member_cap_on_one_key_are_two_documents() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let owners = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            50_000,
        ),
    )
    .await;
    let members = created(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, q.key, SetBy::Member, Some(Day), 10_000),
    )
    .await;
    assert_ne!(owners.id, members.id);
    assert_eq!(live(db, owners.id).await, open("api_key", q.key, "day"));
    assert_eq!(live(db, members.id).await, open("api_key", q.key, "day"));

    send(&q, rows(&q, q.key, Some(q.w.member), "r", 15, None)).await;
    until_exhausted(db, members.id).await;
    assert_eq!(spent(cp, q.w.tenant, owners.id).await, 10_200);
    assert_eq!(live(db, owners.id).await, open("api_key", q.key, "day"));
}

#[tokio::test]
async fn a_cumulative_quota_counts_from_its_creation_and_holds_with_no_period_start() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    send(&q, rows(&q, q.key, Some(q.w.member), "before", 3, None)).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, q.key, SetBy::Member, None, 10_000),
    )
    .await;
    assert_eq!((b.kind, b.period), (Kind::Cumulative, None));
    assert!(b.counted_from.is_some(), "{b:?}");
    let shown = budgets::to_api(&b, spent(cp, q.w.tenant, b.id).await, "k1", true);
    assert_eq!(shown["spent_micro_usd"], json!(0));
    assert_eq!(shown["period"], Value::Null);
    assert_eq!(shown["period_start"], Value::Null);
    assert!(shown["counted_from"].is_string(), "{shown}");
    assert_eq!(live(db, b.id).await, open("api_key", q.key, "month"));

    send(&q, rows(&q, q.key, Some(q.w.member), "after", 15, None)).await;
    let marked = until_exhausted(db, b.id).await;
    let key = q.key.to_string();
    assert_eq!(
        marked,
        json!({
            "scope": "api_key", "scope_ref": key, "period": "month",
            "exhausted": {"reason": {
                "message": "Quota exhausted: the allocation of 0.01 USD is used up.",
                "scope": "api_key", "scope_ref": key,
                "limit_usd": "0.010000", "spent_usd": "0.010200",
                "remaining_usd": "-0.000200", "period": "total",
            }},
        })
    );
}

#[tokio::test]
async fn a_charge_priced_before_counted_from_is_not_counted() {
    let Some(q) = setup().await else { return };
    let cp = &q.w.cp;
    let now = db_now(cp).await;
    insert_sell_backdated(cp.db(), q.model, card_json(), now - TimeDelta::hours(2)).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, q.key, SetBy::OrgAdmin, None, 1_000_000),
    )
    .await;
    let earlier = now - TimeDelta::hours(1);
    send(
        &q,
        rows(&q, q.key, Some(q.w.member), "late", 1, Some(earlier)),
    )
    .await;
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 0);
    send(&q, rows(&q, q.key, Some(q.w.member), "fresh", 1, None)).await;
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 680);
}

#[tokio::test]
async fn a_periodic_quota_counts_what_its_period_already_spent_when_created() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    send(&q, rows(&q, q.key, Some(q.w.member), "r", 15, None)).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            10_000,
        ),
    )
    .await;
    let spent = spent(cp, q.w.tenant, b.id).await;
    assert_eq!(spent, 10_200);
    let shown = budgets::to_api(&b, spent, "k1", true);
    assert_eq!(shown["exhausted"], json!(true));
    assert_eq!(shown["remaining_micro_usd"], json!(-200));
    // Settled in the creating transaction: no settler pass is needed.
    let marked = live(db, b.id).await;
    assert_eq!(
        marked["exhausted"]["reason"]["spent_usd"],
        json!("0.010200"),
        "{marked}"
    );
}

#[tokio::test]
async fn a_charge_on_the_boundary_counts_in_the_period_it_starts() {
    let Some(q) = setup().await else { return };
    let cp = &q.w.cp;
    let today = today(cp).await;
    let monday = today - Days::new(u64::from(today.weekday().num_days_from_monday()));
    insert_sell_backdated(cp.db(), q.model, card_json(), monday - Days::new(2)).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Week),
            1_000_000,
        ),
    )
    .await;
    let sunday_last = monday - TimeDelta::milliseconds(1);
    send(
        &q,
        rows(&q, q.key, Some(q.w.member), "sun", 1, Some(sunday_last)),
    )
    .await;
    send(
        &q,
        rows(&q, q.key, Some(q.w.member), "mon", 1, Some(monday)),
    )
    .await;
    let spend: Vec<(DateTime<Utc>, i64)> = sqlx::query_as(
        "SELECT period_start, spent_micro_usd FROM billing_budget_spend WHERE budget_id = $1 \
         ORDER BY period_start",
    )
    .bind(b.id)
    .fetch_all(cp.db().pool())
    .await
    .expect("read the spend rows");
    assert_eq!(spend, vec![(monday - Days::new(7), 680), (monday, 680)]);
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 680);
}

#[tokio::test]
async fn spend_from_an_earlier_period_never_exhausts_the_current_one() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let today = today(cp).await;
    insert_sell_backdated(db, q.model, card_json(), today - Days::new(2)).await;
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            10_000,
        ),
    )
    .await;
    let yesterday = today - TimeDelta::hours(12);
    send(
        &q,
        rows(&q, q.key, Some(q.w.member), "y", 20, Some(yesterday)),
    )
    .await;
    let held: i64 = sqlx::query_scalar(
        "SELECT spent_micro_usd FROM billing_budget_spend \
         WHERE budget_id = $1 AND period_start = $2",
    )
    .bind(b.id)
    .bind(today - Days::new(1))
    .fetch_one(db.pool())
    .await
    .expect("yesterday's spend row");
    assert_eq!(held, 13_600);
    assert_eq!(spent(cp, q.w.tenant, b.id).await, 0);
    assert!(
        !flagged(db, q.w.tenant).await,
        "yesterday's spend flagged today's quota"
    );
    assert_eq!(live(db, b.id).await, open("api_key", q.key, "day"));
}

#[tokio::test]
async fn raising_a_limit_lifts_the_mark_and_deleting_retires_the_document() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let periodic = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            10_000,
        ),
    )
    .await;
    let cumulative = created(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, q.key, SetBy::Member, None, 10_000),
    )
    .await;
    send(&q, rows(&q, q.key, Some(q.w.member), "r", 15, None)).await;
    until_exhausted(db, periodic.id).await;
    until_exhausted(db, cumulative.id).await;

    for (b, period) in [(&periodic, "day"), (&cumulative, "month")] {
        let raised = set_limit(cp, q.w.tenant, b.id, 1_000_000).await;
        assert_eq!(raised.limit_micro_usd, 1_000_000);
        // Settled in the same transaction.
        assert_eq!(live(db, b.id).await, open("api_key", q.key, period));

        delete(cp, q.w.tenant, b.id).await;
        let (_, deleted, _) = doc(db, b.id).await.expect("a document");
        assert!(deleted, "quota {}'s document is still live", b.id);
        let mut conn = db.pool().acquire().await.expect("a connection");
        assert!(budgets::get(&mut conn, q.w.tenant, b.id)
            .await
            .expect("read")
            .is_none());
        assert!(!budgets::list(&mut conn, q.w.tenant)
            .await
            .expect("list")
            .iter()
            .any(|(row, _)| row.id == b.id));
    }
}

#[tokio::test]
async fn a_period_with_cumulative_is_refused() {
    let Some(q) = setup().await else { return };
    let e = create(
        &q.w.cp,
        q.w.tenant,
        NewBudget {
            kind: Kind::Cumulative,
            ..quota(
                BudgetScope::ApiKey,
                q.key,
                SetBy::OrgAdmin,
                Some(Day),
                10_000,
            )
        },
    )
    .await
    .expect_err("a cumulative quota with a period");
    assert_eq!((e.status.as_u16(), e.code), (422, "validation_failed"));
    assert_eq!(
        e.details
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/period"]
    );
}

#[tokio::test]
async fn one_quota_per_target_kind_period_and_setter() {
    let Some(q) = setup().await else { return };
    let cp = &q.w.cp;
    let k = q.key;
    created(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, k, SetBy::OrgAdmin, Some(Day), 10_000),
    )
    .await;
    let e = create(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, k, SetBy::OrgAdmin, Some(Day), 20_000),
    )
    .await
    .expect_err("a second day quota by the same setter");
    assert_eq!((e.status.as_u16(), e.code), (409, "budget_exists"));
    for (period, set_by) in [
        (Some(Week), SetBy::OrgAdmin),
        (None, SetBy::OrgAdmin),
        (Some(Day), SetBy::Member),
    ] {
        create(
            cp,
            q.w.tenant,
            quota(BudgetScope::ApiKey, k, set_by, period, 10_000),
        )
        .await
        .unwrap_or_else(|e| panic!("{period:?} by {set_by:?}: {e:?}"));
    }
}

#[tokio::test]
async fn a_child_may_exceed_its_parent() {
    let Some(q) = setup().await else { return };
    let cp = &q.w.cp;
    created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::Team,
            q.w.tenant,
            SetBy::Operator,
            Some(Month),
            10_000,
        ),
    )
    .await;
    let member = create(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::Member,
            q.w.member,
            SetBy::OrgAdmin,
            Some(Month),
            5_000_000,
        ),
    )
    .await;
    let key = create(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Month),
            9_000_000,
        ),
    )
    .await;
    assert!(member.is_ok(), "{member:?}");
    assert!(key.is_ok(), "{key:?}");
}

/// What the domain write answers: everything a caller is shown.
fn answer(e: &ApiError) -> (u16, &'static str, String, Vec<Detail>) {
    (
        e.status.as_u16(),
        e.code,
        e.message.clone(),
        e.details.clone(),
    )
}

#[tokio::test]
async fn a_quota_target_must_be_the_tenants_own() {
    let Some(q) = setup().await else { return };
    let cp = &q.w.cp;
    let other = seed_tenant(cp, &q.w.admin).await;
    let their_member = cp.seed_member(other, MemberRole::Member).await.id;
    let their_key = seed_key(cp, &q.w.admin, other, Some(their_member)).await;
    let their_key = Uuid::parse_str(&their_key).expect("a key id");
    let random = Uuid::new_v4();
    let refused = |scope, scope_ref, set_by| {
        let new = quota(scope, scope_ref, set_by, Some(Day), 10_000);
        async move {
            create(cp, q.w.tenant, new)
                .await
                .expect_err("a target outside the tenant")
        }
    };
    let unknown = answer(&refused(BudgetScope::ApiKey, random, SetBy::OrgAdmin).await);
    assert_eq!((unknown.0, unknown.1), (422, "validation_failed"));
    assert_eq!(
        unknown
            .3
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/scope_ref"]
    );
    for (scope, scope_ref, set_by) in [
        (BudgetScope::ApiKey, their_key, SetBy::OrgAdmin),
        (BudgetScope::ApiKey, their_key, SetBy::Member),
        (BudgetScope::Member, their_member, SetBy::OrgAdmin),
        (BudgetScope::Member, random, SetBy::OrgAdmin),
        (BudgetScope::Team, other, SetBy::Operator),
        (BudgetScope::Team, random, SetBy::Operator),
    ] {
        assert_eq!(
            answer(&refused(scope, scope_ref, set_by).await),
            unknown,
            "{scope:?} {scope_ref}"
        );
    }
    // The platform's own tenant is never marked, so it takes no quota.
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("the internal tenant");
    let e = create(
        cp,
        internal,
        quota(
            BudgetScope::Team,
            internal,
            SetBy::Operator,
            Some(Day),
            10_000,
        ),
    )
    .await
    .expect_err("a quota on the internal tenant");
    assert_eq!(answer(&e), unknown);
}

#[tokio::test]
async fn operator_rows_do_not_count_toward_the_tenant_limit() {
    let Some(q) = setup().await else { return };
    let cp = &q.w.cp;
    // An operator's quota first: it must leave the tenant all 100 of its own.
    created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::Team,
            q.w.tenant,
            SetBy::Operator,
            Some(Month),
            10_000,
        ),
    )
    .await;
    // Eight quotas per key (four periods, two setters) on twelve keys, and
    // four member quotas: the tenant's 100.
    let mut keys = vec![q.key];
    for _ in 1..12 {
        keys.push(key_of(&q, Some(q.w.member)).await);
    }
    let periods = [Some(Day), Some(Week), Some(Month), None];
    let mut news = Vec::new();
    for key in &keys {
        for set_by in [SetBy::OrgAdmin, SetBy::Member] {
            for period in periods {
                news.push(quota(BudgetScope::ApiKey, *key, set_by, period, 10_000));
            }
        }
    }
    for period in periods {
        news.push(quota(
            BudgetScope::Member,
            q.w.member,
            SetBy::OrgAdmin,
            period,
            10_000,
        ));
    }
    assert_eq!(news.len() as i64, budgets::MAX_BUDGETS_PER_TENANT);
    for new in news {
        created(cp, q.w.tenant, new).await;
    }
    let spare = key_of(&q, None).await;
    let e = create(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            spare,
            SetBy::OrgAdmin,
            Some(Day),
            10_000,
        ),
    )
    .await
    .expect_err("the 101st quota");
    assert_eq!((e.status.as_u16(), e.code), (409, "budget_limit"));
    // Past the tenant's cap, an operator still sets another.
    created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::Team,
            q.w.tenant,
            SetBy::Operator,
            Some(Week),
            10_000,
        ),
    )
    .await;
}

/// Usage intake updates each account once, its flag included, so a batch
/// that crosses only a quota still never waits on its tenant's row: an
/// operator's edit may hold that row across a long write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_that_crosses_only_a_quota_never_waits_on_a_held_tenant_row() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let b = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            1_000,
        ),
    )
    .await;
    let mut edit = db.pool().begin().await.expect("begin");
    tenants::lock(&mut edit, q.w.tenant)
        .await
        .expect("lock the tenant");
    let r = tokio::time::timeout(
        Duration::from_secs(2),
        deliver(
            &q.w.dp,
            Uuid::new_v4(),
            rows(&q, q.key, Some(q.w.member), "x", 2, None),
        ),
    )
    .await
    .expect("the crossing batch is answered while the tenant is locked");
    assert_eq!(r.status, 200, "{r:?}");
    let (dirty, marked): (bool, Option<bool>) = sqlx::query_as(
        "SELECT marks_dirty, marked_exhausted FROM billing_accounts WHERE tenant_id = $1",
    )
    .bind(q.w.tenant)
    .fetch_one(db.pool())
    .await
    .expect("read the account");
    // Flagged for the quota alone: the balance is far from its floor.
    assert_eq!((dirty, marked), (true, Some(false)));
    edit.rollback().await.expect("end the edit");
    until_exhausted(db, b.id).await;
}

/// A re-pricing correction counts against cumulative quotas only, either
/// way, and flags a cumulative quota whose mark it changes.
#[tokio::test]
async fn an_adjustment_counts_against_cumulative_quotas_only() {
    let Some(q) = setup().await else { return };
    let (cp, db) = (&q.w.cp, q.w.cp.db());
    let cumulative = created(
        cp,
        q.w.tenant,
        quota(BudgetScope::ApiKey, q.key, SetBy::Member, None, 1_000),
    )
    .await;
    let periodic = created(
        cp,
        q.w.tenant,
        quota(
            BudgetScope::ApiKey,
            q.key,
            SetBy::OrgAdmin,
            Some(Day),
            1_000,
        ),
    )
    .await;
    // Counted under the tenant's billing lock, as a re-pricing run counts,
    // and then settled.
    let adjust = |amount: i64| {
        let row = ChargedRow {
            tenant_id: q.w.tenant,
            member_id: Some(q.w.member),
            api_key_id: q.key,
            price_instant: Utc::now(),
            amount,
        };
        let tenant = q.w.tenant;
        async move {
            let flag = cp
                .db()
                .tx(move |conn| {
                    async move {
                        tenants::lock_billing(&mut *conn, tenant).await?;
                        Ok::<_, ApiError>(
                            budgets::count_charges(conn, &[row], CountMode::Adjustment).await?,
                        )
                    }
                    .boxed()
                })
                .await;
            marks::settle(cp.db(), &WriteCtx::of(cp.state()), tenant)
                .await
                .expect("settle");
            flag
        }
    };
    assert_eq!(adjust(1_360).await.expect("count"), vec![q.w.tenant]);
    assert_eq!(spent(cp, q.w.tenant, cumulative.id).await, 1_360);
    assert_eq!(spent(cp, q.w.tenant, periodic.id).await, 0);
    assert!(live(db, cumulative.id).await.get("exhausted").is_some());

    assert_eq!(adjust(-1_000).await.expect("count"), vec![q.w.tenant]);
    assert_eq!(spent(cp, q.w.tenant, cumulative.id).await, 360);
    assert_eq!(
        live(db, cumulative.id).await,
        open("api_key", q.key, "month")
    );
    assert_eq!(adjust(-10).await.expect("count"), Vec::<Uuid>::new());
}

#[test]
fn every_quota_document_loads_on_the_lenient_schema() {
    let now = Utc::now();
    for (scope, set_by) in [
        (BudgetScope::Team, SetBy::Operator),
        (BudgetScope::Member, SetBy::OrgAdmin),
        (BudgetScope::ApiKey, SetBy::Member),
    ] {
        for period in [Some(Day), Some(Week), Some(Month), None] {
            let row = BudgetRow {
                id: Uuid::new_v4(),
                tenant_id: Uuid::new_v4(),
                scope,
                scope_ref: Uuid::new_v4(),
                set_by,
                kind: if period.is_some() {
                    Kind::Periodic
                } else {
                    Kind::Cumulative
                },
                period,
                counted_from: period.is_none().then_some(now),
                limit_micro_usd: 10_000,
                marked_period_start: None,
                marked_exhausted: None,
                created_by_kind: "system".to_string(),
                created_by: None,
                created_at: now,
            };
            for (spent, exhausted) in [(0, false), (9_999, false), (10_000, true), (25_000, true)] {
                let doc = budgets::budget_doc(&row, spent, now);
                assert_eq!(doc.get("exhausted").is_some(), exhausted, "{doc}");
                validate_budget(&doc).unwrap_or_else(|e| panic!("strict: {e} {doc}"));
                validate_budget_lenient(&doc).unwrap_or_else(|e| panic!("lenient: {e} {doc}"));
                serde_json::from_value::<Budget>(doc.clone())
                    .unwrap_or_else(|e| panic!("the gateway's type: {e} {doc}"));
            }
        }
    }
}
