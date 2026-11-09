//! The operators' margin view: what each (group, model) charged in a period
//! against what the upstream cost, and the realised rate between them
//! (`GET /admin/pricing/margin`), plus the same rate on each row of the
//! charge listing.
//!
//! Cost is what the platform pays, in USD; a charge is what a tenant pays, in
//! CNY. The realised rate is CNY charged per USD of cost — no exchange rate
//! is stored, and none is applied here.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it. A
//! row's cost is known only when the row names the upstream that served it
//! (`upstream_provider`, `upstream_model`) and a cost price covers that pair;
//! a row that names neither is counted apart, never priced at a guessed cost.

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::pricing::margin::realised;
use aisix_control_plane::pricing::store::{self, NewCostPrice};
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{card, db_now, deliver, insert_baseline, seed_model, usage, world, World};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The upstream model a seeded model reaches, and the name a cost price must
/// match.
const UPSTREAM_MODEL: &str = "gpt-4o-mini";
const UPSTREAM_PROVIDER: &str = "openai";

/// The group rate the margin tests charge at: 7.2 CNY per USD.
const RATE: &str = "7.2";

/// [`RATE`] as the margin view shows it: four decimal places.
const REALISED_7_2: &str = "7.2000";

/// 1000 prompt tokens as the baseline prices them, at `card()`'s input of
/// 1 USD per million: 1000 µ$ of cost, and 1000 × 1 × 7.2 = 7200 µ¥ of
/// charge, so the realised rate is exactly 7.2.
fn tokens_1000() -> Value {
    json!({"prompt_tokens": 1000, "completion_tokens": 0})
}

/// `usage` with the channel the row was served by, which is what makes its
/// cost knowable.
fn served(w: &World, event_id: &str, model: Uuid, fields: Value) -> Value {
    let mut event = usage(w, event_id, model, fields);
    event["upstream_provider"] = json!(UPSTREAM_PROVIDER);
    event["upstream_model"] = json!(UPSTREAM_MODEL);
    event
}

/// A cost price of `input` USD per million input tokens for the upstream the
/// seeded models reach, starting now.
async fn cost(cp: &TestCp, input: &str) {
    let now = db_now(cp).await;
    let row = NewCostPrice {
        upstream_provider: UPSTREAM_PROVIDER.to_string(),
        upstream_model: UPSTREAM_MODEL.to_string(),
        provider_key_id: None,
        effective_from: now,
        card: serde_json::from_value(json!({
            "default": {"base": {"input": input, "output": "2"}}
        }))
        .expect("a cost card"),
        source: "manual".to_string(),
    };
    cp.db()
        .tx(move |c| async move { store::insert_cost_price(c, &row, Actor::System).await }.boxed())
        .await
        .expect("a cost price");
}

/// The world's baseline (`card()`) and a group rate of [`RATE`], both
/// starting now: a model priced here charges 7.2 µ¥ per µ$ the upstream
/// costs.
async fn price_at_rate(cp: &TestCp, model: Uuid) {
    let now = db_now(cp).await;
    let group = support::world::group_id(cp, support::world::SHARED_GROUP).await;
    rate(cp, group, now, RATE).await;
    insert_baseline(cp, model, now, card())
        .await
        .expect("the baseline");
}

/// A group rate of `cny_per_usd` for `group`'s default, starting at `from`,
/// written by the control plane's own path.
async fn rate(cp: &TestCp, group: Uuid, from: DateTime<Utc>, cny_per_usd: &str) {
    use aisix_control_plane::pricing::rates::{self, NewGroupRate};
    use aisix_pricing::CnyPerUsd;
    let row = NewGroupRate {
        group_id: group,
        model_id: None,
        effective_from: from,
        cny_per_usd: Some(CnyPerUsd::parse(cny_per_usd).expect("a rate")),
        note: "the margin test's rate".to_string(),
    };
    cp.db()
        .tx(move |c| {
            async move {
                rates::insert_group_rate(c, &row, Actor::System)
                    .await
                    .map(|(revision, _)| revision)
            }
            .boxed()
        })
        .await
        .expect("the group rate");
}

async fn send(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// `GET /admin/pricing/margin` over the last hour, as `who`.
async fn margin(cp: &TestCp, who: &OperatorSession, filter: &str) -> Resp {
    let now = db_now(cp).await;
    let from = now - TimeDelta::hours(1);
    let to = now + TimeDelta::hours(1);
    let path = format!(
        "/admin/pricing/margin?from={}&to={}{filter}",
        from.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        to.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    );
    cp.send("GET", &path, Some(who.cookie()), None).await
}

/// The one item of a margin answer.
fn one(r: &Resp) -> Value {
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "one item: {r:?}");
    items[0].clone()
}

