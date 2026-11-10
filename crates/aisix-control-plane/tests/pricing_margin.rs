//! The operators' margin view: what each (group, model) charged in a period
//! against what its upstream accounts cost, profit and margin between them,
//! all in CNY (`GET /admin/pricing/margin`), plus the cost on each row of the
//! charge listing.
//!
//! A charge is a baseline times a group rate; a cost is the same baseline
//! lines times the serving account's cost rate. Both are micro-CNY, so
//! profit is a subtraction and the margin a percentage of the charge.
//!
//! Usage arrives over `POST /dp/telemetry` exactly as a gateway sends it. A
//! row's cost is known only when the row names the account that served it
//! (`provider_key_id`) and that account has a cost rate in force; a row that
//! names none is counted apart, never costed at a guess.
//!
//! Every expected figure is worked from the test's own rates: 1000 prompt
//! tokens at `card()`'s input of 1 USD per million are 1000 µ$ of baseline,
//! so 7200 µ¥ charged at a group rate of 7.2 and 300 µ¥ of cost at an
//! account rate of 0.3: 6900 µ¥ of profit, a margin of 95.83 %.

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::pricing::cost_rates::{self, NewCostRate};
use aisix_control_plane::pricing::margin::margin_pct;
use aisix_pricing::CnyPerUsd;
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{
    card, db_now, deliver, insert_baseline, seed_model, seed_provider_key, usage, world, World,
};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The group rate the margin tests charge at: 7.2 CNY per USD.
const RATE: &str = "7.2";

/// The cost rate of the account that serves the margin tests' rows.
const COST_RATE: &str = "0.3";

/// 7200 µ¥ charged against 300 µ¥ of cost: 6900 / 7200, to two places.
const MARGIN_95_83: &str = "95.83";

/// 1000 prompt tokens as the baseline prices them, at `card()`'s input of
/// 1 USD per million: 1000 µ$ of baseline, 7200 µ¥ of charge at 7.2 and
/// 300 µ¥ of cost at 0.3.
fn tokens_1000() -> Value {
    json!({"prompt_tokens": 1000, "completion_tokens": 0})
}

/// `usage` served through `account`, which is what makes its cost knowable.
fn served(w: &World, event_id: &str, model: Uuid, account: Uuid, fields: Value) -> Value {
    let mut event = usage(w, event_id, model, fields);
    event["provider_key_id"] = json!(account);
    event
}

/// `account`'s default cost rate of `cny_per_usd`, starting now, written by
/// the control plane's own path.
async fn account_rate(cp: &TestCp, account: Uuid, cny_per_usd: &str) {
    let now = db_now(cp).await;
    let row = NewCostRate {
        provider_key_id: account,
        model_id: None,
        effective_from: now,
        cny_per_usd: Some(CnyPerUsd::parse(cny_per_usd).expect("a rate")),
        note: "the margin test's cost rate".to_string(),
    };
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
        .expect("a cost rate");
}

/// An upstream account with a default cost rate of [`COST_RATE`].
async fn costed_account(w: &World) -> Uuid {
    let account = seed_provider_key(&w.cp, &w.admin).await;
    account_rate(&w.cp, account, COST_RATE).await;
    account
}

/// The world's baseline (`card()`) and a group rate of [`RATE`], both
/// starting now: a model priced here charges 7.2 µ¥ per µ$ of baseline.
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
async fn the_margin_is_the_charge_less_the_cost_in_cny() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let account = costed_account(&w).await;
    send(&w, vec![served(&w, "m1", model, account, tokens_1000())]).await;

    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(
        uuid(&item, "/group/id"),
        support::world::group_id(&w.cp, "shared").await
    );
    // The first-boot group's display name (0030), not its routing name.
    assert_eq!(item["group"]["display_name"], "通用分组");
    assert_eq!(uuid(&item, "/model/id"), model);
    assert_eq!(item["charged_micro_cny"], 7_200);
    assert_eq!(item["charged_with_cost_micro_cny"], 7_200);
    assert_eq!(item["cost_micro_cny"], 300);
    assert_eq!(item["profit_micro_cny"], 6_900);
    assert_eq!(item["margin_pct"], MARGIN_95_83);
    assert_eq!(item["rows"], 1);
    assert_eq!(item["rows_without_cost"], 0);
    // The USD figures are gone, not zeroed.
    assert_eq!(item.get("cost_micro_usd"), None, "{item}");
    assert_eq!(item.get("realised_cny_per_usd"), None, "{item}");
}

