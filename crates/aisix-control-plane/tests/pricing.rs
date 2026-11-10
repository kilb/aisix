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
use aisix_control_plane::pricing::cost_rates::{self, CostRateRevision, NewCostRate};
use aisix_control_plane::pricing::store::{self, DiscountRevision, NewDiscount};
use aisix_pricing::{Catalogue, CnyPerUsd, RateCard, ENGINE_VERSION};
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use support::world::{
    card, db_now, deliver, insert_baseline, insert_rate, price, seed_key, seed_model,
    seed_provider_key, seed_tenant, tokens_680, usage, world, World,
};
use support::{OperatorSession, Resp, TestCp};
use tokio::sync::Barrier;
use uuid::Uuid;

const DEDUP: &str = "x-aisix-usage-batch-dedup";

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

async fn insert_cost_rate(cp: &TestCp, row: NewCostRate) -> Result<CostRateRevision, ApiError> {
    cp.db()
        .tx(move |c| {
            async move {
                cost_rates::insert_cost_rate(c, &row, Actor::System)
                    .await
                    .map(|(revision, _)| revision)
            }
            .boxed()
        })
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
        "SELECT status, charge_micro_cny AS charge, error_code, tenant_id, api_key_id, user_id \
         FROM pricing_charges_cny WHERE event_id = $1",
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
    assert_eq!(w.cp.count("pricing_charges_cny").await, 1);
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
    assert_eq!(w.cp.count("pricing_charges_cny").await, 1);
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
    let err = insert_baseline(&w.cp, model, now - TimeDelta::minutes(1), card())
        .await
        .expect_err("a price that started a minute ago");
    assert_refused_at(&err, "/effective_from");
    assert_eq!(w.cp.count("pricing_baselines").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);
    assert_eq!(pricing_version(&w.cp).await, version);

    // Inside the 5 s tolerance.
    let now = db_now(&w.cp).await;
    let rev = insert_baseline(&w.cp, model, now - TimeDelta::seconds(2), card())
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
        "INSERT INTO pricing_baselines \
         (engine_version, model_id, effective_from, card, source_kind, source) \
         VALUES ($1, $2, now(), '{\"default\":{\"base\":{\"input\":\"1\"}}}', 'manual', '{}')",
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
        "INSERT INTO pricing_baselines \
         (engine_version, model_id, effective_from, card, source_kind, source) \
         VALUES ($1, $2, now(), $3, 'manual', '{}')",
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
    sqlx::query("LOCK TABLE pricing_baselines IN ACCESS EXCLUSIVE MODE")
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
    for table in ["usage_batches", "usage_events", "pricing_charges_cny"] {
        assert_eq!(w.cp.count(table).await, 0, "{table}");
    }

    sqlx::query("COMMIT")
        .execute(&mut holder)
        .await
        .expect("release the lock");
    let r = deliver(&w.dp, b, events).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(w.cp.count("usage_events").await, 1);
    assert_eq!(w.cp.count("pricing_charges_cny").await, 1);
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
    assert_eq!(w.cp.count("pricing_charges_cny").await, 4);
}

/// Readers that all find the book stale after a price write share one load:
/// none of them loads it again once the first has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readers_of_a_stale_book_share_one_load() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let prices = w.cp.state().prices.clone();
    // Connections are opened first, so the readers do not queue on the pool.
    let mut conns = Vec::new();
    for _ in 0..6 {
        conns.push(w.cp.db().pool().acquire().await.expect("a connection"));
    }
    let mut first = conns.pop().expect("a connection");
    prices.current(&mut first).await.expect("the book");
    assert_eq!(prices.loads(), 1);
    bump_pricing_version(&w.cp).await;
    let readers: Vec<_> = conns
        .into_iter()
        .map(|mut conn| {
            let prices = prices.clone();
            tokio::spawn(async move { prices.current(&mut conn).await.expect("the book") })
        })
        .collect();
    let mut books = Vec::new();
    for reader in readers {
        books.push(reader.await.expect("the reader finished"));
    }
    assert_eq!(prices.loads(), 2, "five readers, one load");
    assert!(books.windows(2).all(|p| Arc::ptr_eq(&p[0], &p[1])));
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
    let err = insert_baseline(&w.cp, model, now, no_default)
        .await
        .expect_err("a card without a default tier");
    assert_refused_at(&err, "/card");
    assert_eq!(w.cp.count("pricing_baselines").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);
    assert_eq!(pricing_version(&w.cp).await, version);
}

#[tokio::test]
async fn a_price_for_a_deleted_or_unknown_model_is_refused() {
    let Some(w) = world().await else { return };
    let now = db_now(&w.cp).await;
    let err = insert_baseline(&w.cp, Uuid::new_v4(), now, card())
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
    let err = insert_baseline(&w.cp, model, now, card())
        .await
        .expect_err("a deleted model");
    assert_refused_at(&err, "/model_id");

    // A rate for an unknown group is `404 group_not_found`, and one for a
    // retired group `409 group_retired`: neither can price a row.
    let err = insert_rate(&w.cp, Uuid::new_v4(), now)
        .await
        .expect_err("an unknown group");
    assert_eq!(err.status.as_u16(), 404, "{err:?}");
    assert_eq!(w.cp.count("pricing_baselines").await, 0);
    assert_eq!(w.cp.count("pricing_group_rates").await, 0);
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
    // The world's `price()` writes the group's rate, then the baseline.
    assert_eq!(audit.len(), 2, "{audit:?}");
    let (action, kind, target, tenant, detail) = &audit[1];
    assert_eq!(
        (action.as_str(), kind.as_str(), target.clone(), *tenant),
        (
            "pricing.baseline.create",
            "pricing_baseline",
            Some(rev.revision_id.to_string()),
            None
        )
    );
    assert_eq!(detail["model_id"], json!(model));
    assert_eq!(detail["source_kind"], json!("manual"));
    assert_eq!(audit[0].0, "pricing.group_rate.create");
    assert_eq!(audit[0].1, "pricing_group_rate");
    assert_eq!(pricing_version(&w.cp).await, version + 2);
}

