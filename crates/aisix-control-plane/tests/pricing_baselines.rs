//! Baselines: each model's official USD list price, one rate card per model,
//! effective-dated and append-only, written through
//! `POST /admin/pricing/baselines` from the built-in catalogue (with its
//! provenance and the price changes it announces) or by hand (with a note),
//! and listed per live model by `GET /admin/pricing/baselines`, which shows
//! when the catalogue a control plane runs with now says something else than
//! the catalogue-sourced baseline in force.
//!
//! Every expected value comes from the catalogue or the card the test itself
//! writes.

mod support;

use std::sync::Arc;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{OperatorAuth, OperatorRole};
use aisix_control_plane::pricing::baselines::{self, BaselineSource, NewBaseline};
use aisix_pricing::{Catalogue, RateCard};
use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{db_now, seed_provider_key};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

const PATH: &str = "/admin/pricing/baselines";

/// USD per million: input 4, output 20.
fn card() -> Value {
    json!({"default": {"base": {"input": "4", "output": "20"}}})
}

/// A live model named `name`, on a provider key of its own; its id.
async fn model(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let pk = seed_provider_key(cp, admin).await;
    let r = cp
        .send(
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": name,
                "provider": "openai",
                "model_name": "gpt-4o-mini",
                "provider_key_id": pk,
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "create model {name}: {r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a model id")).expect("a model id")
}

/// Deletes model `id` as `admin`.
async fn delete_model(cp: &TestCp, admin: &OperatorSession, id: Uuid) {
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM gw_resources WHERE kind = 'models' AND id = $1")
            .bind(id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the model's version");
    let status = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .delete(cp.url(&format!("/admin/resources/models/{id}")))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .expect("send the delete")
        .status();
    assert_eq!(status, 200, "delete model {id}");
}

async fn post(cp: &TestCp, who: &OperatorSession, body: Value) -> Resp {
    cp.send("POST", PATH, Some(who.cookie()), Some(body)).await
}

/// `POST /admin/pricing/baselines` as `who`, which must answer 201; the
/// answer.
async fn write(cp: &TestCp, who: &OperatorSession, body: Value) -> Value {
    let r = post(cp, who, body).await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body
}

/// `GET /admin/pricing/baselines` as `who`: its items.
async fn list(cp: &TestCp, who: &OperatorSession) -> Vec<Value> {
    let r = cp.send("GET", PATH, Some(who.cookie()), None).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

/// The listed item of `model`.
fn item(items: &[Value], model: Uuid) -> Value {
    items
        .iter()
        .find(|i| i["model_id"] == json!(model))
        .unwrap_or_else(|| panic!("model {model} is not listed: {items:?}"))
        .clone()
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

async fn pricing_version(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT (value::text)::bigint FROM cp_settings WHERE key = 'pricing_version'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pricing_version")
}

fn instant(v: &Value) -> DateTime<Utc> {
    v.as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| panic!("an RFC 3339 instant: {v}"))
}

/// UTC midnight of `day`.
fn midnight(day: NaiveDate) -> DateTime<Utc> {
    day.and_time(NaiveTime::MIN).and_utc()
}

/// The UTC day `days` from now.
fn day_in(days: i64) -> NaiveDate {
    (Utc::now() + TimeDelta::days(days)).date_naive()
}

/// One catalogue entry: `key`, read from the provider's own pricing page on
/// `as_of`, input `input` and output 2 USD per million, plus the fields
/// `extra` (JSON members, each followed by a comma).
fn entry(key: &str, as_of: &str, extra: &str, input: &str) -> String {
    format!(
        r#"{{"key":"{key}","source_url":"https://openai.com/api/pricing","as_of":"{as_of}",{extra}"card":{{"default":{{"base":{{"input":"{input}","output":"2"}}}}}}}}"#
    )
}

fn catalogue(version: &str, entries: &[String]) -> Arc<Catalogue> {
    let file = format!(
        r#"{{"version":"{version}","entries":[{}]}}"#,
        entries.join(",")
    );
    Arc::new(Catalogue::from_json(&file).expect("the test catalogue loads"))
}

/// The card `GET /admin/pricing/catalogue` lists for `key`'s entry dated
/// `dated` (`None` for its undated one).
async fn listed_card(
    cp: &TestCp,
    who: &OperatorSession,
    key: &str,
    dated: Option<NaiveDate>,
) -> Value {
    let r = cp
        .send("GET", "/admin/pricing/catalogue", Some(who.cookie()), None)
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|e| e["key"] == json!(key) && e["effective_from"] == json!(dated))
        .unwrap_or_else(|| panic!("{key} @ {dated:?} in {r:?}"))["card"]
        .clone()
}

#[tokio::test]
async fn a_manual_baseline_needs_a_note() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-manual-7c1").await;
    let now = db_now(&cp).await;
    let manual = |note: Option<Value>| {
        let mut body = json!({"model_id": m, "effective_from": now, "card": card()});
        if let Some(note) = note {
            body["note"] = note;
        }
        body
    };
    let r = post(&cp, &admin, manual(None)).await;
    assert_422_at(&r, "/note");
    assert_eq!(r.body["details"][0]["message"], "手动录入需要填写说明");
    for note in [
        json!(""),
        json!("   "),
        json!(null),
        json!("价".repeat(501)),
    ] {
        let r = post(&cp, &admin, manual(Some(note.clone()))).await;
        assert_422_at(&r, "/note");
    }
    assert_eq!(cp.count("pricing_baselines").await, 0);

    let note = "按官方价格页手动录入，2026-11 核对";
    let body = write(&cp, &admin, manual(Some(json!(note)))).await;
    let rev = &body["revision"];
    assert_eq!(rev["model_id"], json!(m));
    assert_eq!(instant(&rev["effective_from"]), now);
    assert_eq!(rev["card"], card());
    assert_eq!(rev["source_kind"], "manual");
    assert_eq!(rev["source"], json!({"note": note}));
    assert_eq!(
        rev["created_by"],
        json!({"id": admin.id, "email": admin.email})
    );
    assert!(rev["revision_id"].is_i64(), "{rev}");
    assert!(rev["engine_version"].is_i64(), "{rev}");
    assert_eq!(body["later_revisions"], json!([]));
    assert_eq!(body["warnings"], json!([]));

    // 500 characters is the most a note holds.
    let longest = "价".repeat(500);
    let body = write(&cp, &admin, manual(Some(json!(longest)))).await;
    assert_eq!(body["revision"]["source"]["note"], json!(longest));
    assert_eq!(cp.count("pricing_baselines").await, 2);
}

