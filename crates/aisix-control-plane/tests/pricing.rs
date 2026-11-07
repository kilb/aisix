//! Pricing in the control plane: price revisions are written append-only and
//! never start in the past, and every usage row intake stores is priced once,
//! in the transaction that stores it, from token counts and the control
//! plane's own price book.
//!
//! Prices are written through `pricing::store`, or through the operators'
//! API (`/admin/pricing/*`) in the cases at the end; usage arrives over
//! `POST /dp/telemetry` exactly as a gateway sends it. The tenant a row is
//! billed to comes from the control plane's own key record, never from the
//! row's word alone.

mod support;

use std::sync::Arc;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::error::ApiError;
use aisix_control_plane::pricing::store::{
    self, CostRevision, DiscountRevision, NewCostPrice, NewDiscount, NewSellPrice, SellRevision,
};
use aisix_pricing::{Catalogue, Rate, RateCard, ENGINE_VERSION};
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::{DpClient, OperatorSession, Resp, TestCp};
use tokio::sync::Barrier;
use uuid::Uuid;

const DEDUP: &str = "x-aisix-usage-batch-dedup";

/// USD per million: input 1, output 2, cache read 0.1. So 1000 prompt tokens
/// of which 800 were cache hits, and 200 completion tokens, cost
/// 200 × 1 + 800 × 0.1 + 200 × 2 = 680 µ$.
fn card() -> RateCard {
    serde_json::from_value(json!({
        "default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}
    }))
    .expect("the test card decodes")
}

/// The database's clock: what a price write compares `effective_from` with.
async fn db_now(cp: &TestCp) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(cp.db().pool())
        .await
        .expect("read now()")
}

async fn pricing_version(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT (value::text)::bigint FROM cp_settings WHERE key = 'pricing_version'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pricing_version")
}

/// A version bump as a price write makes it, for revisions written by raw SQL.
async fn bump_pricing_version(cp: &TestCp) {
    sqlx::query(
        "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1), updated_at = now() \
         WHERE key = 'pricing_version'",
    )
    .execute(cp.db().pool())
    .await
    .expect("bump pricing_version");
}

fn text(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} in {v}"))
        .to_string()
}

/// `POST path` as `admin`, which must answer 201.
async fn create(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let r = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(r.status, 201, "POST {path}: {r:?}");
    r.body
}

/// `method path` as `admin` with `If-Match: "<version>"`, and no body,
/// labelled JSON as the Dashboard sends every request.
async fn send_if_match(
    cp: &TestCp,
    admin: &OperatorSession,
    method: &str,
    path: &str,
    version: i64,
) -> u16 {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    client
        .request(method, cp.url(path))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .expect("send the request")
        .status()
        .as_u16()
}

/// An upstream account (a provider key), created as an operator; its id.
async fn seed_provider_key(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let pk = create(
        cp,
        admin,
        "/admin/resources/provider_keys",
        json!({"doc": {
            "display_name": format!("acct-pricing-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "api_key": "sk-pricing-upstream-7c1",
            "groups": ["shared"],
        }}),
    )
    .await;
    Uuid::parse_str(&text(&pk, "/id")).expect("a provider key id")
}

/// A platform model on a provider key of its own, created as an operator.
async fn seed_model(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let pk = seed_provider_key(cp, admin).await;
    let model = create(
        cp,
        admin,
        "/admin/resources/models",
        json!({"doc": {
            "display_name": format!("priced-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": pk,
        }}),
    )
    .await;
    Uuid::parse_str(&text(&model, "/id")).expect("a model id")
}

async fn seed_tenant(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let tenant = create(
        cp,
        admin,
        "/admin/tenants",
        json!({"name": format!("tenant-{}", Uuid::new_v4().simple())}),
    )
    .await;
    Uuid::parse_str(&text(&tenant, "/id")).expect("a tenant id")
}

/// A key of `tenant`, bound to `member` when given, minted by an operator;
/// its id.
async fn seed_key(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    member: Option<Uuid>,
) -> String {
    let mut body = json!({"label": "pricing-7c1"});
    if let Some(member) = member {
        body["member_id"] = json!(member);
    }
    let key = create(cp, admin, &format!("/admin/tenants/{tenant}/keys"), body).await;
    text(&key, "/key/id")
}

/// A control plane with an operator, a gateway, a tenant, one of its members
/// and that member's key.
struct World {
    cp: TestCp,
    admin: OperatorSession,
    dp: DpClient,
    tenant: Uuid,
    member: Uuid,
    key: String,
}

async fn world() -> Option<World> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-pricing-7c1").await;
    let tenant = seed_tenant(&cp, &admin).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&cp, &admin, tenant, Some(member)).await;
    Some(World {
        cp,
        admin,
        dp,
        tenant,
        member,
        key,
    })
}

async fn insert_sell(
    cp: &TestCp,
    model_id: Uuid,
    tenant_id: Option<Uuid>,
    effective_from: DateTime<Utc>,
    card: Option<RateCard>,
) -> Result<SellRevision, ApiError> {
    let row = NewSellPrice {
        model_id,
        tenant_id,
        effective_from,
        card,
        source: "manual".to_string(),
    };
    cp.db()
        .tx(move |c| async move { store::insert_sell_price(c, &row, Actor::System).await }.boxed())
        .await
}

/// The platform price `card()` for `model`, starting now.
async fn price(cp: &TestCp, model: Uuid) -> SellRevision {
    let now = db_now(cp).await;
    insert_sell(cp, model, None, now, Some(card()))
        .await
        .expect("price the model")
}

async fn insert_cost(cp: &TestCp, row: NewCostPrice) -> Result<CostRevision, ApiError> {
    cp.db()
        .tx(move |c| async move { store::insert_cost_price(c, &row, Actor::System).await }.boxed())
        .await
}

async fn insert_discount(cp: &TestCp, row: NewDiscount) -> Result<DiscountRevision, ApiError> {
    cp.db()
        .tx(move |c| async move { store::insert_discount(c, &row, Actor::System).await }.boxed())
        .await
}

/// `err` is a 422 at `path`.
fn assert_refused_at(err: &ApiError, path: &str) {
    assert_eq!(
        (err.status.as_u16(), err.code),
        (422, "validation_failed"),
        "{err:?}"
    );
    assert_eq!(
        err.details
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        vec![path],
        "{err:?}"
    );
}

/// A chat usage row of the world's tenant, member and key for `model`,
/// occurring now, with `fields` on top. It claims a `cost_usd` no price would
/// give, which pricing must never read.
fn usage(w: &World, event_id: &str, model: Uuid, fields: Value) -> Value {
    let mut event = json!({
        "event_id": event_id,
        "request_id": format!("req-{event_id}"),
        "occurred_at": Utc::now().to_rfc3339(),
        "team_id": w.tenant.to_string(),
        "api_key_id": w.key,
        "user_id": w.member.to_string(),
        "requested_model_id": model.to_string(),
        "operation": "chat",
        "status_code": 200,
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "cost_usd": 12.5,
    });
    for (k, v) in fields.as_object().expect("fields are an object") {
        event[k] = v.clone();
    }
    event
}

/// 1000 prompt tokens, 800 of them cache hits, and 200 completion tokens:
/// 680 µ$ at `card()`.
fn tokens_680() -> Value {
    json!({"prompt_tokens": 1000, "cached_prompt_tokens": 800, "completion_tokens": 200})
}

async fn deliver(dp: &DpClient, batch_id: Uuid, events: Vec<Value>) -> Resp {
    dp.telemetry(batch_id, &json!({ "events": events })).await
}

fn counts(accepted: u64, rejected: u64, duplicates: u64) -> Value {
    json!({"accepted": accepted, "rejected": rejected, "duplicates": duplicates})
}

/// One `pricing_charges` row, as far as these tests read it.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Charge {
    status: String,
    charge: Option<i64>,
    error_code: Option<String>,
    tenant_id: String,
    api_key_id: String,
    user_id: String,
}

impl Charge {
    fn charged(w: &World, micro_usd: i64) -> Charge {
        Charge {
            status: "charged".to_string(),
            charge: Some(micro_usd),
            error_code: None,
            tenant_id: w.tenant.to_string(),
            api_key_id: w.key.clone(),
            user_id: w.member.to_string(),
        }
    }

    fn unpriced(w: &World, code: &str) -> Charge {
        Charge {
            status: "unpriced".to_string(),
            charge: None,
            error_code: Some(code.to_string()),
            tenant_id: w.tenant.to_string(),
            api_key_id: w.key.clone(),
            user_id: w.member.to_string(),
        }
    }
}

