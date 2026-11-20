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

async fn admin_get(w: &World, path: &str) -> Value {
    let r = w.cp.send("GET", path, Some(w.admin.cookie()), None).await;
    assert_eq!(r.status, 200, "{path}: {r:?}");
    r.body
}

async fn cost_backfill(w: &World, account: Uuid, who: &OperatorSession, body: Value) -> Resp {
    w.cp.send(
        "POST",
        &format!("/admin/provider-keys/{account}/cost-backfill"),
        Some(who.cookie()),
        Some(body),
    )
    .await
}

fn event<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["items"]
        .as_array()
        .expect("计费记录")
        .iter()
        .find(|item| item["event_id"] == id)
        .unwrap_or_else(|| panic!("缺少计费记录 {id}: {list}"))
}

#[tokio::test]
async fn an_unacknowledged_first_page_replays_at_the_same_rate_and_counts_its_own_writes() {
    let Some(mut w) = world().await else { return };
    w.admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let model = worked_model(&w, worked_card()).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    send(
        &w,
        (0..257)
            .map(|i| row(&w, &format!("replay-{i:03}"), model, Some(account)))
            .collect(),
    )
    .await;
    set_rate(&w, account, None, Some("0.2"), db_now(&w.cp).await).await;
    let changes_at = db_now(&w.cp).await + TimeDelta::seconds(6);
    set_rate(&w, account, None, Some("0.9"), changes_at).await;
    let prepared = cost_backfill(&w, account, &w.admin, json!({"prepare": true})).await;
    assert_eq!(prepared.status, 200, "{prepared:?}");
    assert_eq!(prepared.body["examined"], 0);
    assert_eq!(w.cp.count("pricing_cost_backfills").await, 0);
    let token = prepared.body["next_after"]
        .as_str()
        .expect("首次实际写入前已有游标");
    let page = json!({"after": token});
    // 两个客户端并发重传首批，即使实际只有一次写入，两份回执都能确认本次操作的 256 笔。
    let (first, concurrent) = tokio::join!(
        cost_backfill(&w, account, &w.admin, page.clone()),
        cost_backfill(&w, account, &w.admin, page.clone())
    );
    for response in [first, concurrent] {
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(response.body["filled"], 256);
    }
    assert_eq!(w.cp.count("pricing_cost_backfills").await, 256);
    let wait = changes_at - db_now(&w.cp).await;
    if let Ok(duration) = wait.to_std() {
        tokio::time::sleep(duration + std::time::Duration::from_millis(20)).await;
    }
    assert!(db_now(&w.cp).await >= changes_at);
    // 客户端未保存首批回执，此时费率已变化，仍只能用原游标重放。
    let replay = cost_backfill(&w, account, &w.admin, page).await;
    assert_eq!(replay.status, 200, "{replay:?}");
    assert_eq!(replay.body["examined"], 256);
    assert_eq!(replay.body["filled"], 256);
    let token = replay.body["next_after"].as_str().expect("第 257 笔");
    let last = cost_backfill(&w, account, &w.admin, json!({"after":token})).await;
    assert_eq!(
        last.body,
        json!({"examined":1, "filled":1, "next_after":null})
    );
    let amounts: (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), min(cost_micro_cny), max(cost_micro_cny) FROM pricing_cost_backfills",
    )
    .fetch_one(w.cp.db().pool())
    .await
    .expect("补填金额");
    assert_eq!(amounts, (257, 8_000_000, 8_000_000));
}