#[tokio::test]
async fn a_catalogue_baseline_carries_its_provenance_and_later_dated_entries() {
    let change = day_in(2);
    let dated = format!(r#""effective_from":"{change}","#);
    let aliases = r#""aliases":["base-alias-7c1"],"#;
    let cat = catalogue(
        "2026-11-06",
        &[
            entry("openai/base-7c1", "2026-11-01", aliases, "1"),
            entry(
                "openai/base-7c1",
                "2026-11-05",
                &format!("{aliases}{dated}"),
                "3",
            ),
        ],
    );
    let Some(cp) = TestCp::start_with_catalogue(cat).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-catalogue-7c1").await;
    let now = db_now(&cp).await;
    let body = write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": now, "catalogue_key": "openai/base-7c1"}),
    )
    .await;
    let rev = &body["revision"];
    assert_eq!(instant(&rev["effective_from"]), now);
    assert_eq!(rev["source_kind"], "catalogue");
    assert_eq!(
        rev["source"],
        json!({
            "catalogue_version": "2026-11-06",
            "entry_key": "openai/base-7c1",
            "entry_effective_from": null,
            "source_url": "https://openai.com/api/pricing",
            "as_of": "2026-11-01",
        })
    );
    assert_eq!(
        rev["card"],
        listed_card(&cp, &admin, "openai/base-7c1", None).await
    );
    assert_eq!(rev["card"]["default"]["base"]["input"], "1");
    assert_eq!(
        rev["created_by"],
        json!({"id": admin.id, "email": admin.email})
    );

    let later = body["later_revisions"].as_array().expect("later revisions");
    assert_eq!(later.len(), 1, "{body}");
    assert_eq!(instant(&later[0]["effective_from"]), midnight(change));
    assert_eq!(later[0]["model_id"], json!(m));
    assert_eq!(later[0]["source_kind"], "catalogue");
    assert_eq!(
        later[0]["source"],
        json!({
            "catalogue_version": "2026-11-06",
            "entry_key": "openai/base-7c1",
            "entry_effective_from": change,
            "source_url": "https://openai.com/api/pricing",
            "as_of": "2026-11-05",
        })
    );
    assert_eq!(
        later[0]["card"],
        listed_card(&cp, &admin, "openai/base-7c1", Some(change)).await
    );
    assert_eq!(body["warnings"], json!([]));

    // The list shows the one in force and the one scheduled.
    let listed = item(&list(&cp, &admin).await, m);
    assert_eq!(listed["current"]["revision_id"], rev["revision_id"]);
    assert_eq!(listed["current"]["source"], rev["source"]);
    let upcoming: Vec<&Value> = listed["upcoming"]
        .as_array()
        .expect("upcoming")
        .iter()
        .map(|u| &u["revision_id"])
        .collect();
    assert_eq!(upcoming, vec![&later[0]["revision_id"]], "{listed}");
    assert_eq!(listed["catalogue_update"], Value::Null, "{listed}");

    // An alias names its entry, which the source records by its key.
    let other = model(&cp, &admin, "bl-catalogue-alias-7c1").await;
    let body = write(
        &cp,
        &admin,
        json!({
            "model_id": other,
            "effective_from": db_now(&cp).await,
            "catalogue_key": "openai/base-alias-7c1",
        }),
    )
    .await;
    assert_eq!(
        body["revision"]["source"]["entry_key"],
        json!("openai/base-7c1")
    );
    assert_eq!(body["later_revisions"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn a_baseline_is_a_card_or_a_catalogue_key() {
    let cat = catalogue(
        "2026-11-06",
        &[entry("openai/either-7c1", "2026-11-01", "", "1")],
    );
    let Some(cp) = TestCp::start_with_catalogue(cat).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-either-7c1").await;
    let now = db_now(&cp).await;
    let version = pricing_version(&cp).await;
    let note = "手动录入";

    for body in [
        // Both.
        json!({"model_id": m, "effective_from": now, "card": card(), "note": note,
               "catalogue_key": "openai/either-7c1"}),
        // Neither.
        json!({"model_id": m, "effective_from": now, "note": note}),
        json!({"model_id": m, "effective_from": now, "card": null, "note": note}),
        // Cards that are no rate card, or that break its rules: no default
        // tier, a negative rate, a rate that is no number.
        json!({"model_id": m, "effective_from": now, "note": note,
               "card": {"flex": {"base": {"input": "1", "output": "2"}}}}),
        json!({"model_id": m, "effective_from": now, "note": note,
               "card": {"default": {"base": {"input": "-1", "output": "2"}}}}),
        json!({"model_id": m, "effective_from": now, "note": note,
               "card": {"default": {"base": {"input": "abc", "output": "2"}}}}),
        json!({"model_id": m, "effective_from": now, "note": note, "card": "4/20"}),
    ] {
        let r = post(&cp, &admin, body.clone()).await;
        assert_422_at(&r, "/card");
    }
    for key in ["openai/nope-7c1", "nope", "anthropic/either-7c1", "openai/"] {
        let r = post(
            &cp,
            &admin,
            json!({"model_id": m, "effective_from": now, "catalogue_key": key}),
        )
        .await;
        assert_422_at(&r, "/catalogue_key");
    }
    // A note belongs to a card; a catalogue price carries its own source.
    let r = post(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": now, "catalogue_key": "openai/either-7c1",
               "note": note}),
    )
    .await;
    assert_422_at(&r, "/note");
    // A model that does not exist, or no longer does.
    let r = post(
        &cp,
        &admin,
        json!({"model_id": Uuid::new_v4(), "effective_from": now, "card": card(), "note": note}),
    )
    .await;
    assert_422_at(&r, "/model_id");
    let gone = model(&cp, &admin, "bl-gone-7c1").await;
    delete_model(&cp, &admin, gone).await;
    let r = post(
        &cp,
        &admin,
        json!({"model_id": gone, "effective_from": now, "catalogue_key": "openai/either-7c1"}),
    )
    .await;
    assert_422_at(&r, "/model_id");
    // A proposal of an import is confirmed only through its own route.
    let r = post(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": now, "proposal_id": Uuid::new_v4(),
               "attested": true}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");

    assert_eq!(cp.count("pricing_baselines").await, 0);
    assert_eq!(pricing_version(&cp).await, version);
    let pricing_audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action LIKE 'pricing.%'")
            .fetch_one(cp.db().pool())
            .await
            .expect("count pricing audit rows");
    assert_eq!(pricing_audits, 0);
}

#[tokio::test]
async fn a_baseline_never_starts_in_the_past() {
    let cat = catalogue(
        "2026-11-06",
        &[entry("openai/past-7c1", "2026-11-01", "", "1")],
    );
    let Some(cp) = TestCp::start_with_catalogue(cat).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-past-7c1").await;
    let version = pricing_version(&cp).await;
    let past = db_now(&cp).await - TimeDelta::minutes(1);
    let r = post(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": past, "card": card(), "note": "过去"}),
    )
    .await;
    assert_422_at(&r, "/effective_from");
    let r = post(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": past, "catalogue_key": "openai/past-7c1"}),
    )
    .await;
    assert_422_at(&r, "/effective_from");
    assert_eq!(cp.count("pricing_baselines").await, 0);
    assert_eq!(pricing_version(&cp).await, version);

    // Now, and later, are fine.
    let now = db_now(&cp).await;
    write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": now, "card": card(), "note": "现在"}),
    )
    .await;
    write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": now + TimeDelta::days(1),
               "catalogue_key": "openai/past-7c1"}),
    )
    .await;
    assert_eq!(cp.count("pricing_baselines").await, 2);
}

