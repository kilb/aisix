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
use support::world::{
    card, db_now, deliver, insert_baseline, seed_model, seed_provider_key, usage, world, World,
};
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
    // The first-boot group's display name (0030), not its routing name.
    assert_eq!(item["group"]["display_name"], "默认分组");
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
    // Both rows' money is reported: the second row was charged too, and
    // charging it is not made not to have happened by its cost being
    // unknown.
    assert_eq!(item["charged_micro_cny"], 14_400, "{item}");
    // But the rate divides the costed half by the cost the costed half
    // carries: 7200 µ¥ over 1000 µ$ is 7.2, and a rate diluted into 14.4
    // would not be a check of anything.
    assert_eq!(item["charged_with_cost_micro_cny"], 7_200, "{item}");
    assert_eq!(item["charged_without_cost_micro_cny"], 7_200, "{item}");
    assert_eq!(item["cost_micro_usd"], 1_000, "{item}");
    assert_eq!(item["realised_cny_per_usd"], REALISED_7_2, "{item}");
    assert_eq!(item["rows"], 1, "{item}");
    assert_eq!(item["rows_without_cost"], 1, "{item}");
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

/// An unpriced charge of `event_id` on `model`, written by raw SQL as intake
/// stores a row it cannot price: **no group**, no charge, no cost, and a
/// positive adjustment on it as a re-pricing run writes once the key has
/// taken a group (R11). The charge row is never updated, so its group column
/// stays NULL while the money is real.
async fn a_repriced_no_group_row(w: &World, event_id: &str, model: Uuid) -> i64 {
    let occurred_at = Utc::now();
    sqlx::query(
        "INSERT INTO usage_events (event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, \
                                   event) \
         VALUES ($1, $2, $2, $3, $4, $5, \
                 jsonb_build_object('event_id', $1, 'team_id', $4, 'api_key_id', $5, \
                   'requested_model_id', $6::text, 'operation', 'chat'))",
    )
    .bind(event_id)
    .bind(Uuid::new_v4())
    .bind(occurred_at)
    .bind(w.tenant.to_string())
    .bind(&w.key)
    .bind(model)
    .execute(w.cp.db().pool())
    .await
    .expect("insert the usage row");
    sqlx::query(
        "INSERT INTO pricing_charges_cny \
         (event_id, tenant_id, api_key_id, user_id, requested_model_id, occurred_at, status, \
          error_code, engine_version) \
         VALUES ($1, $2, $3, $4, $5::text, $6, 'unpriced', 'no_group', $7)",
    )
    .bind(event_id)
    .bind(w.tenant.to_string())
    .bind(&w.key)
    .bind(w.member.to_string())
    .bind(model)
    .bind(occurred_at)
    .bind(i64::from(aisix_pricing::ENGINE_VERSION))
    .execute(w.cp.db().pool())
    .await
    .expect("insert the unpriced charge");
    // What a run charged once the key's group was set: 7.2 for 1000 tokens at
    // the $1/M cost card, written as an adjustment because the charge row is
    // append-only.
    let delta = 7_200;
    adjustment(w, event_id, delta).await;
    delta
}

#[tokio::test]
async fn a_row_repriced_after_its_key_took_a_group_reports_under_that_group() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    let delta = a_repriced_no_group_row(&w, "ng1", model).await;

    // The charge row still has no group of its own; its key has `shared`, and
    // that is the group the run billed it at. It must be there, not absent.
    let group: Option<Uuid> = sqlx::query_scalar(
        "SELECT g.id FROM pricing_charges_cny c LEFT JOIN tenant_keys k \
           ON k.id::text = c.api_key_id LEFT JOIN provider_groups g \
           ON g.id = COALESCE(c.group_id, k.group_id) WHERE c.event_id = 'ng1'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("read the billed group");
    assert_eq!(
        group,
        Some(support::world::group_id(&w.cp, "shared").await),
        "the fixture did not build a no-group row"
    );

    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(uuid(&item, "/group/id"), group.expect("a group"), "{item}");
    assert_eq!(item["charged_micro_cny"], delta, "{item}");
    assert_eq!(item["cost_micro_usd"], 0, "{item}");
    assert_eq!(item["rows"], 0, "{item}");
    assert_eq!(item["rows_without_cost"], 1, "{item}");
    // No cost, so no rate: null, not 0 and not a division by nothing.
    assert_eq!(item["realised_cny_per_usd"], Value::Null, "{item}");

    // A costed charge on the same model in the same group, beside it: the
    // item now holds both populations, and the rate divides only the costed
    // one. Reading `charged_micro_cny` here would give 14.4 instead of 7.2.
    send(&w, vec![served(&w, "m1", model, tokens_1000())]).await;
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["charged_micro_cny"], 14_400, "{item}");
    assert_eq!(item["charged_with_cost_micro_cny"], 7_200, "{item}");
    assert_eq!(item["charged_without_cost_micro_cny"], delta, "{item}");
    assert_eq!(item["cost_micro_usd"], 1_000, "{item}");
    assert_eq!(item["realised_cny_per_usd"], REALISED_7_2, "{item}");
    assert_eq!(
        (item["rows"].as_i64(), item["rows_without_cost"].as_i64()),
        (Some(1), Some(1)),
        "{item}"
    );
}