#[tokio::test]
async fn rows_without_a_cost_are_counted_apart() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let account = costed_account(&w).await;
    let uncosted = seed_provider_key(&w.cp, &w.admin).await;
    send(
        &w,
        vec![
            served(&w, "m1", model, account, tokens_1000()),
            // Names no account, so nothing says what it cost. Its charge
            // still happened; its amount must not enter the profit.
            usage(&w, "m2", model, tokens_1000()),
            // Served by an account with no cost rate: unknown too, not 0.
            served(&w, "m3", model, uncosted, tokens_1000()),
        ],
    )
    .await;

    let item = one(&margin(&w.cp, &w.admin, "").await);
    // Every row's money is reported: the uncosted rows were charged too,
    // and charging them is not made not to have happened by their cost
    // being unknown.
    assert_eq!(item["charged_micro_cny"], 21_600, "{item}");
    // But the profit sets the costed third against the cost it carries:
    // 7200 − 300. Counting the uncosted charge as profit would read 21 300.
    assert_eq!(item["charged_with_cost_micro_cny"], 7_200, "{item}");
    assert_eq!(item["charged_without_cost_micro_cny"], 14_400, "{item}");
    assert_eq!(item["cost_micro_cny"], 300, "{item}");
    assert_eq!(item["profit_micro_cny"], 6_900, "{item}");
    assert_eq!(item["margin_pct"], MARGIN_95_83, "{item}");
    assert_eq!(item["rows"], 1, "{item}");
    assert_eq!(item["rows_without_cost"], 2, "{item}");
    // And each uncosted charge stores no cost at all, rather than 0.
    let costs: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT cost_micro_cny FROM pricing_charges_cny WHERE event_id IN ('m2', 'm3') \
         ORDER BY event_id",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("read the costs");
    assert_eq!(costs, vec![None, None]);
}

/// A row that billed nothing, or that the response cache served, has no
/// charge and never will have a cost: it is neither costed nor counted as
/// unknown, so the unknown count is only what a cost rate could fix.
#[tokio::test]
async fn a_row_that_billed_nothing_is_not_an_unknown_cost() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let account = costed_account(&w).await;
    let mut hit = served(&w, "h1", model, account, tokens_1000());
    hit["cache_status"] = json!("hit");
    send(
        &w,
        vec![
            served(&w, "m1", model, account, tokens_1000()),
            hit,
            served(&w, "z1", model, account, json!({})),
        ],
    )
    .await;
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(
        (item["rows"].as_i64(), item["rows_without_cost"].as_i64()),
        (Some(1), Some(0)),
        "{item}"
    );
    assert_eq!(item["cost_micro_cny"], 300, "{item}");
}