async fn charge_of(cp: &TestCp, event_id: &str) -> Option<Charge> {
    sqlx::query_as(
        "SELECT status, charge_micro_usd AS charge, error_code, tenant_id, api_key_id, user_id \
         FROM pricing_charges WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_optional(cp.db().pool())
    .await
    .expect("read the charge")
}

#[tokio::test]
async fn a_fresh_usage_row_is_charged_in_the_same_batch() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    // Priced from the tokens and the book: the row's own cost_usd says 12.5.
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 680)));
}

#[tokio::test]
async fn a_re_sent_batch_and_a_repeated_event_are_charged_once() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let (b, event) = (Uuid::new_v4(), usage(&w, "e1", model, tokens_680()));
    for _ in 0..2 {
        let r = deliver(&w.dp, b, vec![event.clone()]).await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    let r = deliver(&w.dp, Uuid::new_v4(), vec![event.clone()]).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(0, 0, 1)), "{r:?}");
    assert_eq!(w.cp.count("pricing_charges").await, 1);
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 680)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_deliveries_of_one_batch_charge_once() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = cp.issue_bundle(&admin, "gw-pricing-7c1").await;
    let tenant = seed_tenant(&cp, &admin).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await.id;
    let key = seed_key(&cp, &admin, tenant, Some(member)).await;
    let model = seed_model(&cp, &admin).await;
    price(&cp, model).await;
    // A second instance on the same database, booted after the credential
    // was issued so it knows it.
    let other = cp.restart().await;
    let w = World {
        dp: cp.dp_client(&bundle),
        cp,
        admin,
        tenant,
        member,
        key,
    };
    let dps = [
        Arc::new(w.cp.dp_client(&bundle)),
        Arc::new(other.dp_client(&bundle)),
    ];
    let (b, body) = (
        Uuid::new_v4(),
        json!({"events": [usage(&w, "c1", model, tokens_680())]}),
    );
    let barrier = Arc::new(Barrier::new(dps.len()));
    let sends: Vec<_> = dps
        .iter()
        .map(|dp| {
            let (dp, barrier, body) = (dp.clone(), barrier.clone(), body.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                dp.telemetry(b, &body).await
            })
        })
        .collect();
    let mut answers = Vec::new();
    for send in sends {
        answers.push(send.await.expect("a delivery task"));
    }
    assert!(
        answers
            .iter()
            .all(|r| r.status == 200 && r.header(DEDUP) == Some("1")),
        "{answers:?}"
    );
    assert_eq!(w.cp.count("usage_events").await, 1);
    assert_eq!(w.cp.count("pricing_charges").await, 1);
    assert_eq!(charge_of(&w.cp, "c1").await, Some(Charge::charged(&w, 680)));
}

#[tokio::test]
async fn two_rows_with_one_event_id_in_one_batch_are_priced_from_the_stored_one() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let first = usage(&w, "e1", model, json!({"prompt_tokens": 100}));
    let second = usage(&w, "e1", model, json!({"prompt_tokens": 999}));
    let r = deliver(&w.dp, Uuid::new_v4(), vec![first, second]).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 1)), "{r:?}");
    let stored: Value = sqlx::query_scalar("SELECT event FROM usage_events WHERE event_id = 'e1'")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read the stored row");
    assert_eq!(stored["prompt_tokens"], 100);
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 100)));
}

#[tokio::test]
async fn a_price_cannot_start_in_the_past() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let (version, audits) = (pricing_version(&w.cp).await, w.cp.count("audit_log").await);
    let now = db_now(&w.cp).await;
    let err = insert_sell(
        &w.cp,
        model,
        None,
        now - TimeDelta::minutes(1),
        Some(card()),
    )
    .await
    .expect_err("a price that started a minute ago");
    assert_refused_at(&err, "/effective_from");
    assert_eq!(w.cp.count("pricing_sell_revisions").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);
    assert_eq!(pricing_version(&w.cp).await, version);

    // Inside the 5 s tolerance.
    let now = db_now(&w.cp).await;
    let rev = insert_sell(
        &w.cp,
        model,
        None,
        now - TimeDelta::seconds(2),
        Some(card()),
    )
    .await
    .expect("a price that started 2 s ago");
    assert_eq!(rev.engine_version, i64::from(ENGINE_VERSION));
    assert_eq!(pricing_version(&w.cp).await, version + 1);
}

#[tokio::test]
async fn a_bad_card_unprices_only_its_model() {
    let Some(w) = world().await else { return };
    let (a, b) = (
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    );
    // No output rate: a card no engine can bill with.
    sqlx::query(
        "INSERT INTO pricing_sell_revisions (engine_version, model_id, effective_from, card, source) \
         VALUES ($1, $2, now(), '{\"default\":{\"base\":{\"input\":\"1\"}}}', 'manual')",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(a)
    .execute(w.cp.db().pool())
    .await
    .expect("write a bad revision");
    bump_pricing_version(&w.cp).await;
    price(&w.cp, b).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "ea", a, tokens_680()),
            usage(&w, "eb", b, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "ea").await,
        Some(Charge::unpriced(&w, "invalid_price"))
    );
    assert_eq!(charge_of(&w.cp, "eb").await, Some(Charge::charged(&w, 680)));
}

/// Writes a sell revision of `card()` for `model` stamped `engine_version`,
/// by raw SQL as a newer control plane (or a hand edit) would.
async fn raw_revision(cp: &TestCp, model: Uuid, engine_version: i64) {
    sqlx::query(
        "INSERT INTO pricing_sell_revisions (engine_version, model_id, effective_from, card, source) \
         VALUES ($1, $2, now(), $3, 'manual')",
    )
    .bind(engine_version)
    .bind(model)
    .bind(serde_json::to_value(card()).expect("the card serializes"))
    .execute(cp.db().pool())
    .await
    .expect("write a raw revision");
    bump_pricing_version(cp).await;
}

#[tokio::test]
async fn a_revision_from_a_newer_engine_unprices_only_its_model() {
    let Some(w) = world().await else { return };
    let (a, b) = (
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    );
    raw_revision(&w.cp, a, i64::from(ENGINE_VERSION) + 1).await;
    price(&w.cp, b).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "ea", a, tokens_680()),
            usage(&w, "eb", b, tokens_680()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "ea").await,
        Some(Charge::unpriced(&w, "invalid_price"))
    );
    assert_eq!(charge_of(&w.cp, "eb").await, Some(Charge::charged(&w, 680)));
}

/// An `engine_version` no `u32` holds (only raw SQL can write one) reads as
/// the newest engine there could be: the revision is isolated, and nothing
/// else is dropped.
#[tokio::test]
async fn an_engine_version_beyond_u32_isolates_only_its_revision() {
    let Some(w) = world().await else { return };
    let (above, negative, b) = (
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    );
    raw_revision(&w.cp, above, i64::from(u32::MAX) + 1).await;
    raw_revision(&w.cp, negative, -1).await;
    price(&w.cp, b).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "e-above", above, tokens_680()),
            usage(&w, "e-negative", negative, tokens_680()),
            usage(&w, "eb", b, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(3, 0, 0)), "{r:?}");
    for id in ["e-above", "e-negative"] {
        assert_eq!(
            charge_of(&w.cp, id).await,
            Some(Charge::unpriced(&w, "invalid_price")),
            "{id}"
        );
    }
    assert_eq!(charge_of(&w.cp, "eb").await, Some(Charge::charged(&w, 680)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_database_error_while_loading_prices_is_503_and_stores_nothing() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    bump_pricing_version(&w.cp).await;
    let mut holder = PgConnection::connect(w.cp.database_url())
        .await
        .expect("connect");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("LOCK TABLE pricing_sell_revisions IN ACCESS EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("lock the sell revisions");
    let (b, events) = (Uuid::new_v4(), vec![usage(&w, "e1", model, tokens_680())]);
    let r = deliver(&w.dp, b, events.clone()).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (503, json!("unavailable")),
        "{r:?}"
    );
    assert_eq!(r.header(DEDUP), Some("1"));
    for table in ["usage_batches", "usage_events", "pricing_charges"] {
        assert_eq!(w.cp.count(table).await, 0, "{table}");
    }

    sqlx::query("COMMIT")
        .execute(&mut holder)
        .await
        .expect("release the lock");
    let r = deliver(&w.dp, b, events).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(w.cp.count("usage_events").await, 1);
    assert_eq!(w.cp.count("pricing_charges").await, 1);
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 680)));
}

