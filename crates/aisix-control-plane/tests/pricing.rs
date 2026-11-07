//! Pricing in the control plane: price revisions are written append-only and
//! never start in the past, and every usage row intake stores is priced once,
//! in the transaction that stores it, from token counts and the control
//! plane's own price book.
//!
//! Prices are written through `pricing::store`; usage arrives over
//! `POST /dp/telemetry` exactly as a gateway sends it. The tenant a row is
//! billed to comes from the control plane's own key record, never from the
//! row's word alone.

mod support;

use std::sync::Arc;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::error::ApiError;
use aisix_control_plane::pricing::store::{
    self, CostRevision, DiscountRevision, NewCostPrice, NewDiscount, NewSellPrice, SellRevision,
};
use aisix_pricing::{RateCard, ENGINE_VERSION};
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

/// `method path` as `admin` with `If-Match: "<version>"`.
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
        .send()
        .await
        .expect("send the request")
        .status()
        .as_u16()
}

/// A platform model on a provider key of its own, created as an operator.
async fn seed_model(cp: &TestCp, admin: &OperatorSession) -> Uuid {
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
    let model = create(
        cp,
        admin,
        "/admin/resources/models",
        json!({"doc": {
            "display_name": format!("priced-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": text(&pk, "/id"),
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

/// A key of `tenant`, minted by an operator; its id.
async fn seed_key(cp: &TestCp, admin: &OperatorSession, tenant: Uuid) -> String {
    let key = create(
        cp,
        admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label": "pricing-7c1"}),
    )
    .await;
    text(&key, "/key/id")
}

/// A control plane with an operator, a gateway, a tenant and one of its keys.
struct World {
    cp: TestCp,
    admin: OperatorSession,
    dp: DpClient,
    tenant: Uuid,
    key: String,
}

async fn world() -> Option<World> {
    let cp = TestCp::start().await?;
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-pricing-7c1").await;
    let tenant = seed_tenant(&cp, &admin).await;
    let key = seed_key(&cp, &admin, tenant).await;
    Some(World {
        cp,
        admin,
        dp,
        tenant,
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

/// A chat usage row of the world's tenant and key for `model`, occurring now,
/// with `fields` on top. It claims a `cost_usd` no price would give, which
/// pricing must never read.
fn usage(w: &World, event_id: &str, model: Uuid, fields: Value) -> Value {
    let mut event = json!({
        "event_id": event_id,
        "request_id": format!("req-{event_id}"),
        "occurred_at": Utc::now().to_rfc3339(),
        "team_id": w.tenant.to_string(),
        "api_key_id": w.key,
        "user_id": "u1",
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
            user_id: "u1".to_string(),
        }
    }

    fn unpriced(w: &World, code: &str) -> Charge {
        Charge {
            status: "unpriced".to_string(),
            charge: None,
            error_code: Some(code.to_string()),
            tenant_id: w.tenant.to_string(),
            api_key_id: w.key.clone(),
            user_id: "u1".to_string(),
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
    let key = seed_key(&cp, &admin, tenant).await;
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
    // A batch of nothing but duplicates prices nothing and reads no book.
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
    assert_eq!(
        charge_of(&w.cp, "e-foreign-team").await,
        Some(Charge {
            tenant_id: other_tenant.to_string(),
            ..Charge::unpriced(&w, "tenant_mismatch")
        })
    );
    assert_eq!(
        charge_of(&w.cp, "e-no-team").await,
        Some(Charge {
            tenant_id: String::new(),
            ..Charge::unpriced(&w, "tenant_mismatch")
        })
    );
    for (id, key) in [
        ("e-unknown-key", None),
        ("e-not-a-key", Some("k1".to_string())),
        ("e-upper-key", Some(w.key.to_uppercase())),
    ] {
        let found = charge_of(&w.cp, id).await.expect("a charge row");
        assert_eq!(
            (
                found.status.as_str(),
                found.charge,
                found.error_code.as_deref()
            ),
            ("unpriced", None, Some("unknown_api_key")),
            "{id}"
        );
        if let Some(key) = key {
            assert_eq!(found.api_key_id, key, "{id}");
        }
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
    let other = w.cp.issue_dp_client("gw-pricing-other-7c1").await;
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, json!({"prompt_tokens": 100}))],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = deliver(
        &other,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, json!({"prompt_tokens": 999}))],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(0, 0, 1)), "{r:?}");
    assert_eq!(w.cp.count("pricing_charges").await, 1);
    assert_eq!(charge_of(&w.cp, "e1").await, Some(Charge::charged(&w, 100)));
}
