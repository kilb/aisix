//! Cost rates: what an upstream account costs the platform, in CNY per USD
//! of a model's baseline, as an account default plus per-model overrides,
//! effective-dated like a group's rates (`/admin/pricing/cost-rates`,
//! `/admin/provider-keys/{id}/cost-rates`); and the cost each charge records
//! from them, in micro-CNY.
//!
//! Every expected figure is worked from the rates the test itself writes.
//! The worked example: a baseline of 10 USD per million input tokens and 30
//! per million output, 1M tokens of each, is 40 USD of list price; at a group
//! rate of 7 the tenant is charged ¥280, at an account cost rate of 0.3 the
//! row costs ¥12, and the platform keeps ¥268.

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::OperatorRole;
use aisix_control_plane::pricing::rates::{self, NewGroupRate};
use aisix_pricing::{CnyPerUsd, RateCard};
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{
    db_now, deliver, group_id, insert_baseline, public_model, seed_model, seed_provider_key, usage,
    world, World, SHARED_GROUP,
};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// 10 USD per million input tokens and 30 per million output.
fn worked_card() -> RateCard {
    serde_json::from_value(json!({"default": {"base": {"input": "10", "output": "30"}}}))
        .expect("a card")
}

/// 1M input and 1M output tokens.
fn million_each() -> Value {
    json!({"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000})
}

/// `shared`'s default rate of `cny_per_usd`, starting now.
async fn group_rate(cp: &TestCp, cny_per_usd: &str) {
    let row = NewGroupRate {
        group_id: group_id(cp, SHARED_GROUP).await,
        model_id: None,
        effective_from: db_now(cp).await,
        cny_per_usd: Some(CnyPerUsd::parse(cny_per_usd).expect("a rate")),
        note: "the cost-rate test's group rate".to_string(),
    };
    cp.db()
        .tx(move |c| async move { rates::insert_group_rate(c, &row, Actor::System).await }.boxed())
        .await
        .expect("the group rate");
}

/// A model priced at `card`, charged at a `shared` rate of 7.
async fn worked_model(w: &World, card: RateCard) -> Uuid {
    let model = seed_model(&w.cp, &w.admin).await;
    group_rate(&w.cp, "7").await;
    insert_baseline(&w.cp, model, db_now(&w.cp).await, card)
        .await
        .expect("the baseline");
    model
}

fn path(account: Uuid) -> String {
    format!("/admin/provider-keys/{account}/cost-rates")
}

/// `POST /admin/provider-keys/{account}/cost-rates` as `who`.
async fn post_rate(cp: &TestCp, who: &OperatorSession, account: Uuid, body: Value) -> Resp {
    cp.send("POST", &path(account), Some(who.cookie()), Some(body))
        .await
}

/// A cost rate written through the API, which must answer 201: the account's
/// default with no `model`, else that model's override (`None` ends it).
async fn set_rate(
    w: &World,
    account: Uuid,
    model: Option<Uuid>,
    rate: Option<&str>,
    from: DateTime<Utc>,
) -> Value {
    let r = post_rate(
        &w.cp,
        &w.admin,
        account,
        json!({"model_id": model, "cny_per_usd": rate, "effective_from": from}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body
}

/// `GET /admin/provider-keys/{account}/cost-rates` as `who`.
async fn detail(cp: &TestCp, who: &OperatorSession, account: Uuid) -> Resp {
    cp.send("GET", &path(account), Some(who.cookie()), None)
        .await
}

/// A row of 1M + 1M tokens on `model`, served through `account` when given.
fn row(w: &World, event_id: &str, model: Uuid, account: Option<Uuid>) -> Value {
    let mut event = usage(w, event_id, model, million_each());
    if let Some(account) = account {
        event["provider_key_id"] = json!(account);
    }
    event
}

async fn send(w: &World, events: Vec<Value>) {
    let r = deliver(&w.dp, Uuid::new_v4(), events).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// `(charge_micro_cny, cost_micro_cny)` of `event_id`'s charge.
async fn charge_and_cost(cp: &TestCp, event_id: &str) -> (Option<i64>, Option<i64>) {
    sqlx::query_as(
        "SELECT charge_micro_cny, cost_micro_cny FROM pricing_charges_cny WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("the charge of {event_id}: {e}"))
}

/// The `cny_per_usd` of a plan's current revision.
fn current(plan: &Value) -> Value {
    plan["current"]["cny_per_usd"].clone()
}

#[tokio::test]
async fn the_worked_example_costs_twelve_yuan_and_keeps_two_hundred_sixty_eight() {
    let Some(w) = world().await else { return };
    let model = worked_model(&w, worked_card()).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let revision = set_rate(&w, account, None, Some("0.3"), db_now(&w.cp).await).await;
    send(&w, vec![row(&w, "w1", model, Some(account))]).await;

    assert_eq!(
        charge_and_cost(&w.cp, "w1").await,
        (Some(280_000_000), Some(12_000_000))
    );
    let source: Value =
        sqlx::query_scalar("SELECT cost_source FROM pricing_charges_cny WHERE event_id = 'w1'")
            .fetch_one(w.cp.db().pool())
            .await
            .expect("read the cost source");
    assert_eq!(
        source,
        json!({"kind": "account_rate", "revision_id": revision["revision"]["revision_id"],
               "cny_per_usd": "0.3", "model_override": false})
    );

    let now = db_now(&w.cp).await;
    let r =
        w.cp.send(
            "GET",
            &format!(
                "/admin/pricing/margin?from={}&to={}&provider_key_id={account}",
                (now - TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                (now + TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let item = &r.body["items"][0];
    assert_eq!(item["charged_micro_cny"], 280_000_000, "{r:?}");
    assert_eq!(item["cost_micro_cny"], 12_000_000, "{r:?}");
    assert_eq!(item["profit_micro_cny"], 268_000_000, "{r:?}");
    assert_eq!(item["margin_pct"], "95.71", "{r:?}");
}

#[tokio::test]
async fn an_override_beats_the_accounts_default_until_it_ends() {
    let Some(w) = world().await else { return };
    let overridden = worked_model(&w, worked_card()).await;
    let other = worked_model(&w, worked_card()).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    set_rate(&w, account, None, Some("0.3"), db_now(&w.cp).await).await;
    set_rate(
        &w,
        account,
        Some(overridden),
        Some("0.2"),
        db_now(&w.cp).await,
    )
    .await;
    send(
        &w,
        vec![
            row(&w, "o1", overridden, Some(account)),
            row(&w, "d1", other, Some(account)),
        ],
    )
    .await;
    // 40 USD at 0.2 on the overridden model, at 0.3 on the other; both
    // charged the group's ¥280.
    assert_eq!(
        charge_and_cost(&w.cp, "o1").await,
        (Some(280_000_000), Some(8_000_000))
    );
    assert_eq!(
        charge_and_cost(&w.cp, "d1").await,
        (Some(280_000_000), Some(12_000_000))
    );

    // The account's detail says which applies to the model.
    let r = detail(&w.cp, &w.admin, account).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(current(&r.body["default"]), json!("0.3"), "{r:?}");
    let listed = r.body["models"].as_array().expect("models");
    let item = listed
        .iter()
        .find(|m| m["model_id"] == json!(overridden))
        .unwrap_or_else(|| panic!("the overridden model is listed: {r:?}"));
    assert_eq!(current(&item["rate"]), json!("0.2"), "{r:?}");

    // Ended, the default costs the model again.
    set_rate(&w, account, Some(overridden), None, db_now(&w.cp).await).await;
    send(&w, vec![row(&w, "o2", overridden, Some(account))]).await;
    assert_eq!(
        charge_and_cost(&w.cp, "o2").await,
        (Some(280_000_000), Some(12_000_000))
    );
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE action LIKE 'pricing.cost_rate.%' ORDER BY id",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("read the audit log");
    assert_eq!(
        audit,
        [
            "pricing.cost_rate.create",
            "pricing.cost_rate.create",
            "pricing.cost_rate.end_override"
        ]
    );
}

#[tokio::test]
async fn a_rate_effective_later_does_not_cost_a_row_before_it() {
    let Some(w) = world().await else { return };
    let model = worked_model(&w, worked_card()).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let now = db_now(&w.cp).await;
    set_rate(&w, account, None, Some("0.3"), now).await;
    let later = now + TimeDelta::minutes(1);
    let scheduled = set_rate(&w, account, None, Some("0.5"), later).await;
    assert_eq!(scheduled["warnings"], json!([]));

    // Occurring before the new rate starts: the old one. Occurring after
    // it (a gateway clock up to five minutes ahead is believed): the new.
    let before = row(&w, "b1", model, Some(account));
    let mut after = row(&w, "a1", model, Some(account));
    after["occurred_at"] = json!((later + TimeDelta::minutes(1)).to_rfc3339());
    send(&w, vec![before, after]).await;
    assert_eq!(
        charge_and_cost(&w.cp, "b1").await,
        (Some(280_000_000), Some(12_000_000))
    );
    assert_eq!(
        charge_and_cost(&w.cp, "a1").await,
        (Some(280_000_000), Some(20_000_000))
    );

    // The account's plan shows both: in force now, and scheduled.
    let r = detail(&w.cp, &w.admin, account).await;
    assert_eq!(current(&r.body["default"]), json!("0.3"), "{r:?}");
    assert_eq!(
        r.body["default"]["upcoming"][0]["cny_per_usd"],
        json!("0.5"),
        "{r:?}"
    );

    // A revision written now with a later one already scheduled is named
    // superseded by it.
    let r = post_rate(
        &w.cp,
        &w.admin,
        account,
        json!({"model_id": null, "cny_per_usd": "0.4", "effective_from": db_now(&w.cp).await}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["warnings"][0]["code"], "superseded_later", "{r:?}");
    assert_eq!(
        r.body["warnings"][0]["revision_id"], scheduled["revision"]["revision_id"],
        "{r:?}"
    );
}

#[tokio::test]
async fn without_a_rate_the_cost_is_unknown_never_zero() {
    let Some(w) = world().await else { return };
    let model = worked_model(&w, worked_card()).await;
    let other = worked_model(&w, worked_card()).await;
    let unrated = seed_provider_key(&w.cp, &w.admin).await;
    // An account whose only rate is another model's override.
    let elsewhere = seed_provider_key(&w.cp, &w.admin).await;
    set_rate(&w, elsewhere, Some(other), Some("0.3"), db_now(&w.cp).await).await;
    // An account that costs nothing: a cost of 0, which is known.
    let free = seed_provider_key(&w.cp, &w.admin).await;
    set_rate(&w, free, None, Some("0"), db_now(&w.cp).await).await;
    send(
        &w,
        vec![
            row(&w, "n1", model, Some(unrated)),
            row(&w, "n2", model, None),
            row(&w, "n3", model, Some(elsewhere)),
            row(&w, "z1", model, Some(free)),
        ],
    )
    .await;
    for event_id in ["n1", "n2", "n3"] {
        assert_eq!(
            charge_and_cost(&w.cp, event_id).await,
            (Some(280_000_000), None),
            "{event_id}"
        );
    }
    assert_eq!(
        charge_and_cost(&w.cp, "z1").await,
        (Some(280_000_000), Some(0))
    );
}

#[tokio::test]
async fn the_cost_is_rounded_up_once() {
    let Some(w) = world().await else { return };
    let account = seed_provider_key(&w.cp, &w.admin).await;
    set_rate(&w, account, None, Some("7.2"), db_now(&w.cp).await).await;
    // One token at 0.15 USD per million, at 7.2: 1.08 µ¥, so 2. Rounding the
    // USD first would give ⌈0.15⌉ = 1 µ$, then 8 µ¥.
    let fifteen = worked_model(
        &w,
        serde_json::from_value(json!({"default": {"base": {"input": "0.15", "output": "0.15"}}}))
            .expect("a card"),
    )
    .await;
    let mut one = usage(&w, "r1", fifteen, json!({"prompt_tokens": 1}));
    one["provider_key_id"] = json!(account);
    // A token in and a token out at 0.05 USD per million, at 7.2: 0.36 µ¥
    // each, 0.72 in all, so 1. Rounding each line would give 2.
    let five = worked_model(
        &w,
        serde_json::from_value(json!({"default": {"base": {"input": "0.05", "output": "0.05"}}}))
            .expect("a card"),
    )
    .await;
    let mut two = usage(
        &w,
        "r2",
        five,
        json!({"prompt_tokens": 1, "completion_tokens": 1}),
    );
    two["provider_key_id"] = json!(account);
    send(&w, vec![one, two]).await;
    assert_eq!(charge_and_cost(&w.cp, "r1").await.1, Some(2));
    assert_eq!(charge_and_cost(&w.cp, "r2").await.1, Some(1));
}

/// An account's models are the ones it serves: a public model ticked on
/// it, and a hand-written model naming it. A channel's generated direct
/// document is costed through its public model and takes no override.
#[tokio::test]
async fn an_accounts_detail_lists_the_models_it_serves() {
    let Some(w) = world().await else { return };
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let public = public_model(
        &w.cp,
        &w.admin,
        &format!("pub-cost-{}", Uuid::new_v4().simple()),
    )
    .await;
    let r =
        w.cp.send(
            "PUT",
            &format!("/admin/provider-keys/{account}/channel-models/{public}"),
            Some(w.admin.cookie()),
            Some(json!({"priority": 1, "weight": 1})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let direct: Uuid = sqlx::query_scalar(
        "SELECT direct_model_id FROM channel_models WHERE provider_key_id = $1 \
         AND public_model_id = $2",
    )
    .bind(account)
    .bind(public)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the direct document");
    let hand =
        w.cp.send(
            "POST",
            "/admin/resources/models",
            Some(w.admin.cookie()),
            Some(json!({"doc": {
                "display_name": format!("hand-{}", Uuid::new_v4().simple()),
                "provider": "openai",
                "model_name": "gpt-4o-mini",
                "provider_key_id": account,
            }})),
        )
        .await;
    assert_eq!(hand.status, 201, "{hand:?}");
    let hand = Uuid::parse_str(hand.body["id"].as_str().expect("an id")).expect("a uuid");
    // Another account's model is not this one's.
    let foreign = seed_model(&w.cp, &w.admin).await;

    let r = detail(&w.cp, &w.admin, account).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["account"]["id"], json!(account), "{r:?}");
    assert_eq!(r.body["default"]["current"], Value::Null, "{r:?}");
    let mut listed: Vec<Uuid> = r.body["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|m| Uuid::parse_str(m["model_id"].as_str().expect("an id")).expect("a uuid"))
        .collect();
    listed.sort();
    let mut want = vec![public, hand];
    want.sort();
    assert_eq!(listed, want, "{r:?}");
    assert!(!listed.contains(&foreign) && !listed.contains(&direct));

    // The direct document is refused, naming the public model to use.
    let r = post_rate(
        &w.cp,
        &w.admin,
        account,
        json!({"model_id": direct, "cny_per_usd": "0.3", "effective_from": db_now(&w.cp).await}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/model_id", "{r:?}");
    assert!(
        r.body["details"][0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("成本换算率")),
        "{r:?}"
    );
    // An override on another account's model is the operator's to write:
    // once written, the model is listed under this account too.
    set_rate(&w, account, Some(foreign), Some("0.3"), db_now(&w.cp).await).await;
    let r = detail(&w.cp, &w.admin, account).await;
    assert!(
        r.body["models"]
            .as_array()
            .expect("models")
            .iter()
            .any(|m| m["model_id"] == json!(foreign)),
        "{r:?}"
    );
}

#[tokio::test]
async fn the_account_list_shows_every_live_account_with_its_default() {
    let Some(w) = world().await else { return };
    let viewer = w.cp.admin_login_seeded(OperatorRole::Viewer).await;
    let rated = seed_provider_key(&w.cp, &w.admin).await;
    let unrated = seed_provider_key(&w.cp, &w.admin).await;
    set_rate(&w, rated, None, Some("0.35"), db_now(&w.cp).await).await;
    let r =
        w.cp.send(
            "GET",
            "/admin/pricing/cost-rates",
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    let of = |id: Uuid| {
        items
            .iter()
            .find(|i| i["account"]["id"] == json!(id))
            .unwrap_or_else(|| panic!("{id} is listed: {r:?}"))
            .clone()
    };
    assert_eq!(current(&of(rated)["default"]), json!("0.35"), "{r:?}");
    assert_eq!(of(unrated)["default"]["current"], Value::Null, "{r:?}");
    assert_eq!(of(rated)["account"]["provider"], "openai", "{r:?}");

    // A deleted account is listed no more, and takes no rate.
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM gw_resources WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(unrated)
    .fetch_one(w.cp.db().pool())
    .await
    .expect("the account's version");
    let deleted =
        w.cp.send_conditional(
            "DELETE",
            &format!("/admin/resources/provider_keys/{unrated}"),
            w.admin.cookie(),
            version,
            json!({}),
        )
        .await;
    assert_eq!(deleted.status, 200, "{deleted:?}");
    let r =
        w.cp.send(
            "GET",
            "/admin/pricing/cost-rates",
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert!(
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .all(|i| i["account"]["id"] != json!(unrated)),
        "{r:?}"
    );
    for account in [unrated, Uuid::new_v4()] {
        let r = post_rate(
            &w.cp,
            &w.admin,
            account,
            json!({"model_id": null, "cny_per_usd": "0.3", "effective_from": db_now(&w.cp).await}),
        )
        .await;
        assert_eq!(r.status, 404, "{r:?}");
        assert_eq!(detail(&w.cp, &w.admin, account).await.status, 404);
    }
    assert_eq!(detail(&w.cp, &w.admin, rated).await.status, 200);
    let r =
        w.cp.send(
            "GET",
            "/admin/provider-keys/nope/cost-rates",
            Some(w.admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
}

/// A cost rate is written with its `pricing_version` bump and its audit row
/// in one transaction, and never changes afterwards.
#[tokio::test]
async fn a_cost_rate_is_audited_and_append_only() {
    let Some(w) = world().await else { return };
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let version = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT (value::text)::bigint FROM cp_settings WHERE key = 'pricing_version'",
        )
        .fetch_one(w.cp.db().pool())
        .await
        .expect("read pricing_version")
    };
    let before = version().await;
    let written = set_rate(&w, account, None, Some("0.3"), db_now(&w.cp).await).await;
    assert_eq!(version().await, before + 1);
    assert_eq!(written["revision"]["created_by"]["id"], json!(w.admin.id));
    let (actor, target, detail): (Option<Uuid>, Option<String>, Value) = sqlx::query_as(
        "SELECT actor_id, target_id, detail FROM audit_log \
         WHERE action = 'pricing.cost_rate.create'",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("one audit row");
    assert_eq!(actor, Some(w.admin.id));
    assert_eq!(target, Some(written["revision"]["revision_id"].to_string()));
    assert_eq!(detail["provider_key_id"], json!(account));
    assert_eq!(detail["cny_per_usd_e4"], 3_000);

    for sql in [
        "UPDATE pricing_cost_rates SET cny_per_usd_e4 = 1",
        "DELETE FROM pricing_cost_rates",
        "TRUNCATE pricing_cost_rates",
    ] {
        let e = sqlx::query(sql)
            .execute(w.cp.db().pool())
            .await
            .expect_err(sql);
        assert!(e.to_string().contains("append-only"), "{sql}: {e}");
    }
    assert_eq!(w.cp.count("pricing_cost_rates").await, 1);
}