#[tokio::test]
async fn the_price_book_is_reloaded_only_when_a_price_changes() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let prices = w.cp.state().prices.clone();
    for i in 0..3 {
        let id = format!("r{i}");
        let r = deliver(
            &w.dp,
            Uuid::new_v4(),
            vec![usage(&w, &id, model, tokens_680())],
        )
        .await;
        assert_eq!(r.status, 200, "{r:?}");
    }
    assert_eq!(prices.loads(), 1);
    // A batch of nothing but duplicates prices nothing and reads no book,
    // not even once the version has moved.
    bump_pricing_version(&w.cp).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "r0", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(0, 0, 1)), "{r:?}");
    assert_eq!(prices.loads(), 1);

    price(&w.cp, seed_model(&w.cp, &w.admin).await).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "r3", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(prices.loads(), 2);
    assert_eq!(w.cp.count("pricing_charges").await, 4);
}

#[tokio::test]
async fn a_row_without_a_price_is_stored_unpriced_with_its_reason() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(w.cp.count("usage_events").await, 1);
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(Charge::unpriced(&w, "missing_price"))
    );
}

#[tokio::test]
async fn a_row_the_engine_cannot_read_is_stored_unpriced_and_the_rest_are_charged() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "e-many", model, json!({"prompt_tokens": "many"})),
            usage(&w, "e-ok", model, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e-many").await,
        Some(Charge::unpriced(&w, "undecodable_usage"))
    );
    assert_eq!(
        charge_of(&w.cp, "e-ok").await,
        Some(Charge::charged(&w, 680))
    );
}

#[tokio::test]
async fn a_cache_hit_row_is_stored_exempt() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let hit = usage(
        &w,
        "e1",
        model,
        json!({"cache_status": "hit", "prompt_tokens": 500}),
    );
    let r = deliver(&w.dp, Uuid::new_v4(), vec![hit]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(Charge {
            status: "exempt".to_string(),
            ..Charge::charged(&w, 0)
        })
    );
}

/// A quantity on a dimension the card does not rate refuses the row: the
/// upstream charged for it, so it is never billed at nothing.
#[tokio::test]
async fn a_quantity_the_card_does_not_rate_is_unpriced_not_free() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let searched = usage(
        &w,
        "e1",
        model,
        json!({"prompt_tokens": 100, "completion_tokens": 10, "web_search_requests": 2}),
    );
    let r = deliver(&w.dp, Uuid::new_v4(), vec![searched]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(Charge::unpriced(&w, "missing_dimension_price"))
    );
}

#[tokio::test]
async fn an_invalid_card_never_reaches_the_table() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let (version, audits) = (pricing_version(&w.cp).await, w.cp.count("audit_log").await);
    let no_default: RateCard = serde_json::from_value(json!({
        "priority": {"base": {"input": "1", "output": "2"}}
    }))
    .expect("decodes; validate() is what refuses it");
    let now = db_now(&w.cp).await;
    let err = insert_sell(&w.cp, model, None, now, Some(no_default))
        .await
        .expect_err("a card without a default tier");
    assert_refused_at(&err, "/card");
    // Only a tenant override can end; a platform price always has a card.
    let err = insert_sell(&w.cp, model, None, now, None)
        .await
        .expect_err("a platform price without a card");
    assert_refused_at(&err, "/card");
    assert_eq!(w.cp.count("pricing_sell_revisions").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);
    assert_eq!(pricing_version(&w.cp).await, version);
}

#[tokio::test]
async fn a_price_for_a_deleted_or_unknown_model_is_refused() {
    let Some(w) = world().await else { return };
    let now = db_now(&w.cp).await;
    let err = insert_sell(&w.cp, Uuid::new_v4(), None, now, Some(card()))
        .await
        .expect_err("an unknown model");
    assert_refused_at(&err, "/model_id");

    let model = seed_model(&w.cp, &w.admin).await;
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM gw_resources WHERE kind = 'models' AND id = $1")
            .bind(model)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the model's version");
    let status = send_if_match(
        &w.cp,
        &w.admin,
        "DELETE",
        &format!("/admin/resources/models/{model}"),
        version,
    )
    .await;
    assert_eq!(status, 200, "delete the model");
    let now = db_now(&w.cp).await;
    let err = insert_sell(&w.cp, model, None, now, Some(card()))
        .await
        .expect_err("a deleted model");
    assert_refused_at(&err, "/model_id");

    let live = seed_model(&w.cp, &w.admin).await;
    let err = insert_sell(&w.cp, live, Some(Uuid::new_v4()), now, Some(card()))
        .await
        .expect_err("an unknown tenant");
    assert_refused_at(&err, "/tenant_id");
    assert_eq!(w.cp.count("pricing_sell_revisions").await, 0);
}

/// `(action, target_kind, target_id, tenant_id, detail)` of every audit row
/// about pricing, oldest first.
async fn pricing_audit(cp: &TestCp) -> Vec<(String, String, Option<String>, Option<Uuid>, Value)> {
    sqlx::query_as(
        "SELECT action, target_kind, target_id, tenant_id, detail FROM audit_log \
         WHERE action LIKE 'pricing.%' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log")
}

#[tokio::test]
async fn every_price_write_is_audited_in_its_transaction() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let version = pricing_version(&w.cp).await;
    let rev = price(&w.cp, model).await;
    let audit = pricing_audit(&w.cp).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    let (action, kind, target, tenant, detail) = &audit[0];
    assert_eq!(
        (action.as_str(), kind.as_str(), target.clone(), *tenant),
        (
            "pricing.sell.create",
            "pricing_sell_revision",
            Some(rev.revision_id.to_string()),
            None
        )
    );
    assert_eq!(detail["model_id"], json!(model));
    assert_eq!(detail["source"], json!("manual"));
    assert_eq!(pricing_version(&w.cp).await, version + 1);

    // A tenant override, then its end.
    let now = db_now(&w.cp).await;
    insert_sell(&w.cp, model, Some(w.tenant), now, Some(card()))
        .await
        .expect("an override");
    let ended = insert_sell(&w.cp, model, Some(w.tenant), db_now(&w.cp).await, None)
        .await
        .expect("end the override");
    assert_eq!(ended.card, None);
    let audit = pricing_audit(&w.cp).await;
    let actions: Vec<(&str, Option<Uuid>)> = audit
        .iter()
        .map(|(action, _, _, tenant, _)| (action.as_str(), *tenant))
        .collect();
    assert_eq!(
        actions,
        vec![
            ("pricing.sell.create", None),
            ("pricing.sell.create", Some(w.tenant)),
            ("pricing.sell.end_override", Some(w.tenant)),
        ]
    );
    assert_eq!(audit[2].2, Some(ended.revision_id.to_string()));
    assert_eq!(pricing_version(&w.cp).await, version + 3);
}

#[tokio::test]
async fn cost_prices_and_discounts_follow_the_same_rules() {
    let Some(w) = world().await else { return };
    let version = pricing_version(&w.cp).await;
    let now = db_now(&w.cp).await;
    let cost = |effective_from: DateTime<Utc>, card: RateCard| NewCostPrice {
        upstream_provider: "openai".to_string(),
        upstream_model: "gpt-4o-mini".to_string(),
        provider_key_id: None,
        effective_from,
        card,
        source: "manual".to_string(),
    };
    let no_default: RateCard = serde_json::from_value(json!({
        "flex": {"base": {"input": "1", "output": "2"}}
    }))
    .expect("decodes");
    assert_refused_at(
        &insert_cost(&w.cp, cost(now, no_default))
            .await
            .expect_err("bad card"),
        "/card",
    );
    assert_refused_at(
        &insert_cost(&w.cp, cost(now - TimeDelta::minutes(1), card()))
            .await
            .expect_err("in the past"),
        "/effective_from",
    );
    let rev = insert_cost(&w.cp, cost(now, card()))
        .await
        .expect("a cost price");
    assert_eq!(rev.engine_version, i64::from(ENGINE_VERSION));

    let discount =
        |tenant_id: Uuid, effective_from: DateTime<Utc>, discount_bps: u16| NewDiscount {
            tenant_id,
            effective_from,
            discount_bps,
        };
    let now = db_now(&w.cp).await;
    assert_refused_at(
        &insert_discount(&w.cp, discount(w.tenant, now, 10_001))
            .await
            .expect_err("over 100%"),
        "/discount_bps",
    );
    assert_refused_at(
        &insert_discount(&w.cp, discount(Uuid::new_v4(), now, 500))
            .await
            .expect_err("an unknown tenant"),
        "/tenant_id",
    );
    assert_refused_at(
        &insert_discount(&w.cp, discount(w.tenant, now - TimeDelta::minutes(1), 500))
            .await
            .expect_err("in the past"),
        "/effective_from",
    );
    let rev = insert_discount(&w.cp, discount(w.tenant, now, 10_000))
        .await
        .expect("a full discount");
    assert_eq!(rev.discount_bps, 10_000);

    let audit = pricing_audit(&w.cp).await;
    let actions: Vec<(&str, &str, Option<Uuid>)> = audit
        .iter()
        .map(|(action, kind, _, tenant, _)| (action.as_str(), kind.as_str(), *tenant))
        .collect();
    // Spelled out of parts: the workspace's price-field census
    // (`no_one_reads_a_models_price_off_the_field`) reads any `.` + `cost`
    // in a test as a field read, string literals included.
    let cost_create = format!("pricing.{}.create", "cost");
    assert_eq!(
        actions,
        vec![
            (cost_create.as_str(), "pricing_cost_revision", None),
            (
                "pricing.discount.create",
                "pricing_discount_revision",
                Some(w.tenant)
            ),
        ]
    );
    assert_eq!(pricing_version(&w.cp).await, version + 2);
    assert_eq!(w.cp.count("pricing_cost_revisions").await, 1);
    assert_eq!(w.cp.count("pricing_discount_revisions").await, 1);
}