#[tokio::test]
async fn cost_rates_and_discounts_follow_the_same_rules() {
    let Some(w) = world().await else { return };
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let version = pricing_version(&w.cp).await;
    let now = db_now(&w.cp).await;
    let cost = |effective_from: DateTime<Utc>, rate: Option<&str>| NewCostRate {
        provider_key_id: account,
        model_id: None,
        effective_from,
        cny_per_usd: rate.map(|r| CnyPerUsd::parse(r).expect("a rate")),
        note: String::new(),
    };
    assert_refused_at(
        &insert_cost_rate(&w.cp, cost(now, None))
            .await
            .expect_err("a default with no rate"),
        "/cny_per_usd",
    );
    assert_refused_at(
        &insert_cost_rate(&w.cp, cost(now - TimeDelta::minutes(1), Some("0.3")))
            .await
            .expect_err("in the past"),
        "/effective_from",
    );
    let rev = insert_cost_rate(&w.cp, cost(now, Some("0.3")))
        .await
        .expect("a cost rate");
    assert_eq!(rev.engine_version, i64::from(ENGINE_VERSION));
    assert_eq!(rev.cny_per_usd.as_deref(), Some("0.3"));

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
    assert_eq!(
        actions,
        vec![
            ("pricing.cost_rate.create", "pricing_cost_rate", None),
            (
                "pricing.discount.create",
                "pricing_discount_revision",
                Some(w.tenant)
            ),
        ]
    );
    assert_eq!(pricing_version(&w.cp).await, version + 2);
    assert_eq!(w.cp.count("pricing_cost_rates").await, 1);
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
    assert_eq!(w.cp.count("pricing_charges_cny").await, 1);
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

#[tokio::test]
async fn an_operator_sets_a_baseline_and_a_viewer_reads_it() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let model = seed_model(&w.cp, &w.admin).await;
    let other = seed_model(&w.cp, &w.admin).await;
    let audits = w.cp.count("audit_log").await;
    let body = json!({
        "model_id": model,
        "effective_from": db_now(&w.cp).await,
        "card": {"default": {"base": {"input": "1", "output": "2", "cache_read": "0.1"}}},
        "note": "the operator's own card",
    });
    let r = as_op(
        &w.cp,
        &viewer,
        "POST",
        "/admin/pricing/baselines",
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (403, json!("forbidden")),
        "{r:?}"
    );
    assert_eq!(w.cp.count("pricing_baselines").await, 0);
    assert_eq!(w.cp.count("audit_log").await, audits);

    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/baselines",
        Some(body),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let rev = r.body["revision"].clone();
    assert_eq!(rev["model_id"], json!(model));
    assert_eq!(rev["source_kind"], "manual");
    assert_eq!(rev["source"]["note"], "the operator's own card");
    assert_eq!(rev["engine_version"], ENGINE_VERSION);
    assert_eq!(rev["created_by"]["id"], json!(w.admin.id));
    assert_eq!(rev["card"]["default"]["base"]["input"], "1");
    assert_eq!(r.body["warnings"], json!([]));
    // A price for another model, which the listing must include too.
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/baselines",
        Some(json!({
            "model_id": other,
            "effective_from": db_now(&w.cp).await,
            "card": card(),
            "note": "another model's card",
        })),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    // Written by the operator, in the audit log.
    let audit: (String, Option<Uuid>, String) = sqlx::query_as(
        "SELECT actor_kind, actor_id, action FROM audit_log WHERE target_id = $1 \
         AND target_kind = 'pricing_baseline'",
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
            "pricing.baseline.create".to_string()
        )
    );

    let r = as_op(&w.cp, &viewer, "GET", "/admin/pricing/baselines", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    let mine = items
        .iter()
        .find(|i| i["model_id"] == json!(model))
        .expect("the model's row");
    assert_eq!(mine["current"]["revision_id"], rev["revision_id"], "{r:?}");
    assert_eq!(mine["current"]["card"]["default"]["base"]["input"], "1");
}

/// A price write that does not say it is JSON is `415` and writes nothing,
/// as on every mutating browser route: a form another site posts with an
/// operator's session cannot set a price.
#[tokio::test]
async fn a_price_write_that_is_not_json_is_415_and_writes_nothing() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let now = db_now(&w.cp).await;
    let version = pricing_version(&w.cp).await;
    let audits = w.cp.count("audit_log").await;
    let writes = [
        (
            "/admin/pricing/baselines".to_string(),
            json!({"model_id": model, "effective_from": now, "card": card(), "note": "x"}),
        ),
        (
            format!("/admin/provider-keys/{account}/cost-rates"),
            json!({"model_id": null, "cny_per_usd": "0.3", "effective_from": now}),
        ),
        (
            "/admin/pricing/discounts".to_string(),
            json!({"tenant_id": w.tenant, "effective_from": now, "discount_bps": 2000}),
        ),
    ];
    for (path, body) in writes {
        let bytes = serde_json::to_vec(&body).expect("serialize the body");
        for content_type in [None, Some("text/plain")] {
            let r =
                w.cp.send_raw(
                    "POST",
                    &path,
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
        "pricing_baselines",
        "pricing_cost_rates",
        "pricing_discount_revisions",
    ] {
        assert_eq!(w.cp.count(table).await, 0, "{table}");
    }
    assert_eq!(pricing_version(&w.cp).await, version);
    assert_eq!(w.cp.count("audit_log").await, audits);
}

#[tokio::test]
async fn a_bad_price_is_422_with_the_field_path() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let now = db_now(&w.cp).await;
    let audits = w.cp.count("audit_log").await;
    let version = pricing_version(&w.cp).await;
    let baseline = |body: Value| {
        as_op(
            &w.cp,
            &w.admin,
            "POST",
            "/admin/pricing/baselines",
            Some(body),
        )
    };
    let card_value = || serde_json::to_value(card()).expect("the card serializes");
    let noted = |model: Uuid, card: Value, extra: Value| {
        let mut body = json!({"model_id": model, "effective_from": now, "card": card,
                              "note": "the test's own card"});
        for (k, v) in extra.as_object().expect("an object") {
            body[k] = v.clone();
        }
        body
    };

    let no_default = json!({"priority": {"base": {"input": "1", "output": "2"}}});
    assert_422_at(
        &baseline(noted(model, no_default.clone(), json!({}))).await,
        "/card",
    );
    assert_422_at(
        &baseline(noted(
            model,
            json!({"default": {"base": {"input": "one", "output": "2"}}}),
            json!({}),
        ))
        .await,
        "/card",
    );
    // Both a card and a catalogue key, or neither.
    assert_422_at(
        &baseline(noted(
            model,
            card_value(),
            json!({"catalogue_key": "openai/gpt-5.4"}),
        ))
        .await,
        "/card",
    );
    assert_422_at(
        &as_op(
            &w.cp,
            &w.admin,
            "POST",
            "/admin/pricing/baselines",
            Some(json!({"model_id": model, "effective_from": now})),
        )
        .await,
        "/card",
    );
    assert_422_at(
        &baseline(noted(model, Value::Null, json!({}))).await,
        "/card",
    );
    // A manual card needs a note, and a catalogue key refuses one.
    assert_422_at(
        &as_op(
            &w.cp,
            &w.admin,
            "POST",
            "/admin/pricing/baselines",
            Some(json!({"model_id": model, "effective_from": now, "card": card()})),
        )
        .await,
        "/note",
    );
    assert_422_at(
        &baseline(json!({"model_id": model, "effective_from": now,
                         "catalogue_key": "openai/gpt-5.4", "note": "x"}))
        .await,
        "/note",
    );
    assert_422_at(
        &baseline(json!({"model_id": model, "effective_from": now,
                         "catalogue_key": "openai/no-such-model-7c1"}))
        .await,
        "/catalogue_key",
    );
    assert_422_at(
        &baseline(json!({"model_id": model, "effective_from": now,
                         "catalogue_key": "no-provider-7c1"}))
        .await,
        "/catalogue_key",
    );
    assert_422_at(
        &baseline(noted(
            model,
            card_value(),
            json!({"effective_from": now - TimeDelta::hours(1)}),
        ))
        .await,
        "/effective_from",
    );
    // A rate is 0 to 100 with at most four decimals, and a group's default
    // row must carry one.
    let group = support::world::group_id(&w.cp, support::world::SHARED_GROUP).await;
    let rates_path = format!("/admin/provider-groups/{group}/rates");
    let rate = |body: Value| as_op(&w.cp, &w.admin, "POST", &rates_path, Some(body));
    for bad in ["-1", "100.0001", "1.00001", "abc"] {
        assert_422_at(
            &rate(json!({"model_id": null, "cny_per_usd": bad, "effective_from": now})).await,
            "/cny_per_usd",
        );
    }
    assert_422_at(
        &rate(json!({"model_id": null, "cny_per_usd": null, "effective_from": now})).await,
        "/cny_per_usd",
    );
    // A cost rate is a rate like a group's: the same bounds, and an
    // account's default row must carry one.
    let cost_path = format!("/admin/provider-keys/{account}/cost-rates");
    let cost = |body: Value| as_op(&w.cp, &w.admin, "POST", &cost_path, Some(body));
    for bad in [
        json!("-1"),
        json!("100.0001"),
        json!("1.00001"),
        json!("abc"),
        json!(0.3),
    ] {
        assert_422_at(
            &cost(json!({"model_id": null, "cny_per_usd": bad, "effective_from": now})).await,
            "/cny_per_usd",
        );
    }
    assert_422_at(
        &cost(json!({"model_id": null, "cny_per_usd": null, "effective_from": now})).await,
        "/cny_per_usd",
    );
    assert_422_at(
        &cost(json!({"model_id": null, "cny_per_usd": "0.3",
                     "effective_from": now - TimeDelta::hours(1)}))
        .await,
        "/effective_from",
    );
    assert_422_at(
        &cost(json!({"model_id": Uuid::new_v4(), "cny_per_usd": "0.3", "effective_from": now}))
            .await,
        "/model_id",
    );

    // Nothing reached a table.
    for table in [
        "pricing_baselines",
        "pricing_cost_rates",
        "pricing_discount_revisions",
    ] {
        assert_eq!(w.cp.count(table).await, 0, "{table}");
    }
    assert_eq!(w.cp.count("audit_log").await, audits);
    assert_eq!(pricing_version(&w.cp).await, version);
}