#[tokio::test]
async fn the_baseline_list_puts_models_without_one_first() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    // Created out of name order.
    let d = model(&cp, &admin, "bl-d-7c1").await;
    let b = model(&cp, &admin, "bl-b-7c1").await;
    let c = model(&cp, &admin, "bl-c-7c1").await;
    let a = model(&cp, &admin, "bl-a-7c1").await;
    let e = model(&cp, &admin, "bl-e-7c1").await;
    let gone = model(&cp, &admin, "bl-0-gone-7c1").await;
    let manual = |m: Uuid, at: DateTime<Utc>, input: &str| {
        json!({"model_id": m, "effective_from": at, "note": "手动",
               "card": {"default": {"base": {"input": input, "output": "20"}}}})
    };
    let now = db_now(&cp).await;
    let a_rev = write(&cp, &admin, manual(a, now, "4")).await["revision"].clone();
    let c_rev = write(&cp, &admin, manual(c, now, "5")).await["revision"].clone();
    write(&cp, &admin, manual(gone, now, "6")).await;
    delete_model(&cp, &admin, gone).await;
    // Only scheduled: e has no baseline in force yet. Of two scheduled at
    // one instant, the later write is the one that will apply.
    let tomorrow = now + TimeDelta::days(1);
    write(&cp, &admin, manual(e, tomorrow, "7")).await;
    let e_rev = write(&cp, &admin, manual(e, tomorrow, "8")).await["revision"].clone();

    let items = list(&cp, &viewer).await;
    let ours = [a, b, c, d, e];
    let order: Vec<Value> = items
        .iter()
        .map(|i| i["model_id"].clone())
        .filter(|id| ours.iter().any(|m| json!(m) == *id) || *id == json!(gone))
        .collect();
    assert_eq!(
        order,
        vec![json!(b), json!(d), json!(e), json!(a), json!(c)],
        "{items:?}"
    );
    for i in &items {
        for field in [
            "model_id",
            "name",
            "current",
            "upcoming",
            "catalogue_update",
        ] {
            assert!(i.get(field).is_some(), "{field} in {i}");
        }
    }
    let item_b = item(&items, b);
    assert_eq!(item_b["name"], "bl-b-7c1");
    assert_eq!(item_b["current"], Value::Null);
    assert_eq!(item_b["upcoming"], json!([]));
    assert_eq!(item_b["catalogue_update"], Value::Null);

    let item_a = item(&items, a);
    assert_eq!(item_a["name"], "bl-a-7c1");
    assert_eq!(item_a["current"], a_rev);
    assert_eq!(item_a["upcoming"], json!([]));
    assert_eq!(item_a["catalogue_update"], Value::Null);
    assert_eq!(item(&items, c)["current"], c_rev);

    let item_e = item(&items, e);
    assert_eq!(item_e["current"], Value::Null);
    assert_eq!(item_e["upcoming"], json!([e_rev]), "{item_e}");
}