fn uuid(v: &Value, pointer: &str) -> Uuid {
    Uuid::parse_str(
        v.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("a uuid at {pointer} in {v}")),
    )
    .expect("a uuid")
}

#[tokio::test]
async fn the_realised_rate_is_cny_charged_per_usd_of_cost() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    send(&w, vec![served(&w, "m1", model, tokens_1000())]).await;

    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(
        uuid(&item, "/group/id"),
        support::world::group_id(&w.cp, "shared").await
    );
    assert_eq!(item["group"]["display_name"], "shared");
    assert_eq!(uuid(&item, "/model/id"), model);
    assert_eq!(item["charged_micro_cny"], 7_200);
    assert_eq!(item["cost_micro_usd"], 1_000);
    // Four decimal places: the configured rate is 7.2, the realised one is
    // shown at the view's own scale.
    assert_eq!(item["realised_cny_per_usd"], "7.2000");
    assert_eq!(item["rows"], 1);
    assert_eq!(item["rows_without_cost"], 0);
}

#[tokio::test]
async fn rows_without_a_cost_are_counted_apart() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    send(
        &w,
        vec![
            served(&w, "m1", model, tokens_1000()),
            // Names no upstream, so nothing says what it cost. Its charge
            // still happened; its amount must not enter either side of the
            // rate.
            usage(&w, "m2", model, tokens_1000()),
        ],
    )
    .await;

    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["charged_micro_cny"], 7_200, "{item}");
    assert_eq!(item["cost_micro_usd"], 1_000, "{item}");
    assert_eq!(item["rows"], 1, "{item}");
    assert_eq!(item["rows_without_cost"], 1, "{item}");
    assert_eq!(item["realised_cny_per_usd"], "7.2000", "{item}");
}

#[tokio::test]
async fn an_adjustment_counts_in_the_realised_rate() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    send(&w, vec![served(&w, "m1", model, tokens_1000())]).await;
    assert_eq!(
        one(&margin(&w.cp, &w.admin, "").await)["charged_micro_cny"],
        7_200
    );

    // A re-pricing run corrected the charge up by half: 10 800 charged
    // against the same 1000 µ$ of cost is 10.8 CNY per USD.
    adjustment(&w, "m1", 3_600).await;
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["charged_micro_cny"], 10_800, "{item}");
    assert_eq!(item["cost_micro_usd"], 1_000, "{item}");
    assert_eq!(item["realised_cny_per_usd"], "10.8000", "{item}");
    assert_eq!(item["rows"], 1, "{item}");
}

/// An applied run's adjustment of `delta` to `event_id`'s charge, written by
/// raw SQL with the control plane's own operator, since a run's own preview
/// is what decides its amounts.
async fn adjustment(w: &World, event_id: &str, delta: i64) {
    let run = Uuid::new_v4();
    let by = w.admin.id;
    sqlx::query(
        "INSERT INTO billing_repricing_runs_cny \
         (id, status, filter, reason, pricing_version, engine_version, events, changed, \
          still_unpriced, skipped_unattributed, delta_micro_cny, by_tenant, created_by, \
          applied_by, applied_at) \
         VALUES ($1, 'applied', '{}', 'margin', 0, $2, 1, 1, 0, 0, $3, '[]', $4, $4, now())",
    )
    .bind(run)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .bind(delta)
    .bind(by)
    .execute(w.cp.db().pool())
    .await
    .expect("insert the run");
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny \
         (run_id, event_id, delta_micro_cny, engine_version, reason, created_by) \
         VALUES ($1, $2, $3, $4, 'margin', $5)",
    )
    .bind(run)
    .bind(event_id)
    .bind(delta)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .bind(by)
    .execute(w.cp.db().pool())
    .await
    .expect("insert the adjustment");
}