/// An unreadable baseline is isolated to its model, and both it and the
/// unpriced charges it leaves are what the status page exists to show.
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
        "INSERT INTO pricing_baselines \
         (engine_version, model_id, effective_from, card, source_kind, source) \
         VALUES ($1, $2, now(), '{\"default\":{\"base\":{\"input\":\"1\"}}}', 'manual', '{}') \
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
            items[0]["charge_micro_cny"].clone()
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
        (json!("baseline"), json!(bad))
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
    sqlx::query(
        "UPDATE pricing_charges_cny SET priced_at = now() - interval '25 hours' \
         WHERE event_id = 'e-old'",
    )
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
    assert_eq!(items[0]["charge_micro_cny"], 680);
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

/// The `?status=` filter of the charge listing pages on `(status, seq)`, so
/// it must not rely on the planner choosing another index: that pair has an
/// index of its own.
#[tokio::test]
async fn the_charge_status_filter_has_its_own_index() {
    let Some(w) = world().await else { return };
    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT indexdef FROM pg_indexes WHERE tablename = 'pricing_charges_cny'",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("read the indexes");
    assert!(
        defs.iter().any(|d| d.contains("(status, seq)")),
        "no (status, seq) index on pricing_charges_cny: {defs:?}"
    );
}

/// A per-dimension cost card written before cost became a rate, by raw SQL
/// as the retired route wrote it; its revision id.
async fn legacy_cost_card(cp: &TestCp, model: &str, key: Option<Uuid>) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO pricing_cost_revisions \
         (engine_version, upstream_provider, upstream_model, provider_key_id, effective_from, \
          card, source) \
         VALUES ($1, 'openai', $2, $3, now(), $4, 'manual') RETURNING revision_id",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(model)
    .bind(key)
    .bind(serde_json::to_value(card()).expect("a card"))
    .fetch_one(cp.db().pool())
    .await
    .expect("write a legacy cost card")
}

/// The cost cards are history: they are still listed, nothing writes one,
/// and none of them costs a row.
#[tokio::test]
async fn the_retired_cost_cards_are_listed_and_never_written_or_read() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let key_id = seed_provider_key(&w.cp, &w.admin).await;
    let g = legacy_cost_card(&w.cp, "gpt-4o-mini", None).await;
    let k = legacy_cost_card(&w.cp, "gpt-4o-mini", Some(key_id)).await;
    legacy_cost_card(&w.cp, "gpt-4o", None).await;

    // The write route is gone: a POST finds no route (the method is not
    // allowed, answered as every unknown route is) and writes nothing.
    let now = db_now(&w.cp).await;
    let r = as_op(
        &w.cp,
        &w.admin,
        "POST",
        "/admin/pricing/cost",
        Some(
            json!({"upstream_provider": "openai", "upstream_model": "gpt-4o-mini",
                    "effective_from": now, "card": card()}),
        ),
    )
    .await;
    assert_eq!(
        (r.status, r.header("allow")),
        (404, Some("GET,HEAD")),
        "{r:?}"
    );
    assert_eq!(w.cp.count("pricing_cost_revisions").await, 3);

    // A row the cards match exactly, served through the keyed card's
    // account: no CNY cost, since cost is now a rate and the account has
    // none, and no USD cost, since the cards are not read.
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let mut row = usage(&w, "legacy-1", model, tokens_680());
    row["provider_key_id"] = json!(key_id);
    row["upstream_provider"] = json!("openai");
    row["upstream_model"] = json!("gpt-4o-mini");
    let r = deliver(&w.dp, Uuid::new_v4(), vec![row]).await;
    assert_eq!(r.status, 200, "{r:?}");
    let costs: (Option<i64>, Option<i64>, Option<Value>) = sqlx::query_as(
        "SELECT cost_micro_cny, cost_micro_usd, cost_source FROM pricing_charges_cny \
         WHERE event_id = 'legacy-1'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read the charge");
    assert_eq!(costs, (None, None, None));
    assert_eq!(
        charge_of(&w.cp, "legacy-1").await,
        Some(Charge::charged(&w, 680))
    );

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
}