#[tokio::test]
async fn a_changed_catalogue_shows_as_an_update() {
    let change = day_in(3);
    let v1 = catalogue(
        "2026-11-06",
        &[
            entry("openai/changes-7c1", "2026-11-01", "", "1"),
            entry("openai/same-7c1", "2026-11-01", "", "2"),
            entry("openai/dated-7c1", "2026-11-01", "", "3"),
        ],
    );
    // The next release's catalogue: one key re-priced, one unchanged, and
    // one announcing a price change.
    let v2 = catalogue(
        "2026-11-07",
        &[
            entry("openai/changes-7c1", "2026-11-07", "", "5"),
            entry("openai/same-7c1", "2026-11-01", "", "2"),
            entry("openai/dated-7c1", "2026-11-01", "", "3"),
            entry(
                "openai/dated-7c1",
                "2026-11-07",
                &format!(r#""effective_from":"{change}","#),
                "6",
            ),
        ],
    );
    let Some(cp) = TestCp::start_with_catalogue(v1).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let changes = model(&cp, &admin, "bl-changes-7c1").await;
    let same = model(&cp, &admin, "bl-same-7c1").await;
    let dated = model(&cp, &admin, "bl-dated-7c1").await;
    let by_hand = model(&cp, &admin, "bl-by-hand-7c1").await;
    let now = db_now(&cp).await;
    for (m, key) in [
        (changes, "openai/changes-7c1"),
        (same, "openai/same-7c1"),
        (dated, "openai/dated-7c1"),
    ] {
        write(
            &cp,
            &admin,
            json!({"model_id": m, "effective_from": now, "catalogue_key": key}),
        )
        .await;
    }
    write(
        &cp,
        &admin,
        json!({"model_id": by_hand, "effective_from": now, "card": card(), "note": "手动"}),
    )
    .await;
    let items = list(&cp, &admin).await;
    for m in [changes, same, dated, by_hand] {
        assert_eq!(
            item(&items, m)["catalogue_update"],
            Value::Null,
            "{items:?}"
        );
    }

    // An upgraded control plane on the same database.
    let upgraded = cp.restart_with_catalogue(v2).await;
    let items = list(&upgraded, &admin).await;
    assert_eq!(
        item(&items, changes)["catalogue_update"],
        json!({
            "catalogue_version": "2026-11-07",
            "entry_key": "openai/changes-7c1",
            "entries": [{
                "effective_from": null,
                "card": listed_card(&upgraded, &admin, "openai/changes-7c1", None).await,
            }],
        }),
        "{items:?}"
    );
    assert_eq!(
        item(&items, dated)["catalogue_update"],
        json!({
            "catalogue_version": "2026-11-07",
            "entry_key": "openai/dated-7c1",
            "entries": [
                {
                    "effective_from": null,
                    "card": listed_card(&upgraded, &admin, "openai/dated-7c1", None).await,
                },
                {
                    "effective_from": change,
                    "card": listed_card(&upgraded, &admin, "openai/dated-7c1", Some(change)).await,
                },
            ],
        }),
        "{items:?}"
    );
    assert_eq!(item(&items, same)["catalogue_update"], Value::Null);
    assert_eq!(item(&items, by_hand)["catalogue_update"], Value::Null);
    // The baselines themselves did not follow the catalogue.
    assert_eq!(
        item(&items, changes)["current"]["card"]["default"]["base"]["input"],
        "1"
    );

    // Confirming an update writes it, and the update is gone.
    for (m, key) in [(changes, "openai/changes-7c1"), (dated, "openai/dated-7c1")] {
        write(
            &upgraded,
            &admin,
            json!({"model_id": m, "effective_from": db_now(&upgraded).await, "catalogue_key": key}),
        )
        .await;
    }
    let items = list(&upgraded, &admin).await;
    for m in [changes, same, dated, by_hand] {
        assert_eq!(
            item(&items, m)["catalogue_update"],
            Value::Null,
            "{items:?}"
        );
    }
    assert_eq!(
        item(&items, changes)["current"]["source"]["catalogue_version"],
        "2026-11-07"
    );
}

#[tokio::test]
async fn baselines_refuse_update_delete_and_truncate() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-append-7c1").await;
    write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": db_now(&cp).await, "card": card(), "note": "n"}),
    )
    .await;
    for sql in [
        "UPDATE pricing_baselines SET card = '{}'::jsonb",
        "DELETE FROM pricing_baselines",
        // With the table that references it in a proposal: Postgres refuses
        // a truncate of `pricing_baselines` alone for the foreign key, which
        // is a different refusal than the trigger's.
        "TRUNCATE pricing_baselines, pricing_import_proposals",
    ] {
        let err = sqlx::query(sql)
            .execute(cp.db().pool())
            .await
            .expect_err(sql);
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    assert_eq!(cp.count("pricing_baselines").await, 1);
}