#[tokio::test]
async fn a_row_whose_key_names_no_group_is_its_own_item_with_a_null_group() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    let delta = a_repriced_no_group_row(&w, "ng1", model).await;
    // A key that names no group (the one window R2 leaves open for a key an
    // older replica minted): the run bills it at nothing, so its money has
    // no group to be reported under. It must still be reported.
    sqlx::query("UPDATE tenant_keys SET group_id = NULL WHERE id = $1::uuid")
        .bind(&w.key)
        .execute(w.cp.db().pool())
        .await
        .expect("clear the key's group");

    let r = margin(&w.cp, &w.admin, "").await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{r:?}");
    let item = &items[0];
    assert_eq!(item["group"]["id"], Value::Null, "{item}");
    assert_eq!(item["group"]["display_name"], Value::Null, "{item}");
    assert_eq!(item["charged_micro_cny"], delta, "{item}");
}

#[tokio::test]
async fn the_margin_items_account_for_every_charged_row() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    // Three kinds of row in one range: a costed charge with an adjustment, a
    // costless charge, and a repriced row whose charge names no group.
    send(
        &w,
        vec![
            served(&w, "m1", model, tokens_1000()),
            usage(&w, "m2", model, tokens_1000()),
        ],
    )
    .await;
    adjustment(&w, "m1", 3_600).await;
    a_repriced_no_group_row(&w, "ng1", model).await;

    let (from, to) = (
        db_now(&w.cp).await - TimeDelta::hours(1),
        db_now(&w.cp).await + TimeDelta::hours(1),
    );
    // What the ledger says: every charge plus every adjustment to a charge in
    // the range.
    let total: i64 = sqlx::query_scalar(
        "SELECT (COALESCE(SUM(c.charge_micro_cny), 0)::bigint \
                 + COALESCE((SELECT SUM(pa.delta_micro_cny)::bigint \
                             FROM pricing_adjustments_cny pa \
                             JOIN pricing_charges_cny pc ON pc.event_id = pa.event_id \
                             WHERE pc.occurred_at >= $1 AND pc.occurred_at < $2), 0)::bigint) \
         FROM pricing_charges_cny c WHERE c.occurred_at >= $1 AND c.occurred_at < $2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("sum the charges");
    assert_eq!(
        total,
        7_200 + 7_200 + 3_600 + 7_200,
        "the fixture's own total"
    );

    let r = margin(&w.cp, &w.admin, "").await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    let summed: i64 = items
        .iter()
        .map(|i| i["charged_micro_cny"].as_i64().expect("an amount"))
        .sum();
    // Whatever the grouping, no charge and no adjustment is outside it.
    assert_eq!(summed, total, "items {r:?}");
    // And it is not one collapsed row: the model has a costed and a costless
    // row, and the repriced one carries no cost of its own.
    let costed: i64 = items
        .iter()
        .map(|i| i["rows"].as_i64().expect("rows"))
        .sum();
    let costless: i64 = items
        .iter()
        .map(|i| i["rows_without_cost"].as_i64().expect("rows"))
        .sum();
    assert_eq!((costed, costless), (1, 2), "items {r:?}");
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

#[tokio::test]
async fn a_deleted_model_is_named_null_not_empty() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    cost(&w.cp, "1").await;
    // A model that never carried a display name, so the two cases are told
    // apart by the row's presence rather than by its text.
    sqlx::query("UPDATE gw_resources SET doc = doc - 'display_name' WHERE id = $1")
        .bind(model)
        .execute(w.cp.db().pool())
        .await
        .expect("strip the display name");
    send(&w, vec![served(&w, "d1", model, tokens_1000())]).await;
    let item = one(&margin(&w.cp, &w.admin, "").await);
    // The row is live but unnamed: `""` says "no display name", which is
    // what the model's document says.
    assert_eq!(item["model"]["name"], "", "{item}");

    // Deleted, its row is a tombstone: `null` says "gone", which an empty
    // string cannot.
    sqlx::query("UPDATE gw_resources SET deleted = true WHERE id = $1")
        .bind(model)
        .execute(w.cp.db().pool())
        .await
        .expect("delete the model");
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["model"]["name"], Value::Null, "{item}");
    assert_eq!(item["charged_micro_cny"], 7_200, "{item}");
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
    // A group's net charge can be negative after adjustments that reverse
    // more than it charged. The sign is kept: it must not read as its own
    // magnitude.
    assert_eq!(realised(-7_200_000, 1_000_000).as_deref(), Some("-7.2000"));
    assert_eq!(realised(-1, 3).as_deref(), Some("-0.3333"));
    // A magnitude below one place rounds to zero and keeps no sign.
    assert_eq!(realised(-4, 1_000_000).as_deref(), Some("0.0000"));
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