#[tokio::test]
async fn discounts_are_written_and_listed() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let other_tenant = seed_tenant(&w.cp, &w.admin).await;
    let wr = &w;
    let post = move |path: &'static str, body: Value| async move {
        let r = as_op(&wr.cp, &wr.admin, "POST", path, Some(body)).await;
        assert_eq!(r.status, 201, "POST {path}: {r:?}");
        r.body
    };
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

/// A catalogue whose `openai/fixture-7c1` announces two price changes far
/// ahead (input 1, then 3 from 2099-01-01, then 5 from 2099-06-01).
#[allow(dead_code)]
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

#[allow(dead_code)]
fn instant(v: &Value) -> DateTime<Utc> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| panic!("an RFC 3339 instant: {v}"))
}

/// The `shared` group's id.
async fn shared_group(cp: &TestCp) -> Uuid {
    support::world::group_id(cp, support::world::SHARED_GROUP).await
}

/// A new group named `name` (`cheap-<random>` unless given) whose default
/// rate is `cny_per_usd`; its id.
async fn new_group(cp: &TestCp, admin: &OperatorSession, name: &str, cny_per_usd: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/provider-groups",
            Some(admin.cookie()),
            Some(json!({"name": name, "display_name": name, "default_cny_per_usd": cny_per_usd})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a group id")).expect("a uuid")
}

/// `shared`'s default rate of `cny_per_usd`, starting now.
async fn set_shared_rate(cp: &TestCp, admin: &OperatorSession, cny_per_usd: &str) {
    let now = db_now(cp).await;
    let r = cp
        .send(
            "POST",
            &format!("/admin/provider-groups/{}/rates", shared_group(cp).await),
            Some(admin.cookie()),
            Some(json!({"model_id": null, "cny_per_usd": cny_per_usd, "effective_from": now})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
}

/// A key of `tenant` naming the live group `group`.
async fn key_in_group(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, group: Uuid) -> String {
    let name: String = sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
        .bind(group)
        .fetch_one(cp.db().pool())
        .await
        .expect("the group's name");
    // Assign the group to the tenant first: a customer key may name only a
    // group its tenant is assigned.
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the tenant");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let r = client
        .patch(cp.url(&format!("/admin/tenants/{tenant}")))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_provider_groups": [name]}))
        .send()
        .await
        .expect("send");
    assert_eq!(
        r.status().as_u16(),
        200,
        "assign the group: {:?}",
        r.text().await
    );
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            Some(json!({"label": "group-7c1", "group_id": group})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body["key"]["id"].as_str().expect("a key id").to_string()
}

// ---- the group's rate prices the row ----

#[tokio::test]
async fn a_row_is_charged_at_its_keys_group_rate() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    // `shared` at 7.2 CNY per USD: 680 µ$ × 7.2 is 4 896 µ¥.
    set_shared_rate(&w.cp, &w.admin, "7.2").await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(Charge::charged(&w, 4_896))
    );
    // And the ledger holds exactly that debit.
    let entry: (String, i64) = sqlx::query_as(
        "SELECT kind, amount_micro_cny FROM billing_ledger_cny WHERE tenant_id = $1",
    )
    .bind(w.tenant)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the debit");
    assert_eq!(entry, ("usage".to_string(), -4_896));
}

#[tokio::test]
async fn two_keys_of_one_tenant_in_two_groups_pay_two_prices() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    // A baseline of `card()` and no rate yet: the rows below are charged only
    // once each group has one.
    let now = db_now(&w.cp).await;
    insert_baseline(&w.cp, model, now, card())
        .await
        .expect("the baseline");
    let cheap = new_group(
        &w.cp,
        &w.admin,
        &format!("cheap-{}", Uuid::new_v4().simple()),
        "1",
    )
    .await;
    // The tenant is assigned both groups, and has a key in each.
    let shared = shared_group(&w.cp).await;
    let name: String = sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
        .bind(cheap)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the cheap group's name");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(w.tenant)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read the tenant");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let r = client
        .patch(w.cp.url(&format!("/admin/tenants/{}", w.tenant)))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, w.admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_provider_groups": [support::world::SHARED_GROUP, name]}))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status().as_u16(), 200, "assign the groups");
    let r =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{}/keys", w.tenant),
            Some(w.admin.cookie()),
            Some(json!({"label": "cheap-7c1", "group_id": cheap, "member_id": w.member})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let cheap_key = r.body["key"]["id"].as_str().expect("a key id").to_string();
    // 7.2 on `shared`, 5.0 on `cheap`: one row of 680 µ$ is 4 896 and 3 400.
    set_shared_rate(&w.cp, &w.admin, "7.2").await;
    let r =
        w.cp.send(
            "POST",
            &format!("/admin/provider-groups/{cheap}/rates"),
            Some(w.admin.cookie()),
            Some(json!({"model_id": null, "cny_per_usd": "5",
                        "effective_from": db_now(&w.cp).await})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");

    let mut cheap_row = usage(&w, "e-cheap", model, tokens_680());
    cheap_row["api_key_id"] = json!(cheap_key);
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e-shared", model, tokens_680()), cheap_row],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e-shared").await,
        Some(Charge::charged(&w, 4_896))
    );
    let cheap_charge: Option<i64> = sqlx::query_scalar(
        "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = 'e-cheap'",
    )
    .fetch_optional(w.cp.db().pool())
    .await
    .expect("read the charge");
    assert_eq!(cheap_charge, Some(3_400));
    let _ = shared;
}

#[tokio::test]
async fn the_charge_records_its_group_rate_baseline_and_discount() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let baseline = price(&w.cp, model).await;
    let group = shared_group(&w.cp).await;
    set_shared_rate(&w.cp, &w.admin, "7.2").await;
    // `shared`'s default rate revision in force: the one the charge must
    // record.
    let rate: i64 = sqlx::query_scalar(
        "SELECT revision_id FROM pricing_group_rates \
         WHERE group_id = $1 AND model_id IS NULL AND cny_per_usd_e4 = 72000",
    )
    .bind(group)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the rate revision");
    let discount: i64 = sqlx::query_scalar(
        "INSERT INTO pricing_discount_revisions \
         (engine_version, tenant_id, effective_from, discount_bps) VALUES ($1, $2, now(), 1000) \
         RETURNING revision_id",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(w.tenant)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("write a discount");
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    // 680 µ$ × 7.2 × 0.9 = 4 406.4, rounded up once: 4 407 µ¥.
    /// `charge`, `group`, `baseline`, `rate`, `cny_per_usd_e4`, the override
    /// flag, `discount_bps`, `discount_revision` of one charge.
    type Charged = (
        Option<i64>,
        Option<Uuid>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        bool,
        i64,
        Option<i64>,
    );
    let row: Charged = sqlx::query_as(
        "SELECT charge_micro_cny, group_id, baseline_revision_id, rate_revision_id, \
                cny_per_usd_e4, rate_is_model_override, discount_bps, discount_revision_id \
         FROM pricing_charges_cny WHERE event_id = 'e1'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read the charge");
    assert_eq!(row.0, Some(4_407));
    assert_eq!(row.1, Some(group));
    assert_eq!(row.2, Some(baseline.revision_id));
    assert_eq!(row.3, Some(rate), "the rate revision in force: {row:?}");
    assert_eq!(row.4, Some(72_000));
    assert!(!row.5);
    assert_eq!(row.6, 1_000);
    assert_eq!(row.7, Some(discount));
}

#[tokio::test]
async fn a_row_of_a_key_with_no_group_is_unpriced_no_group() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    // A legacy key an older replica minted without a group.
    sqlx::query("UPDATE tenant_keys SET group_id = NULL WHERE id = $1")
        .bind(Uuid::parse_str(&w.key).expect("a key id"))
        .execute(w.cp.db().pool())
        .await
        .expect("clear the key's group");
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(Charge::unpriced(&w, "no_group"))
    );
    let filed: Option<Uuid> =
        sqlx::query_scalar("SELECT group_id FROM pricing_charges_cny WHERE event_id = 'e1'")
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the charge's group");
    assert_eq!(filed, None);
}