#[tokio::test]
async fn an_adjustment_counts_in_the_profit() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let account = costed_account(&w).await;
    send(&w, vec![served(&w, "m1", model, account, tokens_1000())]).await;
    assert_eq!(
        one(&margin(&w.cp, &w.admin, "").await)["charged_micro_cny"],
        7_200
    );

    // A re-pricing run corrected the charge up by half: 10 800 charged
    // against the same 300 µ¥ of cost is 10 500 of profit, 97.22 %.
    adjustment(&w, "m1", 3_600).await;
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["charged_micro_cny"], 10_800, "{item}");
    assert_eq!(item["cost_micro_cny"], 300, "{item}");
    assert_eq!(item["profit_micro_cny"], 10_500, "{item}");
    assert_eq!(item["margin_pct"], "97.22", "{item}");
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
    // the $1/M baseline, written as an adjustment because the charge row is
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
    let account = costed_account(&w).await;
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
    assert_eq!(item["cost_micro_cny"], 0, "{item}");
    assert_eq!(item["rows"], 0, "{item}");
    assert_eq!(item["rows_without_cost"], 1, "{item}");
    // No known cost, so no profit to speak of and no margin: null, not a
    // division by nothing.
    assert_eq!(item["profit_micro_cny"], 0, "{item}");
    assert_eq!(item["margin_pct"], Value::Null, "{item}");

    // A costed charge on the same model in the same group, beside it: the
    // item now holds both populations, and the profit counts only the
    // costed one. Reading `charged_micro_cny` here would give 14 100.
    send(&w, vec![served(&w, "m1", model, account, tokens_1000())]).await;
    let item = one(&margin(&w.cp, &w.admin, "").await);
    assert_eq!(item["charged_micro_cny"], 14_400, "{item}");
    assert_eq!(item["charged_with_cost_micro_cny"], 7_200, "{item}");
    assert_eq!(item["charged_without_cost_micro_cny"], delta, "{item}");
    assert_eq!(item["cost_micro_cny"], 300, "{item}");
    assert_eq!(item["profit_micro_cny"], 6_900, "{item}");
    assert_eq!(item["margin_pct"], MARGIN_95_83, "{item}");
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
    let account = costed_account(&w).await;
    // Three kinds of row in one range: a costed charge with an adjustment, a
    // costless charge, and a repriced row whose charge names no group.
    send(
        &w,
        vec![
            served(&w, "m1", model, account, tokens_1000()),
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
async fn each_charge_shows_its_cost() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let account = costed_account(&w).await;
    send(
        &w,
        vec![
            served(&w, "m1", model, account, tokens_1000()),
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
    assert_eq!(row("m1")["cost_micro_cny"], 300, "{r:?}");
    assert_eq!(row("m1")["charge_micro_cny"], 7_200, "{r:?}");
    // A charge whose cost is unknown has none: null, never a guess.
    assert_eq!(row("m2")["cost_micro_cny"], Value::Null, "{r:?}");
    assert_eq!(row("m2")["charge_micro_cny"], 7_200, "{r:?}");
    assert_eq!(row("m1").get("realised_cny_per_usd"), None, "{r:?}");
}

#[tokio::test]
async fn a_deleted_model_is_named_null_not_empty() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let account = costed_account(&w).await;
    // A model that never carried a display name, so the two cases are told
    // apart by the row's presence rather than by its text.
    sqlx::query("UPDATE gw_resources SET doc = doc - 'display_name' WHERE id = $1")
        .bind(model)
        .execute(w.cp.db().pool())
        .await
        .expect("strip the display name");
    send(&w, vec![served(&w, "d1", model, account, tokens_1000())]).await;
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
fn the_margin_rounds_half_away_from_zero_to_two_places() {
    // The worked example: ¥268 of profit on ¥280 charged.
    assert_eq!(
        margin_pct(268_000_000, 280_000_000).as_deref(),
        Some("95.71")
    );
    assert_eq!(margin_pct(6_900, 7_200).as_deref(), Some("95.83"));
    // Exact figures keep their two places.
    assert_eq!(margin_pct(1, 2).as_deref(), Some("50.00"));
    assert_eq!(margin_pct(7, 7).as_deref(), Some("100.00"));
    assert_eq!(margin_pct(0, 7).as_deref(), Some("0.00"));
    // Half a hundredth of a percent rounds up; just under it, down.
    assert_eq!(margin_pct(1, 20_000).as_deref(), Some("0.01"));
    assert_eq!(margin_pct(1, 20_001).as_deref(), Some("0.00"));
    // A loss keeps its sign, and away from zero: a cost above the charge.
    assert_eq!(margin_pct(-1, 20_000).as_deref(), Some("-0.01"));
    assert_eq!(margin_pct(-12, 4).as_deref(), Some("-300.00"));
    // A magnitude below one place rounds to zero and keeps no sign.
    assert_eq!(margin_pct(-1, 20_001).as_deref(), Some("0.00"));
    // No charge, or a net negative one, has no percentage at all.
    assert_eq!(margin_pct(-5, 0), None);
    assert_eq!(margin_pct(5, -1), None);
}

#[tokio::test]
async fn the_margin_filters_by_group_and_model() {
    let Some(w) = world().await else { return };
    let one_model = seed_model(&w.cp, &w.admin).await;
    let other = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, one_model).await;
    price_at_rate(&w.cp, other).await;
    let account = costed_account(&w).await;
    send(
        &w,
        vec![
            served(&w, "m1", one_model, account, tokens_1000()),
            served(&w, "m2", other, account, tokens_1000()),
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
/// and each costs at its own rate. Filtered to one account, the margin is
/// that account's charge against that account's cost, and its items name it.
#[tokio::test]
async fn a_margin_filtered_by_account_names_only_that_accounts_cost() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    price_at_rate(&w.cp, model).await;
    let (a, b) = (
        seed_provider_key(&w.cp, &w.admin).await,
        seed_provider_key(&w.cp, &w.admin).await,
    );
    // A costs 0.1 CNY per USD of baseline, B costs 0.3.
    account_rate(&w.cp, a, "0.1").await;
    account_rate(&w.cp, b, "0.3").await;
    let via = |event_id: &str, account: Uuid| served(&w, event_id, model, account, tokens_1000());
    send(&w, vec![via("ma", a), via("mb", b)]).await;

    // 7200 µ¥ each. A's costs 100 µ¥, a margin of 7100 / 7200; B's costs
    // 300 µ¥, 6900 / 7200.
    for (account, cost, profit, pct) in [(a, 100, 7_100, "98.61"), (b, 300, 6_900, "95.83")] {
        let item = one(&margin(&w.cp, &w.admin, &format!("&provider_key_id={account}")).await);
        assert_eq!(uuid(&item, "/model/id"), model, "{item}");
        assert_eq!(item["charged_micro_cny"], 7_200, "{item}");
        assert_eq!(item["cost_micro_cny"], cost, "{item}");
        assert_eq!(item["profit_micro_cny"], profit, "{item}");
        assert_eq!(item["margin_pct"], pct, "{item}");
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
    assert_eq!(item["cost_micro_cny"], 400, "{item}");
    assert_eq!(item["profit_micro_cny"], 14_000, "{item}");
    assert_eq!(item["margin_pct"], "97.22", "{item}");
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
    let account = costed_account(&w).await;
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
                account,
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