#[tokio::test]
async fn writing_a_baseline_bumps_the_pricing_version() {
    let cat = catalogue(
        "2026-11-06",
        &[
            entry("openai/bump-7c1", "2026-11-01", "", "1"),
            entry(
                "openai/bump-7c1",
                "2026-11-01",
                &format!(r#""effective_from":"{}","#, day_in(5)),
                "2",
            ),
        ],
    );
    let Some(cp) = TestCp::start_with_catalogue(cat).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-bump-7c1").await;
    let audit = || async {
        sqlx::query_as::<_, (String, String, Option<String>, Value)>(
            "SELECT action, target_kind, target_id, detail FROM audit_log \
             WHERE action LIKE 'pricing.%' ORDER BY id",
        )
        .fetch_all(cp.db().pool())
        .await
        .expect("read the audit log")
    };

    let version = pricing_version(&cp).await;
    let body = write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": db_now(&cp).await, "card": card(), "note": "n"}),
    )
    .await;
    assert_eq!(pricing_version(&cp).await, version + 1);
    let rows = audit().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "pricing.baseline.create");
    assert_eq!(rows[0].1, "pricing_baseline");
    assert_eq!(rows[0].2, Some(body["revision"]["revision_id"].to_string()));
    assert_eq!(rows[0].3["source_kind"], "manual");
    assert_eq!(rows[0].3["model_id"], json!(m));

    // A catalogue price and the change it announces: two revisions, each a
    // price write.
    let version = pricing_version(&cp).await;
    let body = write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": db_now(&cp).await,
               "catalogue_key": "openai/bump-7c1"}),
    )
    .await;
    assert_eq!(pricing_version(&cp).await, version + 2);
    let rows = audit().await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    let written: Vec<Option<String>> = std::iter::once(&body["revision"])
        .chain(body["later_revisions"].as_array().expect("later revisions"))
        .map(|r| Some(r["revision_id"].to_string()))
        .collect();
    assert_eq!(
        rows[1..].iter().map(|r| r.2.clone()).collect::<Vec<_>>(),
        written
    );
    for row in &rows[1..] {
        assert_eq!(row.0, "pricing.baseline.create");
        assert_eq!(row.3["source_kind"], "catalogue");
    }

    // A refused write moves nothing.
    let version = pricing_version(&cp).await;
    let r = post(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": db_now(&cp).await, "card": card()}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(pricing_version(&cp).await, version);
    assert_eq!(audit().await.len(), 3);
}