/// A row the engine refuses is still filed under its key's group: the charge
/// says which group's rate a re-pricing run will bill it at, and which group a
/// missing price or rate belongs to.
#[tokio::test]
async fn an_unpriced_row_of_a_grouped_key_records_its_group() {
    let Some(w) = world().await else { return };
    // A model with no baseline, on a key whose group has a rate.
    let model = seed_model(&w.cp, &w.admin).await;
    set_shared_rate(&w.cp, &w.admin, "7.2").await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![usage(&w, "e1", model, tokens_680())],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    assert_eq!(
        charge_of(&w.cp, "e1").await,
        Some(Charge::unpriced(&w, "missing_price"))
    );
    let filed: Option<Uuid> =
        sqlx::query_scalar("SELECT group_id FROM pricing_charges_cny WHERE event_id = 'e1'")
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the charge's group");
    assert_eq!(filed, Some(shared_group(&w.cp).await));
}

#[tokio::test]
async fn an_internal_keys_usage_is_charged_and_posted_like_a_customers() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    set_shared_rate(&w.cp, &w.admin, "7.2").await;
    // The platform's own tenant funds itself postpaid and is priced like any
    // other: its metrics `platform_own` exemption went with owner decision 8.
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the internal tenant");
    let mode: String = sqlx::query_scalar("SELECT billing_mode FROM tenants WHERE id = $1")
        .bind(internal)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read its mode");
    assert_eq!(mode, "postpaid");
    let r =
        w.cp.send(
            "POST",
            &format!("/admin/tenants/{internal}/keys"),
            Some(w.admin.cookie()),
            Some(json!({"label": "ops-7c1", "group_id": shared_group(&w.cp).await})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let key = r.body["key"]["id"].as_str().expect("a key id").to_string();
    let mut row = usage(&w, "e-internal", model, tokens_680());
    row["team_id"] = json!(internal.to_string());
    row["api_key_id"] = json!(key);
    row["user_id"] = json!("");
    let r = deliver(&w.dp, Uuid::new_v4(), vec![row]).await;
    assert_eq!((r.status, r.body.clone()), (200, counts(1, 0, 0)), "{r:?}");
    let charge: Option<i64> = sqlx::query_scalar(
        "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = 'e-internal'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read the charge");
    assert_eq!(charge, Some(4_896));
    // Posted to its ledger in the same transaction, and no budgets document
    // is written for it: marks skip the platform's own tenant.
    let entry: (String, i64) = sqlx::query_as(
        "SELECT kind, amount_micro_cny FROM billing_ledger_cny WHERE tenant_id = $1",
    )
    .bind(internal)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the internal tenant's debit");
    assert_eq!(entry, ("usage".to_string(), -4_896));
    let budgets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE kind = 'budgets' AND managed_by = $1",
    )
    .bind(format!("tenant:{internal}"))
    .fetch_one(w.cp.db().pool())
    .await
    .expect("count its documents");
    assert_eq!(budgets, 0, "the internal tenant has no budgets document");
}