/// Any gateway credential can name any tenant in a row, so the tenant a row
/// bills is the one the control plane's own record gives its key.
#[tokio::test]
async fn usage_is_priced_only_for_the_tenant_its_key_belongs_to() {
    let Some(w) = world().await else { return };
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let foreign_team = usage(
        &w,
        "e-foreign-team",
        model,
        json!({"team_id": other_tenant.to_string(), "prompt_tokens": 100}),
    );
    let unknown_key = usage(
        &w,
        "e-unknown-key",
        model,
        json!({"api_key_id": Uuid::new_v4().to_string(), "prompt_tokens": 100}),
    );
    let not_a_key = usage(
        &w,
        "e-not-a-key",
        model,
        json!({"api_key_id": "k1", "prompt_tokens": 100}),
    );
    // The key's id in another spelling is not the id the gateway was given.
    let upper_key = usage(
        &w,
        "e-upper-key",
        model,
        json!({"api_key_id": w.key.to_uppercase(), "prompt_tokens": 100}),
    );
    let missing_team = usage(
        &w,
        "e-no-team",
        model,
        json!({"team_id": "", "prompt_tokens": 100}),
    );
    let own = usage(&w, "e-own", model, tokens_680());
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            foreign_team,
            unknown_key,
            not_a_key,
            upper_key,
            missing_team,
            own,
        ],
    )
    .await;
    // Intake keeps every row: pricing never refuses or drops one.
    assert_eq!((r.status, r.body.clone()), (200, counts(6, 0, 0)), "{r:?}");
    // Filed under the key's tenant and member, never the tenant a row
    // claims: a forged row does not land in another tenant's records.
    for id in ["e-foreign-team", "e-no-team"] {
        assert_eq!(
            charge_of(&w.cp, id).await,
            Some(Charge::unpriced(&w, "tenant_mismatch")),
            "{id}"
        );
    }
    let claimed: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM usage_events WHERE event_id = 'e-foreign-team'")
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the stored row");
    assert_eq!(
        claimed,
        Some(other_tenant.to_string()),
        "the claim stays in the usage row"
    );
    // A key that resolves to nothing files the row under no tenant and no
    // member, and keeps the key it named.
    for id in ["e-unknown-key", "e-not-a-key", "e-upper-key"] {
        let found = charge_of(&w.cp, id).await.expect("a charge row");
        assert_eq!(
            (
                found.status.as_str(),
                found.charge,
                found.error_code.as_deref(),
                found.tenant_id.as_str(),
                found.user_id.as_str(),
            ),
            ("unpriced", None, Some("unknown_api_key"), "", ""),
            "{id}"
        );
    }
    for (id, key) in [
        ("e-not-a-key", "k1".to_string()),
        ("e-upper-key", w.key.to_uppercase()),
    ] {
        let found = charge_of(&w.cp, id).await.expect("a charge row");
        assert_eq!(found.api_key_id, key, "{id}");
    }
    assert_eq!(
        charge_of(&w.cp, "e-own").await,
        Some(Charge::charged(&w, 680))
    );
}

/// Usage that happened before a key was revoked is still billed when it
/// arrives after.
#[tokio::test]
async fn a_revoked_keys_usage_is_still_charged() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let event = usage(&w, "e1", model, tokens_680());
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1::uuid")
        .bind(&w.key)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read the key's version");
    let status = send_if_match(
        &w.cp,
        &w.admin,
        "DELETE",
        &format!("/admin/keys/{}", w.key),
        version,
    )
    .await;
    assert_eq!(status, 204, "revoke the key");
    let r = deliver(&w.dp, Uuid::new_v4(), vec![event]).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 680)));
}

/// A row that names neither a key nor a tenant (a request refused before
/// its key was resolved) bills nobody: nothing when it carries no usage, and
/// it is unpriced when it does.
#[tokio::test]
async fn a_row_naming_no_key_and_no_tenant_bills_nobody() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let anonymous = |id: &str, fields: Value| {
        let mut event = usage(&w, id, model, fields);
        let fields = event.as_object_mut().expect("an object");
        for name in ["api_key_id", "team_id", "user_id"] {
            fields.remove(name);
        }
        event
    };
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            anonymous("e-refused", json!({"status_code": 401})),
            anonymous("e-tokens", tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    let nobody = |status: &str, charge: Option<i64>, code: Option<&str>| Charge {
        status: status.to_string(),
        charge,
        error_code: code.map(str::to_string),
        tenant_id: String::new(),
        api_key_id: String::new(),
        user_id: String::new(),
    };
    assert_eq!(
        charge_of(&w.cp, "e-refused").await,
        Some(nobody("nothing_to_bill", Some(0), None))
    );
    assert_eq!(
        charge_of(&w.cp, "e-tokens").await,
        Some(nobody("unpriced", None, Some("unknown_tenant")))
    );
}

/// Nothing a row carries may fail its batch: a key or tenant id too long for
/// an index entry (random text, which compression cannot shrink) is not
/// copied into the charge, and the row is stored unpriced like any other
/// unknown key.
#[tokio::test]
async fn an_identity_too_long_to_index_does_not_fail_its_batch() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let long = || {
        (0..200)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect::<String>()
    };
    let event = usage(
        &w,
        "e-long",
        model,
        json!({"api_key_id": long(), "team_id": long(), "user_id": long(), "prompt_tokens": 100}),
    );
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![event, usage(&w, "e-ok", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e-long").await,
        Some(Charge {
            tenant_id: String::new(),
            api_key_id: String::new(),
            user_id: String::new(),
            ..Charge::unpriced(&w, "unknown_api_key")
        })
    );
    assert_eq!(
        charge_of(&w.cp, "e-ok").await,
        Some(Charge::charged(&w, 680))
    );
}

/// An event id another gateway already delivered is a duplicate however its
/// copy differs: the stored row and its one charge stand.
#[tokio::test]
async fn a_duplicate_from_another_gateway_is_not_charged_again() {
    let Some(w) = world().await else { return };
    let issuer = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let bundle = w.cp.issue_bundle(&issuer, "gw-pricing-other-7c1").await;
    let other = w.cp.dp_client(&bundle);
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, json!({"prompt_tokens": 100}))],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let losing = Uuid::new_v4();
    let r = deliver(
        &other,
        losing,
        vec![usage(&w, "e1", model, json!({"prompt_tokens": 999}))],
    )
    .await;
    // Not counted as refused: re-sending could not store it either.
    assert_eq!((r.status, r.body.clone()), (200, counts(0, 0, 1)), "{r:?}");
    let batch_rejected: i64 =
        sqlx::query_scalar("SELECT rejected FROM usage_batches WHERE batch_id = $1")
            .bind(losing)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the batch row");
    assert_eq!(batch_rejected, 0);
    assert_eq!(w.cp.count("pricing_charges").await, 1);
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 100)));
    // The losing copy is kept for operators, under the gateway that sent it.
    let kept: Vec<(String, Uuid, Value)> = sqlx::query_as(
        "SELECT dp_id::text, batch_id, raw FROM usage_rejects WHERE reason = 'foreign_duplicate'",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("read the rejects");
    assert_eq!(kept.len(), 1, "{kept:?}");
    let (dp_id, batch_id, raw) = &kept[0];
    assert_eq!(
        (
            dp_id.as_str(),
            *batch_id,
            raw["event_id"].clone(),
            raw["prompt_tokens"].clone()
        ),
        (bundle.dp_id.as_str(), losing, json!("e1"), json!(999))
    );

    // A copy that bills the same (a row emitted again, at its own time) is an
    // ordinary duplicate.
    let r = deliver(
        &other,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, json!({"prompt_tokens": 100}))],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(0, 0, 1)), "{r:?}");
    assert_eq!(w.cp.count("usage_rejects").await, 1);
}