#[tokio::test]
async fn an_off_peak_catalogue_price_is_warned() {
    let off_peak = r#""notes":"Standard (off-peak) price; peak windows bill double.","#;
    let cat = catalogue(
        "2026-11-06",
        &[
            entry("openai/offpeak-7c1", "2026-11-01", off_peak, "1"),
            entry(
                "openai/offpeak-7c1",
                "2026-11-01",
                &format!(r#"{off_peak}"effective_from":"{}","#, day_in(4)),
                "2",
            ),
            entry("openai/allday-7c1", "2026-11-01", "", "1"),
        ],
    );
    let Some(cp) = TestCp::start_with_catalogue(cat).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-offpeak-7c1").await;
    let body = write(
        &cp,
        &admin,
        json!({"model_id": m, "effective_from": db_now(&cp).await,
               "catalogue_key": "openai/offpeak-7c1"}),
    )
    .await;
    // Written all the same, with one warning for the key.
    assert_eq!(body["later_revisions"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        body["warnings"],
        json!([{
            "code": "time_of_day_price",
            "message": "openai/offpeak-7c1 的目录价格是非高峰时段价格，高峰时段上游按更高价格收费；基准价按非高峰价格计",
        }])
    );

    let other = model(&cp, &admin, "bl-allday-7c1").await;
    let body = write(
        &cp,
        &admin,
        json!({"model_id": other, "effective_from": db_now(&cp).await,
               "catalogue_key": "openai/allday-7c1"}),
    )
    .await;
    assert_eq!(body["warnings"], json!([]));
}

/// A baseline written now that one already scheduled for later will replace
/// is still written, and the answer names the one that takes over and when;
/// another model's scheduled baseline does not count.
#[tokio::test]
async fn a_baseline_a_scheduled_one_will_replace_says_so() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-scheduled-7c1").await;
    let other = model(&cp, &admin, "bl-scheduled-other-7c1").await;
    let now = db_now(&cp).await;
    let manual = |m: Uuid, at: DateTime<Utc>| json!({"model_id": m, "effective_from": at, "card": card(), "note": "n"});
    let scheduled = write(&cp, &admin, manual(m, now + TimeDelta::days(1))).await;
    assert_eq!(scheduled["warnings"], json!([]));
    write(&cp, &admin, manual(other, now + TimeDelta::days(2))).await;

    let body = write(&cp, &admin, manual(m, db_now(&cp).await)).await;
    let warnings = body["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{body}");
    assert_eq!(
        (
            warnings[0]["code"].clone(),
            warnings[0]["revision_id"].clone(),
            warnings[0]["effective_from"].clone(),
        ),
        (
            json!("superseded_later"),
            scheduled["revision"]["revision_id"].clone(),
            scheduled["revision"]["effective_from"].clone(),
        )
    );
    assert!(
        warnings[0]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "{body}"
    );
}

/// What an attested import confirmation writes, through the function it
/// calls: the dataset's provenance and the attestation, as the list shows
/// them.
#[tokio::test]
async fn a_third_party_baseline_keeps_its_attestation() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let m = model(&cp, &admin, "bl-third-party-7c1").await;
    let (import_id, proposal_id) = (Uuid::new_v4(), Uuid::new_v4());
    let attested_at = db_now(&cp).await;
    let checked = NaiveDate::from_ymd_opt(2026, 11, 2).expect("a date");
    let row = NewBaseline {
        model_id: m,
        effective_from: attested_at,
        card: serde_json::from_value::<RateCard>(card()).expect("a card"),
        source: BaselineSource::ThirdParty {
            dataset: "dataset-7c1".to_string(),
            dataset_ref: "abc1234".to_string(),
            pricing_url: Some("https://openai.com/api/pricing".to_string()),
            prices_checked: Some(checked),
            import_id,
            proposal_id,
            attested_by: admin.id,
            attested_at,
        },
    };
    let auth = OperatorAuth {
        id: admin.id,
        email: admin.email.clone(),
        role: OperatorRole::Operator,
    };
    let (rev, warnings) = cp
        .db()
        .tx(move |conn| {
            async move { baselines::insert_baseline(conn, &row, Actor::Operator(&auth)).await }
                .boxed()
        })
        .await
        .expect("a third-party baseline");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(rev.source_kind, "third_party");

    let listed = item(&list(&cp, &admin).await, m);
    let current = &listed["current"];
    assert_eq!(current["revision_id"], json!(rev.revision_id));
    assert_eq!(current["source_kind"], "third_party");
    assert_eq!(
        current["source"],
        json!({
            "dataset": "dataset-7c1",
            "dataset_ref": "abc1234",
            "pricing_url": "https://openai.com/api/pricing",
            "prices_checked": "2026-11-02",
            "import_id": import_id,
            "proposal_id": proposal_id,
            "attested_by": admin.id,
            "attested_at": attested_at,
        })
    );
    assert_eq!(
        current["created_by"],
        json!({"id": admin.id, "email": admin.email})
    );
    assert_eq!(listed["catalogue_update"], Value::Null);
}

#[tokio::test]
async fn who_may_call_the_baseline_routes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let m = model(&cp, &operator, "bl-roles-7c1").await;
    let body = json!({"model_id": m, "effective_from": db_now(&cp).await, "card": card(),
                      "note": "n"});

    let r = cp.send("GET", PATH, None, None).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (401, json!("unauthenticated"))
    );
    let r = cp.send("POST", PATH, None, Some(body.clone())).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (401, json!("unauthenticated"))
    );
    let r = post(&cp, &viewer, body.clone()).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (403, json!("forbidden")),
        "{r:?}"
    );
    assert_eq!(cp.count("pricing_baselines").await, 0);

    // A viewer reads; an operator writes.
    assert!(!list(&cp, &viewer).await.is_empty());
    write(&cp, &operator, body).await;
    assert_eq!(cp.count("pricing_baselines").await, 1);
    assert_eq!(
        item(&list(&cp, &viewer).await, m)["current"]["source_kind"],
        "manual"
    );
}