#[tokio::test]
async fn a_free_group_and_a_full_discount_post_nothing_and_keep_their_batch() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    // T0 in a group whose rate is 0, T1 on `shared` at 7.2 with a 100 %
    // discount, T2 on `shared` at 7.2 paying.
    let free = new_group(
        &w.cp,
        &w.admin,
        &format!("free-{}", Uuid::new_v4().simple()),
        "0",
    )
    .await;
    set_shared_rate(&w.cp, &w.admin, "7.2").await;
    let t0 = seed_tenant(&w.cp, &w.admin).await;
    let t1 = seed_tenant(&w.cp, &w.admin).await;
    let free_key = key_in_group(&w.cp, &w.admin, t0, free).await;
    let t1_key = {
        let name: String = sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
            .bind(shared_group(&w.cp).await)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("the group's name");
        let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
            .bind(t1)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the tenant");
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client");
        let r = client
            .patch(w.cp.url(&format!("/admin/tenants/{t1}")))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::COOKIE, w.admin.cookie())
            .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
            .json(&json!({"allowed_provider_groups": [name]}))
            .send()
            .await
            .expect("send");
        assert_eq!(r.status().as_u16(), 200);
        let r =
            w.cp.send(
                "POST",
                &format!("/admin/tenants/{t1}/keys"),
                Some(w.admin.cookie()),
                Some(json!({"label": "t1-7c1", "group_id": shared_group(&w.cp).await})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        r.body["key"]["id"].as_str().expect("a key id").to_string()
    };
    sqlx::query(
        "INSERT INTO pricing_discount_revisions \
         (engine_version, tenant_id, effective_from, discount_bps) VALUES ($1, $2, now(), 10000)",
    )
    .bind(i64::from(ENGINE_VERSION))
    .bind(t1)
    .execute(w.cp.db().pool())
    .await
    .expect("write a full discount");

    let mut free_row = usage(&w, "e-free", model, tokens_680());
    free_row["team_id"] = json!(t0.to_string());
    free_row["api_key_id"] = json!(free_key);
    free_row["user_id"] = json!("");
    let mut discounted = usage(&w, "e-discounted", model, tokens_680());
    discounted["team_id"] = json!(t1.to_string());
    discounted["api_key_id"] = json!(t1_key);
    discounted["user_id"] = json!("");
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            free_row,
            discounted,
            usage(&w, "e-paying", model, tokens_680()),
        ],
    )
    .await;
    // One batch: the free row and the discounted one charge 0 and post
    // nothing, and the paying tenant's debit is still stored.
    assert_eq!((r.status, r.body.clone()), (200, counts(3, 0, 0)), "{r:?}");
    for (event, tenant) in [("e-free", t0), ("e-discounted", t1)] {
        let charge: Option<i64> = sqlx::query_scalar(
            "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = $1",
        )
        .bind(event)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read the charge");
        assert_eq!(charge, Some(0), "{event}");
        let entries: i64 =
            sqlx::query_scalar("SELECT count(*) FROM billing_ledger_cny WHERE tenant_id = $1")
                .bind(tenant)
                .fetch_one(w.cp.db().pool())
                .await
                .expect("count its entries");
        assert_eq!(entries, 0, "{event} posted an entry");
    }
    let paying: Option<i64> = sqlx::query_scalar(
        "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = 'e-paying'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read the paying charge");
    assert_eq!(paying, Some(4_896));
    let entries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM billing_ledger_cny WHERE tenant_id = $1")
            .bind(w.tenant)
            .fetch_one(w.cp.db().pool())
            .await
            .expect("count the paying tenant's entries");
    assert_eq!(entries, 1);
}

/// Whether a backend of this database is waiting for a lock while it reads
/// `table`.
async fn a_read_of_waits(cp: &TestCp, table: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock' \
           AND query ILIKE '%FROM ' || $1 || '%')",
    )
    .bind(table)
    .fetch_one(cp.db().pool())
    .await
    .expect("read pg_stat_activity")
}

/// R-001: intake loads the price book with one statement per table, each on
/// a snapshot of its own. Two price writes that commit between its read of
/// the baselines and its read of the group rates must not leave it pricing
/// with the old baseline and the new rate, a pair that was never in force
/// together: the book it prices with is one some version of the tables held.
///
/// The row occurs after both new revisions start (the 5 s backdate window a
/// write is allowed): at the old prices it is 680 µ¥ (card × rate 1), at the
/// new ones 2720 µ¥ (card × 2 at rate 2), and the mixed book would bill it
/// 1360 µ¥. A row before both new revisions is still priced at the old pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_book_loaded_while_prices_change_never_mixes_two_versions() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price(&w.cp, model).await;
    let group = support::world::group_id(&w.cp, support::world::SHARED_GROUP).await;
    // The revisions above start at the database's now; the new ones start a
    // moment later, and before the row occurs.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let new_from = db_now(&w.cp).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut early = usage(&w, "e-early", model, tokens_680());
    early["occurred_at"] = json!((new_from - TimeDelta::milliseconds(10)).to_rfc3339());
    let late = usage(&w, "e-late", model, tokens_680());

    // The group rates are held, so the load stops after reading the baselines.
    let mut holder = PgConnection::connect(w.cp.database_url())
        .await
        .expect("connect");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("LOCK TABLE pricing_group_rates IN ACCESS EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("lock the group rates");

    let delivered = deliver(&w.dp, Uuid::new_v4(), vec![early, late]);
    let race = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        while !a_read_of_waits(&w.cp, "pricing_group_rates").await {
            assert!(
                std::time::Instant::now() < deadline,
                "intake never waited on the group rates"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // A new baseline, card × 2, commits while intake waits...
        let doubled = json!({
            "default": {"base": {"input": "2", "output": "4", "cache_read": "0.2"}}
        });
        let mut tx = w.cp.db().pool().begin().await.expect("begin");
        sqlx::query(
            "INSERT INTO pricing_baselines \
             (engine_version, model_id, effective_from, card, source_kind, source) \
             VALUES ($1, $2, $3, $4, 'manual', '{\"note\": \"doubled\"}')",
        )
        .bind(i64::from(ENGINE_VERSION))
        .bind(model)
        .bind(new_from)
        .bind(doubled)
        .execute(&mut *tx)
        .await
        .expect("write the new baseline");
        sqlx::query(
            "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1) \
             WHERE key = 'pricing_version'",
        )
        .execute(&mut *tx)
        .await
        .expect("bump");
        tx.commit().await.expect("commit the baseline");
        // ...and then a new rate of 2, which releases the lock.
        sqlx::query(
            "INSERT INTO pricing_group_rates \
             (engine_version, group_id, model_id, effective_from, cny_per_usd_e4, note) \
             VALUES ($1, $2, NULL, $3, 20000, 'doubled')",
        )
        .bind(i64::from(ENGINE_VERSION))
        .bind(group)
        .bind(new_from)
        .execute(&mut holder)
        .await
        .expect("write the new rate");
        sqlx::query(
            "UPDATE cp_settings SET value = to_jsonb((value::text)::bigint + 1) \
             WHERE key = 'pricing_version'",
        )
        .execute(&mut holder)
        .await
        .expect("bump");
        sqlx::query("COMMIT")
            .execute(&mut holder)
            .await
            .expect("commit the rate");
    };
    let (r, ()) = tokio::join!(delivered, race);
    assert_eq!((r.status, r.body.clone()), (200, counts(2, 0, 0)), "{r:?}");
    let charge = |id: &'static str| {
        let pool = w.cp.db().pool().clone();
        async move {
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT charge_micro_cny FROM pricing_charges_cny WHERE event_id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read the charge")
        }
    };
    // Never the mixed 1360: the late row at the new pair.
    assert_eq!(charge("e-late").await, Some(2_720));
    // The book still holds the history: the early row at the old pair.
    assert_eq!(charge("e-early").await, Some(680));
}

/// A model document's `display_name` and `version`.
async fn model_doc(cp: &TestCp, id: Uuid) -> (String, i64) {
    sqlx::query_as(
        "SELECT doc->>'display_name', version FROM gw_resources WHERE kind = 'models' AND id = $1",
    )
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the model document")
}