#[tokio::test]
async fn cost_backfill_uses_the_first_positive_adjustment_even_after_a_second_repricing() {
    let Some(mut w) = world().await else { return };
    w.admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let model = seed_model(&w.cp, &w.admin).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    group_rate(&w.cp, "7").await;
    let legacy = row(&w, "legacy-first", model, Some(account));
    send(&w, vec![row(&w, "corrected", model, Some(account))]).await;
    let charges = format!("/admin/pricing/charges?model_id={model}");
    let unpriced = admin_get(&w, &charges).await;
    assert_eq!(event(&unpriced, "corrected")["error_code"], "missing_price");

    let from = db_now(&w.cp).await - TimeDelta::hours(1);
    let reprice = async |expected_delta: i64| {
        let preview =
            w.cp.send(
                "POST",
                "/admin/billing/repricing",
                Some(w.admin.cookie()),
                Some(
                    json!({"from": from, "to": db_now(&w.cp).await, "model_id": model,
                        "reason": "补录模型基准价，核对历史成本"}),
                ),
            )
            .await;
        assert_eq!(preview.status, 201, "{preview:?}");
        assert_eq!(
            preview.body["delta_micro_cny"], expected_delta,
            "{preview:?}"
        );
        let id = preview.body["id"].as_str().expect("重定价标识");
        let applied =
            w.cp.send(
                "POST",
                &format!("/admin/billing/repricing/{id}/apply"),
                Some(w.admin.cookie()),
                Some(json!({})),
            )
            .await;
        assert_eq!(applied.status, 200, "{applied:?}");
        Uuid::parse_str(id).expect("重定价 UUID")
    };
    // 调用首次实际收费时的基准是 10 + 30 = 40 美元，租户费率 7。
    insert_baseline(&w.cp, model, db_now(&w.cp).await, worked_card())
        .await
        .expect("首笔正额调整使用的基准");
    let first = reprice(280_000_000).await;

    // 旧版本的首笔正额调整没有保存行。即使后续调整保存明细，原始成本仍应未知。
    send(&w, vec![legacy]).await;
    let delayed = admin_get(&w, &charges).await;
    assert_eq!(
        event(&delayed, "legacy-first")["error_code"],
        "missing_price"
    );
    sqlx::query(
        "INSERT INTO pricing_adjustments_cny
           (run_id, event_id, delta_micro_cny, baseline_revision_id, rate_revision_id,
            engine_version, reason, created_by)
         SELECT run_id, 'legacy-first', delta_micro_cny, baseline_revision_id, rate_revision_id,
                engine_version, reason, created_by
         FROM pricing_adjustments_cny WHERE run_id = $1 AND event_id = 'corrected'",
    )
    .bind(first)
    .execute(w.cp.db().pool())
    .await
    .expect("无行明细的旧版调整");

    // 第二次把两笔调用各上调到 ¥560，与第一次基准及稍后生效的账户费率可区分。
    let doubled = serde_json::from_value(json!({"default":{"base":{"input":"20","output":"60"}}}))
        .expect("第二次基准");
    insert_baseline(&w.cp, model, db_now(&w.cp).await, doubled)
        .await
        .expect("提高模型基准");
    reprice(560_000_000).await;
    set_rate(&w, account, None, Some("0.3"), db_now(&w.cp).await).await;
    let before = admin_get(&w, &charges).await;
    for id in ["corrected", "legacy-first"] {
        assert_eq!(event(&before, id)["adjusted_micro_cny"], 560_000_000);
        assert_eq!(event(&before, id).get("cost_micro_cny"), Some(&Value::Null));
    }
    let ledger = format!("/admin/tenants/{}/billing/ledger", w.tenant);
    let before_ledger = admin_get(&w, &ledger).await;
    let fill = cost_backfill(&w, account, &w.admin, json!({})).await;
    assert_eq!(fill.status, 200, "{fill:?}");
    assert_eq!(fill.body.get("filled"), Some(&json!(1)), "{fill:?}");
    let after = admin_get(&w, &charges).await;
    assert_eq!(
        event(&after, "corrected").get("cost_micro_cny"),
        Some(&json!(12_000_000))
    );
    assert_eq!(
        event(&after, "legacy-first").get("cost_micro_cny"),
        Some(&Value::Null)
    );
    assert_eq!(
        event(&after, "corrected")["adjusted_micro_cny"],
        560_000_000
    );
    assert_eq!(admin_get(&w, &ledger).await, before_ledger);
}