/// A key's member never changes, so a charge is filed under the key's member
/// whatever member the row names, and under none for a key bound to none.
#[tokio::test]
async fn a_charge_records_its_keys_member_whatever_the_row_names() {
    let Some(w) = world().await else { return };
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let stranger = w.cp.seed_member(other_tenant, MemberRole::Member).await.id;
    let unbound_key = seed_key(&w.cp, &w.admin, w.tenant, None).await;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(
                &w,
                "e-stranger",
                model,
                json!({"user_id": stranger.to_string(), "prompt_tokens": 100}),
            ),
            usage(
                &w,
                "e-unbound",
                model,
                json!({"api_key_id": unbound_key, "user_id": "u-forged-7c1", "prompt_tokens": 100}),
            ),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e-stranger").await,
        Some(Charge::charged(&w, 100))
    );
    assert_eq!(
        charge_of(&w.cp, "e-unbound").await,
        Some(Charge {
            api_key_id: unbound_key.clone(),
            user_id: String::new(),
            ..Charge::charged(&w, 100)
        })
    );
}

/// Delivers `e<i>` (680 µ$ of usage for `model`), which must be stored and
/// charged.
async fn deliver_charged(w: &World, model: Uuid, i: usize) {
    let id = format!("e{i}");
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(w, &id, model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, &id).await,
        Some(Charge::charged(w, 680)),
        "{id}"
    );
}

/// A version row that is gone or hand-edited into something that is not a
/// whole number never stops intake: every batch then reloads the book and
/// is priced from what the tables hold.
#[tokio::test]
async fn a_missing_or_malformed_pricing_version_never_stops_intake() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let prices = w.cp.state().prices.clone();
    let pool = w.cp.db().pool();
    deliver_charged(&w, model, 0).await;
    assert_eq!(prices.loads(), 1);

    sqlx::query("DELETE FROM cp_settings WHERE key = 'pricing_version'")
        .execute(pool)
        .await
        .expect("delete the version row");
    deliver_charged(&w, model, 1).await;
    deliver_charged(&w, model, 2).await;
    assert_eq!(prices.loads(), 3, "reloaded for every batch");

    sqlx::query("INSERT INTO cp_settings (key, value) VALUES ('pricing_version', '\"seven\"')")
        .execute(pool)
        .await
        .expect("write a text version");
    let mut i = 3;
    for value in ["\"seven\"", "1.5", "100000000000000000000000", "null"] {
        sqlx::query("UPDATE cp_settings SET value = $1::jsonb WHERE key = 'pricing_version'")
            .bind(value)
            .execute(pool)
            .await
            .expect("write a malformed version");
        let before = prices.loads();
        deliver_charged(&w, model, i).await;
        assert_eq!(prices.loads(), before + 1, "{value}");
        i += 1;
    }

    // Restored, the version is cached on again.
    sqlx::query("UPDATE cp_settings SET value = '5' WHERE key = 'pricing_version'")
        .execute(pool)
        .await
        .expect("restore the version");
    let before = prices.loads();
    deliver_charged(&w, model, i).await;
    deliver_charged(&w, model, i + 1).await;
    assert_eq!(prices.loads(), before + 1);
}

// ---- the operators' pricing API: /admin/pricing/* ----