/// Deletes model `id` as `admin`, which must answer 200.
async fn delete_model(cp: &TestCp, admin: &OperatorSession, id: Uuid) {
    let (_, version) = model_doc(cp, id).await;
    let status = send_if_match(
        cp,
        admin,
        "DELETE",
        &format!("/admin/resources/models/{id}"),
        version,
    )
    .await;
    assert_eq!(status, 200, "delete model {id}");
}

/// The detail of `status` for `code` on `model`.
fn detail<'a>(status: &'a Value, code: &str, model: Uuid) -> &'a Value {
    status["unpriced_details"]["items"]
        .as_array()
        .expect("details")
        .iter()
        .find(|d| d["code"] == code && d["model"]["id"] == json!(model.to_string()))
        .unwrap_or_else(|| panic!("no {code} detail for {model} in {status}"))
}

/// B3: 计价状态 says what each unpriced call was and what to fix, not just a
/// count per reason: each reason by model, with the tier the card lacks
/// (`missing_tier_price`) and the group without a rate (`missing_rate`), the
/// count and the latest call. A deleted model is still named. A charge an
/// applied re-pricing run has corrected no longer counts as unpriced: it is
/// counted apart, as corrected.
#[tokio::test]
async fn the_status_details_each_unpriced_kind_and_counts_corrected_rows_apart() {
    let Some(w) = world().await else { return };
    let cp = &w.cp;
    let (tiered, unpriced, fixed, unrated) = (
        seed_model(cp, &w.admin).await,
        seed_model(cp, &w.admin).await,
        seed_model(cp, &w.admin).await,
        seed_model(cp, &w.admin).await,
    );
    price(cp, tiered).await;
    // A group whose rate starts now, and a key of the world's tenant in it:
    // a call of an hour ago has a baseline in force and no rate.
    support::seed_group(cp, &w.admin, "nr-b3").await;
    let tenant = as_op(
        cp,
        &w.admin,
        "GET",
        &format!("/admin/tenants/{}", w.tenant),
        None,
    )
    .await;
    let patched = cp
        .send_conditional(
            "PATCH",
            &format!("/admin/tenants/{}", w.tenant),
            w.admin.cookie(),
            tenant.body["version"].as_i64().expect("a version"),
            json!({"allowed_provider_groups": ["shared", "nr-b3"]}),
        )
        .await;
    assert_eq!(patched.status, 200, "{patched:?}");
    let nr_key =
        support::world::seed_key_in_group(cp, &w.admin, w.tenant, Some(w.member), "nr-b3").await;
    let now = db_now(cp).await;
    support::world::insert_baseline_backdated(
        cp.db(),
        unrated,
        serde_json::to_value(card()).expect("a card"),
        now - TimeDelta::hours(2),
    )
    .await;

    let tier = |id: &str, ago: i64| {
        let mut e = usage(&w, id, tiered, tokens_680());
        e["service_tier"] = json!("priority");
        e["occurred_at"] = json!((Utc::now() - TimeDelta::milliseconds(ago)).to_rfc3339());
        e
    };
    let mut none = usage(&w, "e-none", unpriced, tokens_680());
    none["requested_model"] = json!("sent-none");
    let mut rate = usage(&w, "e-rate", unrated, tokens_680());
    rate["api_key_id"] = json!(nr_key);
    rate["occurred_at"] = json!((now - TimeDelta::hours(1)).to_rfc3339());
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            tier("e-tier-1", 10),
            tier("e-tier-2", 0),
            none,
            rate,
            usage(&w, "e-fix", fixed, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(5, 0, 0)), "{r:?}");

    // A run charges the one row of `fixed` once it has a price.
    price(cp, fixed).await;
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let to = db_now(cp).await;
    let run = as_op(
        cp,
        &w.admin,
        "POST",
        "/admin/billing/repricing",
        Some(json!({
            "from": to - TimeDelta::hours(1), "to": to, "model_id": fixed,
            "reason": "补录 b3",
        })),
    )
    .await;
    assert_eq!(
        (run.status, run.body["changed"].clone()),
        (201, json!(1)),
        "{run:?}"
    );
    let id = run.body["id"].as_str().expect("a run id");
    let applied = as_op(
        cp,
        &admin,
        "POST",
        &format!("/admin/billing/repricing/{id}/apply"),
        Some(json!({})),
    )
    .await;
    assert_eq!(applied.status, 200, "{applied:?}");

    let r = as_op(cp, &w.admin, "GET", "/admin/pricing/status", None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let s = &r.body;
    assert_eq!(
        (
            s["unpriced_last_24h"]["total"].clone(),
            s["unpriced_last_24h"]["by_code"].clone()
        ),
        (
            json!(4),
            json!({"missing_tier_price": 2, "missing_price": 1, "missing_rate": 1})
        ),
        "{s}"
    );
    assert_eq!(
        s["corrected_last_24h"],
        json!({"total": 1, "by_code": {"missing_price": 1}}),
        "{s}"
    );
    assert_eq!(s["unpriced_details"]["truncated"], false);
    assert_eq!(
        s["unpriced_details"]["items"].as_array().map(Vec::len),
        Some(3),
        "{s}"
    );

    let t = detail(s, "missing_tier_price", tiered);
    assert_eq!(t["count"], 2, "{t}");
    assert_eq!(t["service_tier"], "priority", "{t}");
    assert_eq!(
        t["model"]["name"],
        json!(model_doc(cp, tiered).await.0),
        "{t}"
    );
    assert_eq!(t["model"]["deleted"], false, "{t}");
    assert!(t["group"].is_null(), "{t}");
    let latest: DateTime<Utc> =
        serde_json::from_value(t["latest_at"].clone()).expect("a latest time");
    let first: DateTime<Utc> = sqlx::query_scalar(
        "SELECT occurred_at FROM pricing_charges_cny WHERE event_id = 'e-tier-1'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read the first tier row");
    assert!(latest > first, "the latest of the two: {t}");

    let g = detail(s, "missing_rate", unrated);
    assert_eq!(g["group"]["name"], "nr-b3", "{g}");
    assert!(g["service_tier"].is_null(), "{g}");

    let n = detail(s, "missing_price", unpriced);
    assert_eq!(n["count"], 1, "{n}");
    let name = model_doc(cp, unpriced).await.0;
    assert_eq!(n["model"]["name"], json!(name), "{n}");

    // Once the model is deleted it is still named, and marked deleted.
    delete_model(cp, &w.admin, unpriced).await;
    let r = as_op(cp, &w.admin, "GET", "/admin/pricing/status", None).await;
    let n = detail(&r.body, "missing_price", unpriced);
    assert_eq!(
        (n["model"]["name"].clone(), n["model"]["deleted"].clone()),
        (json!(name), json!(true)),
        "{n}"
    );
}