#[tokio::test]
async fn explicit_cost_backfill_uses_original_lines_and_only_changes_missing_account_costs() {
    let Some(mut w) = world().await else { return };
    w.admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let model = worked_model(&w, worked_card()).await;
    let other_model = worked_model(&w, worked_card()).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let other_account = seed_provider_key(&w.cp, &w.admin).await;
    let discount = w.cp.send(
        "POST", "/admin/pricing/discounts", Some(w.admin.cookie()),
        Some(json!({"tenant_id": w.tenant, "effective_from": db_now(&w.cp).await, "discount_bps": 5_000})),
    ).await;
    assert_eq!(discount.status, 201, "{discount:?}");
    // 每次调用按原始美元基准计 40 美元，租户费率 7、五折，因此每笔实收 140 元。
    send(
        &w,
        vec![
            row(&w, "override", model, Some(account)),
            row(&w, "default", other_model, Some(account)),
            row(&w, "foreign", model, Some(other_account)),
            row(&w, "unattributed", model, None),
        ],
    )
    .await;
    // 已经确定的 0 成本不能按新费率重写。
    set_rate(&w, account, Some(model), Some("0"), db_now(&w.cp).await).await;
    send(&w, vec![row(&w, "known-zero", model, Some(account))]).await;
    let active = set_rate(&w, account, Some(model), Some("0.2"), db_now(&w.cp).await).await;
    set_rate(&w, account, None, Some("0.3"), db_now(&w.cp).await).await;
    set_rate(
        &w,
        account,
        Some(model),
        Some("9"),
        db_now(&w.cp).await + TimeDelta::days(1),
    )
    .await;
    insert_baseline(
        &w.cp,
        model,
        db_now(&w.cp).await,
        serde_json::from_value(json!({"default":{"base":{"input":"100","output":"300"}}}))
            .expect("新基准"),
    )
    .await
    .expect("调高现行基准");

    let charges = format!("/admin/pricing/charges?tenant_id={}", w.tenant);
    let before = admin_get(&w, &charges).await;
    assert_eq!(
        event(&before, "override").get("cost_micro_cny"),
        Some(&Value::Null)
    );
    assert_eq!(
        event(&before, "known-zero").get("cost_micro_cny"),
        Some(&json!(0))
    );
    let ledger = format!("/admin/tenants/{}/billing/ledger", w.tenant);
    let before_ledger = admin_get(&w, &ledger).await;
    assert_eq!(before_ledger["items"].as_array().expect("流水").len(), 5);
    let billing = format!("/admin/tenants/{}/billing", w.tenant);
    assert_eq!(
        admin_get(&w, &billing).await["balance_micro_cny"],
        -700_000_000
    );

    for role in [OperatorRole::Viewer, OperatorRole::Operator] {
        let who = w.cp.admin_login_seeded(role).await;
        assert_eq!(
            cost_backfill(&w, account, &who, json!({})).await.status,
            403
        );
    }
    assert_eq!(admin_get(&w, &charges).await, before);
    let (a, b) = tokio::join!(
        cost_backfill(&w, account, &w.admin, json!({})),
        cost_backfill(&w, account, &w.admin, json!({})),
    );
    assert_eq!(a.status, 200, "{a:?}");
    assert_eq!(b.status, 200, "{b:?}");
    assert_eq!(
        a.body["filled"].as_u64().expect("新增笔数") + b.body["filled"].as_u64().expect("新增笔数"),
        2
    );
    for answer in [a, b] {
        assert_eq!(answer.body.get("next_after"), Some(&Value::Null));
    }

    let after = admin_get(&w, &charges).await;
    for (id, expected_cost) in [
        ("override", json!(8_000_000)),
        ("default", json!(12_000_000)),
        ("known-zero", json!(0)),
        ("foreign", Value::Null),
        ("unattributed", Value::Null),
    ] {
        let charge = event(&after, id);
        assert_eq!(
            charge.get("cost_micro_cny"),
            Some(&expected_cost),
            "{charge}"
        );
        assert_eq!(charge["charge_micro_cny"], 140_000_000, "{charge}");
    }
    let revision = active["revision"]["revision_id"]
        .as_i64()
        .expect("费率版本");
    assert_eq!(
        event(&after, "override")["cost_source"],
        json!({"kind": "account_rate", "revision_id": revision, "cny_per_usd": "0.2",
               "model_override": true, "backfilled": true})
    );
    let attribution: (Uuid, Uuid, i64) = sqlx::query_as(
        "SELECT created_by, provider_key_id, rate_revision_id FROM pricing_cost_backfills WHERE event_id = 'override'",
    )
    .fetch_one(w.cp.db().pool()).await.expect("操作员补填记录");
    assert_eq!(attribution, (w.admin.id, account, revision));
    assert_eq!(
        charge_and_cost(&w.cp, "override").await,
        (Some(140_000_000), None)
    );
    assert_eq!(
        event(&after, "known-zero")["cost_source"],
        event(&before, "known-zero")["cost_source"]
    );

    let grouped = admin_get(
        &w,
        &format!("{charges}&group_requests=true&event_id=override"),
    )
    .await;
    assert_eq!(
        event(&grouped, "override")["request_group"]["cost_micro_cny"],
        "8000000"
    );
    let now = db_now(&w.cp).await;
    let margin = admin_get(
        &w,
        &format!(
            "/admin/pricing/margin?from={}&to={}&provider_key_id={account}&model_id={model}",
            (now - TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            (now + TimeDelta::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
    )
    .await;
    assert_eq!(margin["items"][0]["cost_micro_cny"], 8_000_000, "{margin}");
    assert_eq!(
        margin["items"][0]["profit_micro_cny"], 272_000_000,
        "{margin}"
    );
    let totals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let reply =
                w.cp.send(
                    "GET",
                    "/admin/billing/profit-summary",
                    Some(w.admin.cookie()),
                    None,
                )
                .await;
            if reply.status != 503 || reply.body["code"] != "profit_summary_catching_up" {
                assert_eq!(reply.status, 200, "{reply:?}");
                break reply.body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("等待利润日报完成投影");
    assert_eq!(
        totals["totals"]["known_cost_micro_cny"], "20000000",
        "{totals}"
    );
    assert_eq!(totals["totals"]["rows_with_cost"], "3", "{totals}");
    assert_eq!(totals["totals"]["rows_without_cost"], "2", "{totals}");
    let accounts = totals["accounts"].as_array().expect("按上游账号汇算");
    assert_eq!(accounts.len(), 2, "没有归属账号的用量只计入全站总计");
    assert_eq!(accounts[0]["provider_key_id"], account.to_string());
    assert_eq!(accounts[0]["totals"]["charged_micro_cny"], "420000000");
    assert_eq!(accounts[0]["totals"]["known_cost_micro_cny"], "20000000");
    assert_eq!(accounts[0]["totals"]["rows_with_cost"], "3");
    assert_eq!(accounts[1]["provider_key_id"], other_account.to_string());
    assert_eq!(accounts[1]["totals"]["charged_micro_cny"], "140000000");
    assert_eq!(accounts[1]["totals"]["rows_without_cost"], "1");

    let retried = cost_backfill(&w, account, &w.admin, json!({})).await;
    assert_eq!(retried.status, 200, "{retried:?}");
    assert_eq!(retried.body.get("filled"), Some(&json!(0)));
    let changes: Vec<(Uuid, String, Value)> = sqlx::query_as(
        "SELECT actor_id, target_id, detail FROM audit_log WHERE action = 'pricing.cost_backfill'",
    )
    .fetch_all(w.cp.db().pool())
    .await
    .expect("补填审计");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].0, w.admin.id);
    assert_eq!(changes[0].1, account.to_string());
    assert_eq!(changes[0].2["filled"], 2);
    assert_eq!(admin_get(&w, &ledger).await, before_ledger);
    assert_eq!(
        admin_get(&w, &billing).await["balance_micro_cny"],
        -700_000_000
    );
}

#[tokio::test]
async fn explicit_cost_backfill_pages_past_unknown_rows_and_refuses_a_superseded_rate() {
    let Some(mut w) = world().await else { return };
    w.admin = w.cp.admin_login_seeded(OperatorRole::Admin).await;
    let model = worked_model(&w, worked_card()).await;
    let another_model = worked_model(&w, worked_card()).await;
    let account = seed_provider_key(&w.cp, &w.admin).await;
    let mut batch: Vec<_> = (0..256)
        .map(|i| row(&w, &format!("earlier-{i:03}"), model, Some(account)))
        .collect();
    batch.push(row(&w, "last", another_model, Some(account)));
    let delayed = row(&w, "delayed", another_model, Some(account));
    send(&w, batch).await;
    // 当前版本已不再生成美元成本。按 0032 迁移前的格式插入历史行，保留其真实收费明细。
    sqlx::query(
        "WITH old_event AS (
            INSERT INTO usage_events(event_id, dp_id, batch_id, occurred_at, team_id, api_key_id, event)
            SELECT 'legacy-usd', dp_id, batch_id, occurred_at, team_id, api_key_id,
                   event || jsonb_build_object('event_id', 'legacy-usd')
              FROM usage_events WHERE event_id = 'last' RETURNING event_id
         )
         INSERT INTO pricing_charges_cny(event_id, tenant_id, api_key_id, user_id, requested_model_id,
             occurred_at, price_instant, status, charge_micro_cny, cost_micro_usd, group_id,
             baseline_revision_id, rate_revision_id, lines, engine_version)
         SELECT old_event.event_id, c.tenant_id, c.api_key_id, c.user_id, c.requested_model_id,
                c.occurred_at, c.price_instant, c.status, c.charge_micro_cny, 40000, c.group_id,
                c.baseline_revision_id, c.rate_revision_id, c.lines, c.engine_version
           FROM old_event, pricing_charges_cny c WHERE c.event_id = 'last'",
    ).execute(w.cp.db().pool()).await.expect("迁移前记录");
    set_rate(
        &w,
        account,
        None,
        Some("2"),
        db_now(&w.cp).await + TimeDelta::days(1),
    )
    .await;
    let first = cost_backfill(&w, account, &w.admin, json!({})).await;
    assert_eq!(first.status, 200, "{first:?}");
    assert_eq!(first.body["examined"], 256);
    assert_eq!(first.body["filled"], 0);
    let cursor = first.body["next_after"].as_str().expect("还有一页");
    // 两页之间费率被修改时，操作员需从头开始，不混用两次费率。
    set_rate(
        &w,
        account,
        Some(another_model),
        Some("0"),
        db_now(&w.cp).await,
    )
    .await;
    let stale = cost_backfill(&w, account, &w.admin, json!({"after": cursor})).await;
    assert_eq!(stale.status, 409, "{stale:?}");
    assert_eq!(stale.body["code"], "cost_backfill_stale");
    let last_event = "/admin/pricing/charges?event_id=last";
    assert_eq!(
        event(&admin_get(&w, last_event).await, "last").get("cost_micro_cny"),
        Some(&Value::Null)
    );

    let restarted = cost_backfill(&w, account, &w.admin, json!({})).await;
    assert_eq!(restarted.status, 200, "{restarted:?}");
    assert_eq!(restarted.body["examined"], 256);
    assert_eq!(restarted.body["filled"], 0);
    let cursor = restarted.body["next_after"]
        .as_str()
        .expect("重新确认后还有一页");
    // 旧调用刚送达，成本应留待下次操作，本次检查仍截止在第一页开始时。
    send(&w, vec![delayed]).await;
    let last = cost_backfill(&w, account, &w.admin, json!({"after": cursor})).await;
    assert_eq!(last.status, 200, "{last:?}");
    assert_eq!(last.body["examined"], 2);
    assert_eq!(last.body["filled"], 1);
    assert_eq!(last.body.get("next_after"), Some(&Value::Null));
    assert_eq!(
        event(&admin_get(&w, last_event).await, "last").get("cost_micro_cny"),
        Some(&json!(0))
    );
    let unknown = admin_get(&w, "/admin/pricing/charges?event_id=earlier-000").await;
    assert_eq!(
        event(&unknown, "earlier-000").get("cost_micro_cny"),
        Some(&Value::Null)
    );
    let legacy = admin_get(&w, "/admin/pricing/charges?event_id=legacy-usd").await;
    let legacy = event(&legacy, "legacy-usd");
    assert_eq!(legacy["cost_micro_usd"], 40_000);
    assert_eq!(legacy.get("cost_micro_cny"), Some(&Value::Null));
    assert_eq!(w.cp.count("pricing_cost_backfills").await, 1);
    let delayed_path = "/admin/pricing/charges?event_id=delayed";
    assert_eq!(
        event(&admin_get(&w, delayed_path).await, "delayed").get("cost_micro_cny"),
        Some(&Value::Null)
    );
    let first = cost_backfill(&w, account, &w.admin, json!({})).await;
    assert_eq!(first.status, 200, "{first:?}");
    let cursor = first.body["next_after"].as_str().expect("新一轮第二页");
    let last = cost_backfill(&w, account, &w.admin, json!({"after": cursor})).await;
    assert_eq!(last.status, 200, "{last:?}");
    assert_eq!(last.body["examined"], 3);
    assert_eq!(last.body["filled"], 1);
    assert_eq!(
        event(&admin_get(&w, delayed_path).await, "delayed").get("cost_micro_cny"),
        Some(&json!(0))
    );
}