/// `method path` as `who`, with `body`.
async fn as_op(
    cp: &TestCp,
    who: &OperatorSession,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Resp {
    cp.send(method, path, Some(who.cookie()), body).await
}

/// `r` is a 422 `validation_failed` at exactly `path`.
fn assert_422_at(r: &Resp, path: &str) {
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (422, json!("validation_failed")),
        "{r:?}"
    );
    let paths: Vec<&str> = r.body["details"]
        .as_array()
        .map(|d| d.iter().filter_map(|x| x["path"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(paths, vec![path], "{r:?}");
}

/// The one charge `GET /admin/pricing/charges?event_id=` lists for `event_id`.
async fn charge_via_api(w: &World, event_id: &str) -> Value {
    let r = as_op(
        &w.cp,
        &w.admin,
        "GET",
        &format!("/admin/pricing/charges?event_id={event_id}"),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{r:?}");
    assert_eq!(r.body["next_before"], Value::Null, "{r:?}");
    items[0].clone()
}

#[tokio::test]
async fn an_operator_sets_a_price_and_a_viewer_reads_it() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let model = seed_model(&w.cp, &w.admin).await;
    let other = seed_model(&w.cp, &w.admin).await;
    let audits = w.cp.count("audit_log").await;
    let body = json!({
        "model_id": model,
        "effective_from": db_now(&w.cp).await,
        "card": {"default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}},
    });
    let r = as_op(
        &w.cp,
        &viewer,
        "POST",
        "/admin/pricing/sell",
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (403, json!("forbidden")),
        "{r:?}"
    );
    assert_eq!(w.cp.count("pricing_sell_revisions").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);

    let r = as_op(&w.cp, &w.admin, "POST", "/admin/pricing/sell", Some(body)).await;
    assert_eq!(r.status, 201, "{r:?}");
    let rev = r.body.clone();
    assert_eq!(rev["model_id"], json!(model));
    assert_eq!(rev["tenant_id"], Value::Null);
    assert_eq!(rev["source"], "manual");
    assert_eq!(rev["engine_version"], ENGINE_VERSION);
    assert_eq!(rev["created_by"], json!(w.admin.id));
    assert_eq!(rev["card"]["default"]["base"]["input"], "1");
    assert_eq!(rev["warnings"], json!([]));
    // A price for another model, which the filter must leave out.
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/sell",
        Some(json!({"model_id": other, "effective_from": db_now(&w.cp).await, "card": card()})),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    // Written by the operator, in the audit log.
    let audit: (String, Option<Uuid>, String) = sqlx::query_as(
        "SELECT actor_kind, actor_id, action FROM audit_log WHERE target_id = $1 \
         AND target_kind = 'pricing_sell_revision'",
    )
    .bind(
        rev["revision_id"]
            .as_i64()
            .expect("a revision id")
            .to_string(),
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the audit row");
    assert_eq!(
        audit,
        (
            "operator".to_string(),
            Some(w.admin.id),
            "pricing.sell.create".to_string()
        )
    );

    let r = as_op(
        &w.cp,
        &viewer,
        "GET",
        &format!("/admin/pricing/sell?model_id={model}"),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mut listed = rev.clone();
    let listed = listed.as_object_mut().expect("an object");
    listed.remove("warnings");
    listed.remove("later_revisions");
    assert_eq!(r.body["items"], json!([listed]), "{r:?}");
    assert_eq!(r.body["next_before"], Value::Null);
}

/// A price write that does not say it is JSON is `415` and writes nothing,
/// as on every mutating browser route: a form another site posts with an
/// operator's session cannot set a price.
#[tokio::test]
async fn a_price_write_that_is_not_json_is_415_and_writes_nothing() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let now = db_now(&w.cp).await;
    let version = pricing_version(&w.cp).await;
    let audits = w.cp.count("audit_log").await;
    let writes = [
        (
            "/admin/pricing/sell",
            json!({"model_id": model, "effective_from": now, "card": card()}),
        ),
        (
            "/admin/pricing/cost",
            json!({"upstream_provider": "openai", "upstream_model": "gpt-4o-mini",
                   "effective_from": now, "card": card()}),
        ),
        (
            "/admin/pricing/discounts",
            json!({"tenant_id": w.tenant, "effective_from": now, "discount_bps": 2000}),
        ),
    ];
    for (path, body) in writes {
        let bytes = serde_json::to_vec(&body).expect("serialize the body");
        for content_type in [None, Some("text/plain")] {
            let r =
                w.cp.send_raw(
                    "POST",
                    path,
                    Some(w.admin.cookie()),
                    content_type,
                    Some(bytes.clone()),
                )
                .await;
            assert_eq!(
                (r.status, r.body["code"].clone()),
                (415, json!("unsupported_media_type")),
                "{path} {content_type:?}: {r:?}"
            );
        }
    }
    for table in [
        "pricing_sell_revisions",
        "pricing_cost_revisions",
        "pricing_discount_revisions",
    ] {
        assert_eq!(w.cp.count(table).await, 0, "{table}");
    }
    assert_eq!(pricing_version(&w.cp).await, version);
    assert_eq!(w.cp.count("audit_log").await, audits);
}

#[tokio::test]
async fn a_catalogue_price_is_imported_with_its_markup() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let model = seed_model(&w.cp, &w.admin).await;
    let r = as_op(&w.cp, &viewer, "GET", "/admin/pricing/catalogue", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let version = r.body["version"].as_str().expect("a version").to_string();
    let items = r.body["items"].as_array().expect("items").clone();
    assert!(items.len() > 1, "{r:?}");
    let first = &items[0];
    let key = first["key"].as_str().expect("a key");
    assert!(first["source_url"].as_str().is_some(), "{first}");

    let list_input = Rate::parse(
        first["card"]["default"]["base"]["input"]
            .as_str()
            .expect("a list input rate"),
    )
    .expect("a rate");
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/sell",
        Some(json!({
            "model_id": model,
            "effective_from": db_now(&w.cp).await,
            "catalogue_key": key,
            "markup_bps": 2000,
        })),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    // List × 1.2, rounded half up.
    let expected = Rate((list_input.0 * 12 + 5) / 10);
    let input = Rate::parse(
        r.body["card"]["default"]["base"]["input"]
            .as_str()
            .expect("an input rate"),
    )
    .expect("a rate");
    assert_eq!(input, expected, "{r:?}");
    let source = r.body["source"].as_str().expect("a source");
    assert!(source.starts_with("catalogue:"), "{source}");
    let dated = first["effective_from"]
        .as_str()
        .map_or("-".to_string(), str::to_string);
    assert_eq!(source, format!("catalogue:{version}:{key}@{dated}"));
    assert_eq!(r.body["warnings"], json!([]), "{r:?}");

    // A list price that holds only off peak says so.
    let off_peak = items
        .iter()
        .find(|e| {
            e["key"]
                .as_str()
                .is_some_and(|k| k.starts_with("deepseek/"))
        })
        .expect("a DeepSeek entry in the catalogue");
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/sell",
        Some(json!({
            "model_id": model,
            "tenant_id": w.tenant,
            "effective_from": db_now(&w.cp).await,
            "catalogue_key": off_peak["key"],
        })),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let warnings = r.body["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{r:?}");
    assert_eq!(warnings[0]["code"], "time_of_day_price", "{r:?}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{r:?}"
    );
    // Without a markup the list price is stored as it is.
    assert_eq!(r.body["card"], off_peak["card"], "{r:?}");
    assert_eq!(r.body["tenant_id"], json!(w.tenant));

    // An alias names its entry, which the source records by its key.
    let alias = off_peak["aliases"][0].as_str().expect("an alias");
    let provider = off_peak["key"]
        .as_str()
        .and_then(|k| k.split_once('/'))
        .map(|(p, _)| p)
        .expect("a provider");
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/sell",
        Some(json!({
            "model_id": model,
            "effective_from": db_now(&w.cp).await,
            "catalogue_key": format!("{provider}/{alias}"),
        })),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        r.body["source"],
        json!(format!(
            "catalogue:{version}:{}@-",
            off_peak["key"].as_str().unwrap_or_default()
        ))
    );
}

#[tokio::test]
async fn a_bad_price_is_422_with_the_field_path() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let now = db_now(&w.cp).await;
    let audits = w.cp.count("audit_log").await;
    let version = pricing_version(&w.cp).await;
    let sell = |body: Value| as_op(&w.cp, &w.admin, "POST", "/admin/pricing/sell", Some(body));

    let no_default = json!({"priority": {"base": {"input": "1", "output": "2"}}});
    assert_422_at(
        &sell(json!({"model_id": model, "effective_from": now, "card": no_default})).await,
        "/card",
    );
    assert_422_at(
        &sell(json!({"model_id": model, "effective_from": now, "card": {"default": {"base": {"input": "one", "output": "2"}}}})).await,
        "/card",
    );
    assert_422_at(
        &sell(json!({"model_id": model, "effective_from": now, "card": card(), "catalogue_key": "openai/gpt-5.4"})).await,
        "/card",
    );
    assert_422_at(
        &sell(json!({"model_id": model, "effective_from": now})).await,
        "/card",
    );
    assert_422_at(
        &sell(json!({"model_id": model, "effective_from": now, "card": null})).await,
        "/card",
    );
    assert_422_at(
        &sell(json!({"model_id": model, "effective_from": now, "catalogue_key": "openai/no-such-model-7c1"})).await,
        "/catalogue_key",
    );
    assert_422_at(
        &sell(
            json!({"model_id": model, "effective_from": now, "catalogue_key": "no-provider-7c1"}),
        )
        .await,
        "/catalogue_key",
    );
    // A markup applies only to a catalogue import.
    assert_422_at(
        &sell(
            json!({"model_id": model, "effective_from": now, "card": card(), "markup_bps": 2000}),
        )
        .await,
        "/markup_bps",
    );
    assert_422_at(
        &sell(json!({"model_id": model, "tenant_id": Uuid::new_v4(), "effective_from": now, "card": card()})).await,
        "/tenant_id",
    );
    assert_422_at(
        &sell(json!({"model_id": Uuid::new_v4(), "effective_from": now, "card": card()})).await,
        "/model_id",
    );
    assert_422_at(
        &sell(
            json!({"model_id": model, "effective_from": now - TimeDelta::hours(1), "card": card()}),
        )
        .await,
        "/effective_from",
    );

    let discount = |body: Value| {
        as_op(
            &w.cp,
            &w.admin,
            "POST",
            "/admin/pricing/discounts",
            Some(body),
        )
    };
    for bps in [10_001, -1, 70_000] {
        assert_422_at(
            &discount(json!({"tenant_id": w.tenant, "effective_from": now, "discount_bps": bps}))
                .await,
            "/discount_bps",
        );
    }
    assert_422_at(
        &discount(json!({"tenant_id": Uuid::new_v4(), "effective_from": now, "discount_bps": 500}))
            .await,
        "/tenant_id",
    );
    assert_422_at(
        &discount(json!({"tenant_id": w.tenant, "effective_from": now - TimeDelta::hours(1), "discount_bps": 500})).await,
        "/effective_from",
    );

    let cost = |body: Value| as_op(&w.cp, &w.admin, "POST", "/admin/pricing/cost", Some(body));
    let upstream = |card: Value| json!({"upstream_provider": "openai", "upstream_model": "gpt-4o-mini", "effective_from": now, "card": card});
    assert_422_at(&cost(upstream(Value::Null)).await, "/card");
    assert_422_at(&cost(upstream(no_default.clone())).await, "/card");
    assert_422_at(
        &cost(json!({"upstream_provider": "", "upstream_model": "gpt-4o-mini", "effective_from": now, "card": card()})).await,
        "/upstream_provider",
    );
    assert_422_at(
        &cost(json!({"upstream_provider": "openai", "upstream_model": "", "effective_from": now, "card": card()})).await,
        "/upstream_model",
    );

    // Nothing reached a table.
    for table in [
        "pricing_sell_revisions",
        "pricing_cost_revisions",
        "pricing_discount_revisions",
    ] {
        assert_eq!(w.cp.count(table).await, 0, "{table}");
    }
    assert_eq!(w.cp.count("audit_log").await, audits);
    assert_eq!(pricing_version(&w.cp).await, version);
}

#[tokio::test]
async fn ending_an_override_restores_the_platform_price_and_discount() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let wr = &w;
    let post = move |path: &'static str, body: Value| async move {
        let r = as_op(&wr.cp, &wr.admin, "POST", path, Some(body)).await;
        assert_eq!(r.status, 201, "POST {path}: {r:?}");
        r.body
    };
    let input = |rate: &str| json!({"default": {"base": {"input": rate, "output": "2"}}});
    post(
        "/admin/pricing/sell",
        json!({"model_id": model, "effective_from": db_now(&w.cp).await, "card": input("1")}),
    )
    .await;
    let discount = post(
        "/admin/pricing/discounts",
        json!({"tenant_id": w.tenant, "effective_from": db_now(&w.cp).await, "discount_bps": 5000}),
    )
    .await;
    assert_eq!(discount["discount_bps"], 5000);
    assert_eq!(discount["tenant_id"], json!(w.tenant));
    let r#override = post(
        "/admin/pricing/sell",
        json!({"model_id": model, "tenant_id": w.tenant, "effective_from": db_now(&w.cp).await, "card": input("3")}),
    )
    .await;

    let thousand_in = json!({"prompt_tokens": 1000});
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e-override", model, thousand_in.clone())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let charged = charge_via_api(&w, "e-override").await;
    // The override replaces the price, and is not discounted again.
    assert_eq!(
        (
            charged["status"].clone(),
            charged["charge_micro_usd"].clone(),
            charged["tenant_override"].clone(),
            charged["sell_revision_id"].clone(),
        ),
        (
            json!("charged"),
            json!(3000),
            json!(true),
            r#override["revision_id"].clone()
        )
    );

    let ended = post(
        "/admin/pricing/sell",
        json!({"model_id": model, "tenant_id": w.tenant, "effective_from": db_now(&w.cp).await, "card": null}),
    )
    .await;
    assert_eq!(ended["card"], Value::Null);
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e-platform", model, thousand_in)],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let charged = charge_via_api(&w, "e-platform").await;
    // The platform's 1000 µ$, at the tenant's 50%.
    assert_eq!(
        (
            charged["status"].clone(),
            charged["charge_micro_usd"].clone(),
            charged["tenant_override"].clone(),
            charged["discount_bps"].clone(),
            charged["discount_revision_id"].clone(),
        ),
        (
            json!("charged"),
            json!(500),
            json!(false),
            json!(5000),
            discount["revision_id"].clone()
        )
    );
    let audit = pricing_audit(&w.cp).await;
    assert_eq!(
        audit.last().map(|a| a.0.as_str()),
        Some("pricing.sell.end_override")
    );
}