/// B4: an operator's charge listing says why a row was billed nothing and
/// names its model even once the model is gone: the call's status, error
/// class and attempt, the model's name from its document (or the name the
/// call sent), and a flag for a deleted model. Rows billed nothing can be
/// hidden, and the listing filters by model and reason.
#[tokio::test]
async fn a_charge_says_how_its_call_ended_and_names_a_deleted_model() {
    let Some(w) = world().await else { return };
    let cp = &w.cp;
    let model = seed_model(cp, &w.admin).await;
    price(cp, model).await;
    let mut retry = usage(&w, "e-retry", model, json!({}));
    for (k, v) in [
        ("status_code", json!(502)),
        ("error_class", json!("upstream_status")),
        ("error_message", json!("upstream answered 502")),
        ("attempt_kind", json!("retry")),
        ("attempt_index", json!(2)),
        ("is_final_attempt", json!(false)),
        ("requested_model", json!("gpt-5.6")),
    ] {
        retry[k] = v;
    }
    let mut refused = usage(&w, "e-refused", model, json!({}));
    refused["requested_model_id"] = json!("");
    refused["requested_model"] = json!("no-such-model");
    refused["status_code"] = json!(404);
    refused["error_class"] = json!("model_not_found");
    let mut anonymous = usage(&w, "e-anon", model, json!({}));
    anonymous["requested_model_id"] = json!("");
    anonymous["status_code"] = json!(401);
    anonymous["error_class"] = json!("authentication_error");
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            retry,
            refused,
            anonymous,
            usage(&w, "e-ok", model, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(4, 0, 0)), "{r:?}");
    let operator = &w.admin;
    let list = move |query: String| async move {
        let r = as_op(
            cp,
            operator,
            "GET",
            &format!("/admin/pricing/charges?{query}"),
            None,
        )
        .await;
        assert_eq!(r.status, 200, "{query}: {r:?}");
        r.body["items"].as_array().expect("items").clone()
    };
    let one = |items: Vec<Value>, id: &str| {
        items
            .into_iter()
            .find(|c| c["event_id"] == id)
            .unwrap_or_else(|| panic!("no {id}"))
    };
    let name = model_doc(cp, model).await.0;

    let all = list(format!("tenant_id={}", w.tenant)).await;
    let retried = one(all.clone(), "e-retry");
    assert_eq!(retried["status"], "nothing_to_bill", "{retried}");
    assert_eq!(
        (
            retried["status_code"].clone(),
            retried["error_class"].clone(),
            retried["attempt_kind"].clone(),
            retried["attempt_index"].clone(),
            retried["is_final_attempt"].clone(),
        ),
        (
            json!(502),
            json!("upstream_status"),
            json!("retry"),
            json!(2),
            json!(false)
        ),
        "{retried}"
    );
    assert_eq!(
        (
            retried["model_name"].clone(),
            retried["model_deleted"].clone()
        ),
        (json!(name), json!(false)),
        "{retried}"
    );
    // The upstream's own words stay in the usage row.
    assert!(retried.get("error_message").is_none(), "{retried}");
    let r = one(all.clone(), "e-refused");
    assert_eq!(
        (
            r["model_name"].clone(),
            r["model_deleted"].clone(),
            r["status_code"].clone()
        ),
        (json!("no-such-model"), json!(false), json!(404)),
        "{r}"
    );
    let r = one(all.clone(), "e-anon");
    assert_eq!(
        (r["model_name"].clone(), r["model_deleted"].clone()),
        (Value::Null, json!(false)),
        "{r}"
    );
    let ok = one(all, "e-ok");
    assert_eq!(
        (
            ok["attempt_kind"].clone(),
            ok["error_class"].clone(),
            ok["status_code"].clone()
        ),
        (json!("initial"), Value::Null, json!(200)),
        "{ok}"
    );

    // Hidden: every row billed nothing.
    let shown: Vec<Value> = list(format!(
        "tenant_id={}&exclude_status=nothing_to_bill",
        w.tenant
    ))
    .await
    .into_iter()
    .map(|c| c["event_id"].clone())
    .collect();
    assert_eq!(shown, [json!("e-ok")]);
    let by_model: Vec<Value> = list(format!("model_id={model}"))
        .await
        .into_iter()
        .map(|c| c["event_id"].clone())
        .collect();
    // Newest first: intake stores a batch in event-id order.
    assert_eq!(by_model, [json!("e-retry"), json!("e-ok")]);
    let r = as_op(
        cp,
        &w.admin,
        "GET",
        "/admin/pricing/charges?exclude_status=billed",
        None,
    )
    .await;
    assert_422_at(&r, "exclude_status");

    // The model is retired: its rows still name it, as deleted.
    delete_model(cp, &w.admin, model).await;
    let retried = one(list(format!("model_id={model}")).await, "e-retry");
    assert_eq!(
        (
            retried["model_name"].clone(),
            retried["model_deleted"].clone()
        ),
        (json!(name), json!(true)),
        "{retried}"
    );
}

/// The listing's `error_code` filter: one reason, or `unknown` for a charge
/// stored without one.
#[tokio::test]
async fn charges_are_listed_by_reason() {
    let Some(w) = world().await else { return };
    let (priced, unpriced) = (
        seed_model(&w.cp, &w.admin).await,
        seed_model(&w.cp, &w.admin).await,
    );
    price(&w.cp, priced).await;
    let r = deliver(
        &w.dp,
        Uuid::new_v4(),
        vec![
            usage(&w, "e-np", unpriced, tokens_680()),
            usage(
                &w,
                "e-search",
                priced,
                json!({"prompt_tokens": 100, "web_search_requests": 2}),
            ),
            usage(&w, "e-ok", priced, tokens_680()),
        ],
    )
    .await;
    assert_eq!((r.status, r.body.clone()), (200, counts(3, 0, 0)), "{r:?}");
    let ids = |query: &'static str| {
        let w = &w;
        async move {
            let r = as_op(
                &w.cp,
                &w.admin,
                "GET",
                &format!("/admin/pricing/charges?{query}"),
                None,
            )
            .await;
            assert_eq!(r.status, 200, "{r:?}");
            r.body["items"]
                .as_array()
                .expect("items")
                .iter()
                .map(|c| c["event_id"].as_str().expect("an id").to_string())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(ids("error_code=missing_price").await, ["e-np"]);
    assert_eq!(
        ids("error_code=missing_dimension_price").await,
        ["e-search"]
    );
    assert!(ids("error_code=unknown&status=unpriced").await.is_empty());
}