#[tokio::test]
async fn each_charge_shows_its_realised_rate() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    send(
        &w,
        vec![
            served(&w, "m1", model, tokens_1000()),
            usage(&w, "m2", model, tokens_1000()),
        ],
    )
    .await;

    let r =
        w.cp.send(
            "GET",
            &format!("/admin/pricing/charges?tenant_id={}", w.tenant),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    let row = |id: &str| {
        items
            .iter()
            .find(|c| c["event_id"] == json!(id))
            .unwrap_or_else(|| panic!("{id} in {r:?}"))
            .clone()
    };
    assert_eq!(row("m1")["realised_cny_per_usd"], REALISED_7_2, "{r:?}");
    // A charge whose cost is unknown has no rate: null, never a guess.
    assert_eq!(row("m2")["realised_cny_per_usd"], Value::Null, "{r:?}");
}

#[test]
fn the_realised_rate_rounds_half_up_to_four_places() {
    // 7.2 is exact at any scale; 7.20005 rounds to 7.2001, not 7.2000.
    assert_eq!(realised(7_200_000, 1_000_000).as_deref(), Some("7.2000"));
    assert_eq!(realised(7_200_050, 1_000_000).as_deref(), Some("7.2001"));
    assert_eq!(realised(7_200_049, 1_000_000).as_deref(), Some("7.2000"));
    // A third of a yuan per dollar, to four places.
    assert_eq!(realised(1, 3).as_deref(), Some("0.3333"));
    assert_eq!(realised(2, 3).as_deref(), Some("0.6667"));
    // No cost, or a cost of nothing, has no rate at all.
    assert_eq!(realised(5, 0), None);
    assert_eq!(realised(5, -1), None);
    // A charge of nothing against a real cost is 0, not null.
    assert_eq!(realised(0, 1_000).as_deref(), Some("0.0000"));
    // A cost of nothing against a real charge has no rate: cost 0 is the
    // unknown case, and a ratio against it is not a number.
    assert_eq!(realised(1_000, 0), None);
    // A rate above 10 000 is possible (a deeply discounted upstream), and
    // shown rather than clamped: 10⁶ µ¥ charged per 1 µ$ of cost is one
    // million CNY per dollar.
    assert_eq!(realised(1_000_000, 1).as_deref(), Some("1000000.0000"));
}

#[tokio::test]
async fn the_margin_filters_by_group_and_model() {
    let Some(w) = world().await else { return };
    let one_model = seed_model(&w.cp, &w.admin).await;
    let other = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, one_model).await;
    price_at_rate(&w.cp, other).await;
    cost(&w.cp, "1").await;
    send(
        &w,
        vec![
            served(&w, "m1", one_model, tokens_1000()),
            served(&w, "m2", other, tokens_1000()),
        ],
    )
    .await;
    assert_eq!(
        margin(&w.cp, &w.admin, "").await.body["items"]
            .as_array()
            .expect("items")
            .len(),
        2
    );

    let group = support::world::group_id(&w.cp, "shared").await;
    let filtered = one(&margin(&w.cp, &w.admin, &format!("&model_id={other}")).await);
    assert_eq!(uuid(&filtered, "/model/id"), other, "{filtered}");
    // Both charges are in `shared`, so the group filter keeps both and each
    // is still the group's.
    let both = margin(&w.cp, &w.admin, &format!("&group_id={group}")).await;
    assert_eq!(both.status, 200, "{both:?}");
    for item in both.body["items"].as_array().expect("items") {
        assert_eq!(uuid(item, "/group/id"), group, "{item}");
    }
    assert_eq!(both.body["items"].as_array().expect("items").len(), 2);
    // A group nothing charged in matches nothing.
    let none = margin(&w.cp, &w.admin, &format!("&group_id={}", Uuid::new_v4())).await;
    assert_eq!(none.body["items"], json!([]), "{none:?}");
    // A malformed id is refused at its path.
    let bad = margin(&w.cp, &w.admin, "&group_id=nope").await;
    assert_eq!(bad.status, 422, "{bad:?}");
}

#[tokio::test]
async fn a_margin_range_is_bounded_and_ordered() {
    let Some(w) = world().await else { return };
    // Longer than two years is refused, as every summary by month is.
    let now = db_now(&w.cp).await;
    let r =
        w.cp.send(
            "GET",
            &format!(
                "/admin/pricing/margin?from={}&to={}",
                (now - TimeDelta::days(800)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    // A missing bound is a query the route cannot read.
    let r =
        w.cp.send(
            "GET",
            "/admin/pricing/margin?to=2026-01-01T00:00:00Z",
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
}

#[tokio::test]
async fn the_margin_needs_a_viewer() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    assert_eq!(margin(&w.cp, &viewer, "").await.status, 200);
}