#[tokio::test]
async fn unpriced_charges_and_invalid_revisions_are_shown_to_operators() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let (unpriced, broken) = (
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    );
    // No output rate: a card no engine can bill with.
    let bad: i64 = sqlx::query_scalar(
        "INSERT INTO pricing_sell_revisions (engine_version, model_id, effective_from, card, source) \
         VALUES ($1, $2, now(), '{\"default\":{\"base\":{\"input\":\"1\"}}}', 'manual') \
         RETURNING revision_id",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(broken)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("write a bad revision");
    bump_pricing_version(&w.cp).await;
    // And a row that is charged, which neither listing nor count may hold.
    let priced = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, priced).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "e-unpriced", unpriced, tokens_680()),
            usage(&w, "e-charged", priced, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");

    let r = as_op(
        &w.cp,
        &viewer,
        "GET",
        "/admin/pricing/charges?status=unpriced",
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{r:?}");
    assert_eq!(
        (
            items[0]["event_id"].clone(),
            items[0]["error_code"].clone(),
            items[0]["charge_micro_usd"].clone()
        ),
        (json!("e-unpriced"), json!("missing_price"), Value::Null)
    );
    assert_eq!(items[0]["tenant_id"], json!(w.tenant));
    assert_eq!(items[0]["lines"], json!([]));
    assert_eq!(items[0]["warnings"], json!([]));

    let r = as_op(&w.cp, &viewer, "GET", "/admin/pricing/status", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["pricing_version"],
        json!(pricing_version(&w.cp).await)
    );
    let invalid = r.body["invalid_revisions"]
        .as_array()
        .expect("invalid revisions");
    assert_eq!(invalid.len(), 1, "{r:?}");
    assert_eq!(
        (
            invalid[0]["table"].clone(),
            invalid[0]["revision_id"].clone()
        ),
        (json!("sell"), json!(bad))
    );
    assert!(
        invalid[0]["reason"].as_str().is_some_and(|m| !m.is_empty()),
        "{r:?}"
    );
    assert_eq!(
        r.body["unpriced_last_24h"],
        json!({"total": 1, "by_code": {"missing_price": 1}})
    );
}

/// `by_code` counts every reason a row was stored unpriced, the control
/// plane's own (a row it cannot attribute or read) as well as the engine's,
/// and only rows priced in the last 24 hours.
#[tokio::test]
async fn every_unpriced_code_is_counted_over_the_last_day() {
    let Some(w) = world().await else { return };
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let (priced, unpriced) = (
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    );
    price(&w.cp, priced).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(
                &w,
                "e-key",
                priced,
                json!({"api_key_id": Uuid::new_v4().to_string(), "prompt_tokens": 100}),
            ),
            usage(
                &w,
                "e-team",
                priced,
                json!({"team_id": other_tenant.to_string(), "prompt_tokens": 100}),
            ),
            usage(&w, "e-many", priced, json!({"prompt_tokens": "many"})),
            usage(
                &w,
                "e-search",
                priced,
                json!({"prompt_tokens": 100, "web_search_requests": 2}),
            ),
            usage(&w, "e-price", unpriced, tokens_680()),
            usage(&w, "e-old", unpriced, tokens_680()),
            usage(&w, "e-ok", priced, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(7, 0, 0)), "{r:?}");
    sqlx::query("UPDATE pricing_charges SET priced_at = now() - interval '25 hours' WHERE event_id = 'e-old'")
        .execute(w.cp.db().pool())
        .await
        .expect("age a charge");
    let r = as_op(&w.cp, &w.admin, "GET", "/admin/pricing/status", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["unpriced_last_24h"],
        json!({"total": 5, "by_code": {
            "unknown_api_key": 1,
            "tenant_mismatch": 1,
            "undecodable_usage": 1,
            "missing_dimension_price": 1,
            "missing_price": 1,
        }}),
        "{r:?}"
    );
    assert_eq!(r.body["invalid_revisions"], json!([]));
}

/// Charges are listed by event, key, tenant and status, newest first and
/// paged; a tenant's listing holds only the rows its own keys made, however
/// a row names its tenant.
#[tokio::test]
async fn charges_are_listed_by_event_key_tenant_and_status() {
    let Some(w) = world().await else { return };
    let victim = seed_tenant(&w.cp, &w.admin).await;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let events: Vec<Value> = (0..3)
        .map(|i| usage(&w, &format!("e{i}"), model, tokens_680()))
        .collect();
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(3, 0, 0)), "{r:?}");
    // The world's key, claiming another tenant, in a later batch.
    let forged = usage(
        &w,
        "e-forged",
        model,
        json!({"team_id": victim.to_string(), "prompt_tokens": 100}),
    );
    let r = deliver(&w.dp, Uuid::new_v4(), vec![forged]).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    let wr = &w;
    let list = move |query: String| async move {
        let r = as_op(
            &wr.cp,
            &wr.admin,
            "GET",
            &format!("/admin/pricing/charges?{query}"),
            None,
        )
        .await;
        assert_eq!(r.status, 200, "{query}: {r:?}");
        let ids: Vec<String> = r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|c| c["event_id"].as_str().expect("an event id").to_string())
            .collect();
        (ids, r.body["next_before"].clone(), r.body["items"].clone())
    };

    let (ids, next, items) = list("event_id=e1".to_string()).await;
    assert_eq!((ids, next), (vec!["e1".to_string()], Value::Null));
    assert_eq!(items[0]["charge_micro_usd"], 680);
    assert_eq!(items[0]["api_key_id"], json!(w.key));
    assert_eq!(items[0]["user_id"], json!(w.member));
    assert_eq!(items[0]["requested_model_id"], json!(model));
    assert_eq!(items[0]["estimated"], false);
    assert!(items[0]["seq"].as_i64().is_some(), "{items}");
    assert!(
        items[0]["lines"].as_array().is_some_and(|l| !l.is_empty()),
        "{items}"
    );
    assert!(list("event_id=e-none".to_string()).await.0.is_empty());

    // The forged row is filed under the key's tenant, never the victim's.
    assert!(list(format!("tenant_id={victim}")).await.0.is_empty());
    let (ids, ..) = list(format!("tenant_id={}", w.tenant)).await;
    assert_eq!(ids, ["e-forged", "e2", "e1", "e0"]);
    let (ids, ..) = list(format!("tenant_id={}&status=unpriced", w.tenant)).await;
    assert_eq!(ids, ["e-forged"]);
    let (ids, ..) = list(format!("api_key_id={}&status=charged", w.key)).await;
    assert_eq!(ids, ["e2", "e1", "e0"]);

    // Paged on seq, newest first.
    let (first, next, _) = list(format!("api_key_id={}&limit=2", w.key)).await;
    assert_eq!(first, ["e-forged", "e2"]);
    let before = next.as_i64().expect("a next page");
    let (rest, next, _) = list(format!("api_key_id={}&limit=2&before={before}", w.key)).await;
    assert_eq!(
        (rest, next),
        (vec!["e1".to_string(), "e0".to_string()], Value::Null)
    );

    let r = as_op(
        &w.cp,
        &w.admin,
        "GET",
        "/admin/pricing/charges?status=billed",
        None,
    )
    .await;
    assert_422_at(&r, "status");
}