/// A cost price of `input` USD per million input tokens for the upstream the
/// seeded models reach, when account `account` serves it, starting now.
async fn account_cost(cp: &TestCp, account: Uuid, input: &str) {
    let now = db_now(cp).await;
    let row = NewCostPrice {
        upstream_provider: UPSTREAM_PROVIDER.to_string(),
        upstream_model: UPSTREAM_MODEL.to_string(),
        provider_key_id: Some(account),
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

/// Account `id`'s display name, as its stored document holds it.
async fn account_name(cp: &TestCp, id: Uuid) -> String {
    sqlx::query_scalar(
        "SELECT doc->>'display_name' FROM gw_resources WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("the account's name")
}

/// Two accounts serve one model in one group: both charge the group's price,
/// and each costs its own. Filtered to one account, the margin is that
/// account's charge over that account's cost, and its items name it.
#[tokio::test]
async fn a_margin_filtered_by_account_names_only_that_accounts_cost() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let (a, b) = (
        seed_provider_key(&w.cp, &w.admin).await,
        seed_provider_key(&w.cp, &w.admin).await,
    );
    // A costs 1 USD per million input tokens, B costs 3.
    account_cost(&w.cp, a, "1").await;
    account_cost(&w.cp, b, "3").await;
    let via = |event_id: &str, account: Uuid| {
        let mut event = served(&w, event_id, model, tokens_1000());
        event["provider_key_id"] = json!(account);
        event
    };
    send(&w, vec![via("ma", a), via("mb", b)]).await;

    // 7200 µ¥ each. A's is over 1000 µ$ of cost, a realised 7.2; B's over
    // 3000 µ$, a realised 2.4.
    for (account, cost, rate) in [(a, 1_000, "7.2000"), (b, 3_000, "2.4000")] {
        let item = one(&margin(&w.cp, &w.admin, &format!("&provider_key_id={account}")).await);
        assert_eq!(uuid(&item, "/model/id"), model, "{item}");
        assert_eq!(item["charged_micro_cny"], 7_200, "{item}");
        assert_eq!(item["cost_micro_usd"], cost, "{item}");
        assert_eq!(item["realised_cny_per_usd"], rate, "{item}");
        assert_eq!(item["rows"], 1, "{item}");
        assert_eq!(item["provider_key_id"], json!(account), "{item}");
        assert_eq!(
            item["provider_key_display_name"],
            json!(account_name(&w.cp, account).await),
            "{item}"
        );
    }
    // Over every account: both rows, one item, and no account named.
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["charged_micro_cny"], 14_400, "{item}");
    assert_eq!(item["cost_micro_usd"], 4_000, "{item}");
    assert_eq!(item["realised_cny_per_usd"], "3.6000", "{item}");
    assert_eq!(item["provider_key_id"], Value::Null, "{item}");
    assert_eq!(item["provider_key_display_name"], Value::Null, "{item}");
    // An account that served nothing matches nothing, and a malformed id is
    // refused at its path.
    let none = margin(
        &w.cp,
        &w.admin,
        &format!("&provider_key_id={}", Uuid::new_v4()),
    )
    .await;
    assert_eq!(none.body["items"], json!([]), "{none:?}");
    let bad = margin(&w.cp, &w.admin, "&provider_key_id=nope").await;
    assert_eq!(bad.status, 422, "{bad:?}");
    assert_eq!(
        bad.body["details"][0]["path"], "/provider_key_id",
        "{bad:?}"
    );
}

#[tokio::test]
async fn the_margin_is_ordered_by_the_groups_display_name_then_the_model() {
    let Some(w) = world().await else { return };
    // Two groups whose creation order, routing names and ids all disagree
    // with the display names, and two models per group whose names and ids
    // disagree with their id order, so only the documented order passes.
    let suffix = Uuid::new_v4().simple();
    let zebra = group_named(&w.cp, &w.admin, &format!("aa-{suffix}"), "斑马").await;
    let alpha = group_named(&w.cp, &w.admin, &format!("zz-{suffix}"), "阿阿尔法").await;
    let b_model = seed_model(&w.cp, &w.admin).await;
    let a_model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, b_model).await;
    price_at_rate(&w.cp, a_model).await;
    rate(&w.cp, zebra, db_now(&w.cp).await, RATE).await;
    rate(&w.cp, alpha, db_now(&w.cp).await, RATE).await;
    cost(&w.cp, "1").await;
    // The tenant must be assigned both groups before a key may name either.
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(w.tenant)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read the version");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .request(
            reqwest::Method::PATCH,
            w.cp.url(&format!("/admin/tenants/{}", w.tenant)),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, w.admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_provider_groups": [
            group_name(&w.cp, zebra).await, group_name(&w.cp, alpha).await,
        ]}))
        .send()
        .await
        .expect("assign the groups");
    assert_eq!(r.status().as_u16(), 200, "assign the groups");
    // A charge is billed at its key's group, so each group needs a key of its
    // own for a row to land in it.
    let mut events = Vec::new();
    for (group, name) in [(zebra, "aa-"), (alpha, "zz-")] {
        let key = support::world::seed_key_in_group(
            &w.cp,
            &w.admin,
            w.tenant,
            None,
            &group_name(&w.cp, group).await,
        )
        .await;
        for (model, prefix) in [(b_model, "b"), (a_model, "a")] {
            let mut event = served(
                &w,
                &format!("{name}{prefix}-{suffix}"),
                model,
                tokens_1000(),
            );
            event["api_key_id"] = json!(key);
            events.push(event);
        }
    }
    send(&w, events).await;

    let r = margin(&w.cp, &w.admin, "").await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    // Display name first, then the group's id, then the model's id. Both
    // groups hold both models, so each group contributes two items ordered by
    // model id, and the groups are ordered by their display names as the
    // database compares them (not as they were created, and not by routing
    // name).
    let order: Vec<(String, String)> = items
        .iter()
        .map(|i| {
            (
                i["group"]["display_name"]
                    .as_str()
                    .expect("a name")
                    .to_string(),
                i["model"]["id"].as_str().expect("an id").to_string(),
            )
        })
        .collect();
    let (first, second) = if "斑马" < "阿阿尔法" {
        ("斑马", "阿阿尔法")
    } else {
        ("阿阿尔法", "斑马")
    };
    let mut expected: Vec<(String, String)> = Vec::new();
    for group in [first, second] {
        let mut models = vec![a_model, b_model];
        models.sort_unstable();
        for model in models {
            expected.push((group.to_string(), model.to_string()));
        }
    }
    assert_eq!(order, expected, "{r:?}");
}

/// The routing name of the group `id`, which a key is minted under.
async fn group_name(cp: &support::TestCp, id: Uuid) -> String {
    sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("the group's name")
}

/// A live group whose routing name is `name` and whose display name is
/// `display_name`, created as an operator; its id.
async fn group_named(
    cp: &support::TestCp,
    admin: &OperatorSession,
    name: &str,
    display_name: &str,
) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/provider-groups",
            Some(admin.cookie()),
            Some(json!({"name": name, "display_name": display_name,
                        "description": "排序", "default_cny_per_usd": "1"})),
        )
        .await;
    assert_eq!(r.status, 201, "create group {name}: {r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a group id")).expect("a uuid")
}

#[tokio::test]
async fn a_margin_range_is_bounded() {
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