#[tokio::test]
async fn cost_prices_and_discounts_are_written_and_listed() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let key_id = seed_provider_key(&w.cp, &w.admin).await;
    let wr = &w;
    let post = move |path: &'static str, body: Value| async move {
        let r = as_op(&wr.cp, &wr.admin, "POST", path, Some(body)).await;
        assert_eq!(r.status, 201, "POST {path}: {r:?}");
        r.body
    };
    let now = db_now(&w.cp).await;
    let general = post(
        "/admin/pricing/cost",
        json!({"upstream_provider": "openai", "upstream_model": "gpt-4o-mini", "effective_from": now, "card": card()}),
    )
    .await;
    assert_eq!(
        (
            general["source"].clone(),
            general["provider_key_id"].clone(),
            general["created_by"].clone()
        ),
        (json!("manual"), Value::Null, json!(w.admin.id))
    );
    let keyed = post(
        "/admin/pricing/cost",
        json!({"upstream_provider": "openai", "upstream_model": "gpt-4o-mini", "provider_key_id": key_id, "effective_from": now, "card": card()}),
    )
    .await;
    post(
        "/admin/pricing/cost",
        json!({"upstream_provider": "openai", "upstream_model": "gpt-4o", "effective_from": now, "card": card()}),
    )
    .await;
    let ids = |r: &Resp| -> Vec<i64> {
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|x| x["revision_id"].as_i64().expect("a revision id"))
            .collect()
    };
    let r = as_op(
        &w.cp,
        &viewer,
        "GET",
        "/admin/pricing/cost?upstream_provider=openai&upstream_model=gpt-4o-mini",
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let (g, k) = (
        general["revision_id"].as_i64().expect("an id"),
        keyed["revision_id"].as_i64().expect("an id"),
    );
    assert_eq!(ids(&r), vec![k, g], "newest first: {r:?}");
    let r = as_op(
        &w.cp,
        &viewer,
        "GET",
        &format!("/admin/pricing/cost?provider_key_id={key_id}"),
        None,
    )
    .await;
    assert_eq!(ids(&r), vec![k]);
    let r = as_op(&w.cp, &viewer, "GET", "/admin/pricing/cost?limit=1", None).await;
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(1));
    assert!(r.body["next_before"].as_i64().is_some(), "{r:?}");

    let mine = post(
        "/admin/pricing/discounts",
        json!({"tenant_id": w.tenant, "effective_from": db_now(&w.cp).await, "discount_bps": 2000}),
    )
    .await;
    post(
        "/admin/pricing/discounts",
        json!({"tenant_id": other_tenant, "effective_from": db_now(&w.cp).await, "discount_bps": 0}),
    )
    .await;
    let r = as_op(
        &w.cp,
        &viewer,
        "GET",
        &format!("/admin/pricing/discounts?tenant_id={}", w.tenant),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["items"], json!([mine]), "{r:?}");
    let r = as_op(&w.cp, &viewer, "GET", "/admin/pricing/discounts", None).await;
    assert_eq!(r.body["items"].as_array().map(Vec::len), Some(2), "{r:?}");
    let r = as_op(
        &w.cp,
        &viewer,
        "GET",
        "/admin/pricing/discounts?tenant_id=not-a-uuid",
        None,
    )
    .await;
    assert_422_at(&r, "query");
}

/// A cost price for one provider key must name a key that exists: a
/// revision for any other would never apply.
#[tokio::test]
async fn a_cost_price_names_a_live_provider_key() {
    let Some(w) = world().await else { return };
    let now = db_now(&w.cp).await;
    let cost = |key: Uuid| {
        json!({
            "upstream_provider": "openai",
            "upstream_model": "gpt-4o-mini",
            "provider_key_id": key,
            "effective_from": now,
            "card": card(),
        })
    };
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/cost",
        Some(cost(Uuid::new_v4())),
    )
    .await;
    assert_422_at(&r, "/provider_key_id");

    let deleted = seed_provider_key(&w.cp, &w.admin).await;
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM gw_resources WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(deleted)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read the key's version");
    let status = send_if_match(
        &w.cp,
        &w.admin,
        "DELETE",
        &format!("/admin/resources/provider_keys/{deleted}"),
        version,
    )
    .await;
    assert_eq!(status, 200, "delete the provider key");
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/cost",
        Some(cost(deleted)),
    )
    .await;
    assert_422_at(&r, "/provider_key_id");
    assert_eq!(w.cp.count("pricing_cost_revisions").await, 0);

    let live = seed_provider_key(&w.cp, &w.admin).await;
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/cost",
        Some(cost(live)),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["provider_key_id"], json!(live));
}

/// A catalogue whose `openai/fixture-7c1` announces two price changes far
/// ahead (input 1, then 3 from 2099-01-01, then 5 from 2099-06-01).
fn announcing_catalogue() -> Arc<Catalogue> {
    let entry = |extra: &str, input: &str| {
        format!(
            r#"{{"key":"openai/fixture-7c1","source_url":"https://openai.com/api/pricing","as_of":"2026-11-01",{extra}"card":{{"default":{{"base":{{"input":"{input}","output":"2"}}}}}}}}"#
        )
    };
    let file = format!(
        r#"{{"version":"2026-11-06","entries":[{},{},{}]}}"#,
        entry("", "1"),
        entry(r#""effective_from":"2099-01-01","#, "3"),
        entry(r#""effective_from":"2099-06-01","#, "5"),
    );
    Arc::new(Catalogue::from_json(&file).expect("the fixture catalogue loads"))
}

fn instant(v: &Value) -> DateTime<Utc> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| panic!("an RFC 3339 instant: {v}"))
}

/// An import writes the price in effect now, and each price change the
/// catalogue announces as a further revision at its own date, marked up the
/// same.
#[tokio::test]
async fn an_import_writes_each_announced_price_change_at_its_own_date() {
    let Some(cp) = TestCp::start_with_catalogue(announcing_catalogue()).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model = seed_model(&cp, &admin).await;
    let now = db_now(&cp).await;
    let r = cp
        .send(
            "POST",
            "/admin/pricing/sell",
            Some(admin.cookie()),
            Some(json!({
                "model_id": model,
                "effective_from": now,
                "catalogue_key": "openai/fixture-7c1",
                "markup_bps": 5000,
            })),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let input = |rev: &Value| rev["card"]["default"]["base"]["input"].clone();
    assert_eq!(instant(&r.body["effective_from"]), now);
    assert_eq!(
        r.body["source"],
        "catalogue:2026-11-06:openai/fixture-7c1@-"
    );
    assert_eq!(input(&r.body), "1.5");
    let later = r.body["later_revisions"]
        .as_array()
        .expect("later revisions");
    let seen: Vec<(DateTime<Utc>, Value, Value, Value)> = later
        .iter()
        .map(|rev| {
            (
                instant(&rev["effective_from"]),
                rev["source"].clone(),
                input(rev),
                rev["model_id"].clone(),
            )
        })
        .collect();
    let day = |s: &str| instant(&json!(s));
    assert_eq!(
        seen,
        vec![
            (
                day("2099-01-01T00:00:00Z"),
                json!("catalogue:2026-11-06:openai/fixture-7c1@2099-01-01"),
                json!("4.5"),
                json!(model),
            ),
            (
                day("2099-06-01T00:00:00Z"),
                json!("catalogue:2026-11-06:openai/fixture-7c1@2099-06-01"),
                json!("7.5"),
                json!(model),
            ),
        ],
        "{r:?}"
    );
    // The changes the import itself wrote are not news to the operator.
    assert_eq!(r.body["warnings"], json!([]), "{r:?}");
    assert_eq!(cp.count("pricing_sell_revisions").await, 3);
    let audit = pricing_audit(&cp).await;
    assert_eq!(audit.len(), 3, "{audit:?}");
}

/// A price written now that a revision already scheduled for later will
/// replace is still written, and the answer names the revision that takes
/// over and when; a later revision of another tenant does not count.
#[tokio::test]
async fn a_price_a_scheduled_revision_will_replace_says_so() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let wr = &w;
    let post = move |body: Value| async move {
        let r = as_op(&wr.cp, &wr.admin, "POST", "/admin/pricing/sell", Some(body)).await;
        assert_eq!(r.status, 201, "{r:?}");
        r.body
    };
    let now = db_now(&w.cp).await;
    let scheduled = post(json!({
        "model_id": model,
        "effective_from": now + TimeDelta::days(1),
        "card": card(),
    }))
    .await;
    assert_eq!(scheduled["warnings"], json!([]));
    let tenant_scheduled = post(json!({
        "model_id": model,
        "tenant_id": w.tenant,
        "effective_from": now + TimeDelta::days(2),
        "card": card(),
    }))
    .await;

    let written =
        post(json!({"model_id": model, "effective_from": db_now(&w.cp).await, "card": card()}))
            .await;
    let warnings = written["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{written}");
    assert_eq!(
        (
            warnings[0]["code"].clone(),
            warnings[0]["revision_id"].clone(),
            warnings[0]["effective_from"].clone(),
        ),
        (
            json!("superseded_later"),
            scheduled["revision_id"].clone(),
            scheduled["effective_from"].clone(),
        )
    );
    assert!(
        warnings[0]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{written}"
    );
    assert_eq!(w.cp.count("pricing_sell_revisions").await, 3);

    // The tenant's own override, written now, meets only the tenant's.
    let override_now = post(json!({
        "model_id": model,
        "tenant_id": w.tenant,
        "effective_from": db_now(&w.cp).await,
        "card": card(),
    }))
    .await;
    let ids: Vec<Value> = override_now["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .map(|x| x["revision_id"].clone())
        .collect();
    assert_eq!(ids, vec![tenant_scheduled["revision_id"].clone()]);
}
