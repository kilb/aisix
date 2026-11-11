//! `GET /api/prices`: what each of a tenant's assigned groups charges, in CNY
//! per million tokens, at the prices in force now and less the tenant's own
//! discount.
//!
//! A price is the official USD baseline times the group's rate for that
//! model, less the tenant's discount (R12: a listing price is the final one).
//! Only models with both a baseline and a rate in force are listed: an
//! unpriced model is absent rather than shown at 0.
//!
//! The answer is a tenant's, so it names no routing name, no provider, no
//! upstream model and no provider account.

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::pricing::rates::{self, NewGroupRate};
use aisix_control_plane::pricing::store::{self, NewDiscount};
use aisix_pricing::{CnyPerUsd, RateCard};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::members::MemberSession;
use support::world::{db_now, insert_baseline, seed_model, seed_tenant, world, World};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// Group A's routing name: distinctive, and never shown to a tenant.
const ROUTING_A: &str = "grp-route-9f3";

/// The baseline the prices tests charge against: 4 USD per million input and
/// 20 output.
fn baseline_card() -> RateCard {
    serde_json::from_value(json!({"default": {"base": {"input": "4", "output": "20"}}}))
        .expect("a card")
}

/// A live group whose routing name is `name` and whose display name is
/// `display_name`, created as an operator; its id.
async fn group(cp: &TestCp, admin: &OperatorSession, name: &str, display_name: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/provider-groups",
            Some(admin.cookie()),
            Some(json!({"name": name, "display_name": display_name,
                        "description": "说明", "default_cny_per_usd": "1"})),
        )
        .await;
    assert_eq!(r.status, 201, "create group {name}: {r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("a group id")).expect("a uuid")
}

/// A rate of `cny_per_usd` for `group`'s default (`model` `None`) or for one
/// model, starting now, written by the control plane's own path.
async fn rate(cp: &TestCp, group: Uuid, model: Option<Uuid>, cny_per_usd: &str) {
    let from: DateTime<Utc> = db_now(cp).await;
    let row = NewGroupRate {
        group_id: group,
        model_id: model,
        effective_from: from,
        cny_per_usd: Some(CnyPerUsd::parse(cny_per_usd).expect("a rate")),
        note: "the prices test's rate".to_string(),
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

/// The tenant's discount of `bps` starting now.
async fn discount(cp: &TestCp, tenant: Uuid, bps: u16) {
    let from: DateTime<Utc> = db_now(cp).await;
    let row = NewDiscount {
        tenant_id: tenant,
        effective_from: from,
        discount_bps: bps,
    };
    cp.db()
        .tx(move |c| async move { store::insert_discount(c, &row, Actor::System).await }.boxed())
        .await
        .expect("the discount");
}

/// Gives `tenant` the `model`, as a grant change does.
async fn grant(cp: &TestCp, tenant: Uuid, model: Uuid) {
    sqlx::query(
        "UPDATE tenants SET allowed_model_ids = array_append(allowed_model_ids, $2) \
         WHERE id = $1 AND NOT ($2 = ANY(allowed_model_ids))",
    )
    .bind(tenant)
    .bind(model)
    .execute(cp.db().pool())
    .await
    .expect("grant the model");
}

/// Replaces `tenant`'s groups with the live groups `ids` name, through the
/// tenant write path the Dashboard uses.
async fn assign(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, ids: &[Uuid]) {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the version");
    let mut names: Vec<String> = Vec::new();
    for id in ids {
        names.push(
            sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
                .bind(id)
                .fetch_one(cp.db().pool())
                .await
                .expect("the group's name"),
        );
    }
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .request(
            reqwest::Method::PATCH,
            cp.url(&format!("/admin/tenants/{tenant}")),
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_provider_groups": names}))
        .send()
        .await
        .expect("send the assignment");
    let status = r.status().as_u16();
    let body = r.text().await.expect("read the answer");
    assert_eq!(status, 200, "assign {names:?}: {body}");
}

/// `GET /api/prices` as `who`.
async fn prices(cp: &TestCp, who: &MemberSession) -> Resp {
    cp.send("GET", "/api/prices", Some(who.cookie()), None)
        .await
}

/// The group `id`'s entry in a prices answer.
fn group_of(body: &Value, id: Uuid) -> Value {
    body["groups"]
        .as_array()
        .unwrap_or_else(|| panic!("groups: {body}"))
        .iter()
        .find(|g| g["id"] == json!(id))
        .unwrap_or_else(|| panic!("group {id} in {body}"))
        .clone()
}

/// The model `id`'s entry in a group entry.
fn model_of(group: &Value, id: Uuid) -> Value {
    group["models"]
        .as_array()
        .unwrap_or_else(|| panic!("models: {group}"))
        .iter()
        .find(|m| m["model_id"] == json!(id))
        .unwrap_or_else(|| panic!("model {id} in {group}"))
        .clone()
}

/// The sandbox every prices test builds: a tenant assigned two groups, A at
/// 7.2 and B at 5.0 with an override of 6.0 for the model `x`; a 10 %
/// discount; and `x` plus an unpriced model `y` granted to it.
struct Priced {
    w: World,
    a: Uuid,
    b: Uuid,
    x: Uuid,
    y: Uuid,
}

async fn setup() -> Option<Priced> {
    let w = world().await?;
    let suffix = Uuid::new_v4().simple();
    let a = group(&w.cp, &w.admin, ROUTING_A, &format!("组A-{suffix}")).await;
    let b = group(&w.cp, &w.admin, &format!("grp-b-{suffix}"), "组B").await;
    let x = seed_model(&w.cp, &w.admin).await;
    let y = seed_model(&w.cp, &w.admin).await;
    let now = db_now(&w.cp).await;
    insert_baseline(&w.cp, x, now, baseline_card())
        .await
        .expect("x's baseline");
    // y gets no baseline: nothing can price it, and it must be absent.
    rate(&w.cp, a, None, "7.2").await;
    rate(&w.cp, b, None, "5").await;
    rate(&w.cp, b, Some(x), "6").await;
    discount(&w.cp, w.tenant, 1_000).await;
    grant(&w.cp, w.tenant, x).await;
    grant(&w.cp, w.tenant, y).await;
    assign(&w.cp, &w.admin, w.tenant, &[a, b]).await;
    Some(Priced { w, a, b, x, y })
}

async fn owner(w: &World) -> MemberSession {
    w.cp.member_login_seeded(w.tenant, MemberRole::Owner).await
}

#[tokio::test]
async fn api_prices_lists_each_assigned_groups_prices_after_the_discount() {
    let Some(p) = setup().await else { return };
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["currency"], "CNY", "{r:?}");

    // A: 4 × 7.2 = 28.8 and 20 × 7.2 = 144, less 10 % → 25.92 and 129.6.
    let a = group_of(&r.body, p.a);
    let x = model_of(&a, p.x);
    assert_eq!(x["input_per_million"], "25.92", "{a}");
    assert_eq!(x["output_per_million"], "129.6", "{a}");
    assert_eq!(x["name"], json!(model_name(&p.w, p.x).await));

    // B overrides x at 6.0: 4 × 6 = 24, less 10 % → 21.6.
    let b = group_of(&r.body, p.b);
    let x = model_of(&b, p.x);
    assert_eq!(x["input_per_million"], "21.6", "{b}");
    assert_eq!(x["output_per_million"], "108", "{b}");

    // An unpriced model is absent, not zero.
    for g in [&a, &b] {
        assert!(
            g["models"]
                .as_array()
                .expect("models")
                .iter()
                .all(|m| m["model_id"] != json!(p.y)),
            "the unpriced model is listed: {g}"
        );
    }
    // A group the tenant is not assigned is absent.
    let shared: Uuid = sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'shared'")
        .fetch_one(p.w.cp.db().pool())
        .await
        .expect("the shared group");
    assert!(
        r.body["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .all(|g| g["id"] != json!(shared)),
        "an unassigned group is listed: {r:?}"
    );
}

/// The display name of `model`.
async fn model_name(w: &World, model: Uuid) -> String {
    sqlx::query_scalar("SELECT doc->>'display_name' FROM gw_resources WHERE id = $1")
        .bind(model)
        .fetch_one(w.cp.db().pool())
        .await
        .expect("the model's name")
}

#[tokio::test]
async fn api_prices_names_no_routing_name_and_no_channel() {
    let Some(p) = setup().await else { return };
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    assert_eq!(r.status, 200, "{r:?}");
    let body = r.body.to_string();
    for token in [ROUTING_A, "grp-b-", "openai", "gpt-4o-mini", "acct-pricing"] {
        assert!(!body.contains(token), "{token} in {body}");
    }
    // The entry names exactly what a tenant is owed and nothing else.
    let a = group_of(&r.body, p.a);
    let mut keys: Vec<&str> = a
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "models")
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["description", "display_name", "id"], "{a}");
    let x = model_of(&a, p.x);
    let mut keys: Vec<&str> = x
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "input_per_million",
            "model_id",
            "name",
            "output_per_million",
            "prices_by_tier"
        ],
        "{x}"
    );
}

#[tokio::test]
async fn api_prices_is_for_every_member() {
    let Some(p) = setup().await else { return };
    for role in [
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ] {
        let who = p.w.cp.member_login_seeded(p.w.tenant, role).await;
        let r = prices(&p.w.cp, &who).await;
        assert_eq!(r.status, 200, "{role:?}: {r:?}");
        let a = group_of(&r.body, p.a);
        assert_eq!(
            model_of(&a, p.x)["input_per_million"],
            "25.92",
            "{role:?}: {a}"
        );
    }
    // A session-less request is 401, not a price.
    let r = p.w.cp.send("GET", "/api/prices", None, None).await;
    assert_eq!(r.status, 401, "{r:?}");
}

#[tokio::test]
async fn another_tenants_groups_never_appear() {
    let Some(p) = setup().await else { return };
    // A second tenant of its own, assigned only `shared` by the defaults.
    let other = seed_tenant(&p.w.cp, &p.w.admin).await;
    let member = p.w.cp.member_login_seeded(other, MemberRole::Owner).await;
    let r = prices(&p.w.cp, &member).await;
    assert_eq!(r.status, 200, "{r:?}");
    let ids: Vec<&Value> = r.body["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|g| &g["id"])
        .collect();
    assert!(!ids.contains(&&json!(p.a)), "{r:?}");
    assert!(!ids.contains(&&json!(p.b)), "{r:?}");
    let body = r.body.to_string();
    assert!(!body.contains(ROUTING_A), "{body}");
    // It cannot even name the first tenant's model: nothing is granted to it.
    assert!(!body.contains(&p.x.to_string()), "{body}");
}

#[tokio::test]
async fn a_discount_of_the_whole_price_lists_zero() {
    let Some(w) = world().await else { return };
    let model = seed_model(&w.cp, &w.admin).await;
    let a = group(
        &w.cp,
        &w.admin,
        &format!("grp-{}", Uuid::new_v4().simple()),
        "全免",
    )
    .await;
    let now = db_now(&w.cp).await;
    insert_baseline(&w.cp, model, now, baseline_card())
        .await
        .expect("the baseline");
    rate(&w.cp, a, None, "7.2").await;
    grant(&w.cp, w.tenant, model).await;
    assign(&w.cp, &w.admin, w.tenant, &[a]).await;
    discount(&w.cp, w.tenant, 10_000).await;
    let r = prices(&w.cp, &owner(&w).await).await;
    assert_eq!(r.status, 200, "{r:?}");
    let x = model_of(&group_of(&r.body, a), model);
    // A free model is listed as 0, not dropped: it has a price, and it is 0.
    assert_eq!(x["input_per_million"], "0", "{x}");
    assert_eq!(x["output_per_million"], "0", "{x}");
}

#[tokio::test]
async fn a_retired_group_is_not_offered() {
    let Some(p) = setup().await else { return };
    sqlx::query("UPDATE provider_groups SET retired_at = now() WHERE id = ANY($1)")
        .bind(vec![p.a, p.b])
        .execute(p.w.cp.db().pool())
        .await
        .expect("retire the groups");
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["groups"], json!([]), "{r:?}");
}

// 完整报价合同仅通过 HTTP JSON 验证；旧服务缺少字段时必须真实失败。
fn quote_rate<'a>(rates: &'a Value, dimension: &str) -> &'a Value {
    rates
        .as_array()
        .expect("rates must exist as an array")
        .iter()
        .find(|r| r["dimension"] == dimension)
        .unwrap_or_else(|| panic!("missing dimension {dimension}: {rates}"))
}

fn all_rates(input: &str, output: &str) -> Value {
    json!({"input": input, "output": output, "cache_read":"1", "cache_write":"5",
        "cache_write_1h":"8", "reasoning":"9", "audio_input":"10", "audio_output":"11",
        "image_input":"12", "image_output":"13", "web_search":"1000"})
}

#[tokio::test]
async fn full_quote_preserves_every_dimension_tier_band_and_group_discount() {
    let Some(p) = setup().await else { return };
    let now = db_now(&p.w.cp).await;
    let card: RateCard = serde_json::from_value(json!({
        "default":{"base":all_rates("4", "20"), "long_context":[
            {"above_input_tokens":200000,"rates":all_rates("8", "40")},
            {"above_input_tokens":400000,"rates":all_rates("12", "60")}]},
        "batch":{"base":all_rates("2", "10"), "long_context":[
            {"above_input_tokens":200000,"rates":all_rates("4", "20")},
            {"above_input_tokens":400000,"rates":all_rates("6", "30")}]},
        "custom-fast":{"base":all_rates("6", "30"), "long_context":[
            {"above_input_tokens":200000,"rates":all_rates("12", "60")},
            {"above_input_tokens":400000,"rates":all_rates("18", "90")}]}
    }))
    .expect("full card");
    insert_baseline(&p.w.cp, p.x, now, card)
        .await
        .expect("full baseline");
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["discount_bps"], 1000,
        "discount metadata is required"
    );
    assert!(
        r.body["pricing_version"].as_i64().is_some_and(|v| v > 0),
        "{r:?}"
    );
    assert!(
        r.body["as_of"]
            .as_str()
            .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
            .is_some(),
        "{r:?}"
    );
    assert!(r
        .body
        .as_object()
        .expect("body")
        .contains_key("valid_until"));
    let x = model_of(&group_of(&r.body, p.a), p.x);
    let tiers = &x["prices_by_tier"];
    assert_eq!(tiers.as_object().expect("tiers").len(), 3);
    let base = &tiers["default"]["base"];
    assert_eq!(base.as_array().expect("base").len(), 11);
    for (dimension, expected) in [
        ("input", "25.92"),
        ("output", "129.6"),
        ("cache_read", "6.48"),
        ("cache_write", "32.4"),
        ("cache_write_1h", "51.84"),
        ("reasoning", "58.32"),
        ("audio_input", "64.8"),
        ("audio_output", "71.28"),
        ("image_input", "77.76"),
        ("image_output", "84.24"),
        ("web_search", "6480"),
    ] {
        let rate = quote_rate(base, dimension);
        assert_eq!(
            rate["price_cny_per_million"], expected,
            "{dimension}: {rate}"
        );
        assert_eq!(
            rate["unit"],
            if dimension == "web_search" {
                "request"
            } else {
                "token"
            }
        );
        assert!(rate.get("fallback_from").is_none(), "explicit rate {rate}");
        assert_eq!(
            rate.as_object().expect("rate").len(),
            3,
            "no platform details: {rate}"
        );
    }
    assert_eq!(
        quote_rate(&tiers["batch"]["base"], "input")["price_cny_per_million"],
        "12.96"
    );
    assert_eq!(
        quote_rate(&tiers["custom-fast"]["base"], "input")["price_cny_per_million"],
        "38.88"
    );
    let bands = tiers["default"]["long_context"].as_array().expect("bands");
    assert_eq!(bands.len(), 2);
    for (band, threshold, expected) in [(&bands[0], 200000, "51.84"), (&bands[1], 400000, "77.76")]
    {
        assert_eq!(band["above_input_tokens"], threshold);
        assert_eq!(band["rates"].as_array().expect("rates").len(), 11);
        assert_eq!(
            quote_rate(&band["rates"], "input")["price_cny_per_million"],
            expected
        );
    }
    let b = model_of(&group_of(&r.body, p.b), p.x);
    assert_eq!(
        quote_rate(&b["prices_by_tier"]["default"]["base"], "input")["price_cny_per_million"],
        "21.6"
    );
}

#[tokio::test]
async fn full_quote_distinguishes_fallback_missing_search_and_explicit_zero() {
    let Some(p) = setup().await else { return };
    let now = db_now(&p.w.cp).await;
    let card: RateCard = serde_json::from_value(json!({"default":{
        "base":{"input":"4","output":"0","cache_write":"5"},
        "long_context":[{"above_input_tokens":200000,"rates":{"input":"8","output":"40","cache_write":"10"}}]},
        "batch":{"base":{"input":"2","output":"10","cache_write":"2"},"long_context":[
            {"above_input_tokens":200000,"rates":{"input":"4","output":"20","cache_write":"4"}}]}
    })).expect("fallback card");
    insert_baseline(&p.w.cp, p.x, now, card)
        .await
        .expect("fallback baseline");
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    let x = model_of(&group_of(&r.body, p.a), p.x);
    let tiers = &x["prices_by_tier"];
    let base = &tiers["default"]["base"];
    assert_eq!(quote_rate(base, "output")["price_cny_per_million"], "0");
    assert!(quote_rate(base, "output").get("fallback_from").is_none());
    for (dimension, from, expected) in [
        ("cache_read", "input", "25.92"),
        ("cache_write_1h", "cache_write", "32.4"),
        ("reasoning", "output", "0"),
        ("audio_input", "input", "25.92"),
        ("audio_output", "output", "0"),
        ("image_input", "input", "25.92"),
        ("image_output", "output", "0"),
    ] {
        let rate = quote_rate(base, dimension);
        assert_eq!(rate["fallback_from"], from, "{rate}");
        assert_eq!(rate["price_cny_per_million"], expected, "{rate}");
    }
    let search = quote_rate(base, "web_search");
    assert!(search
        .as_object()
        .expect("search")
        .contains_key("price_cny_per_million"));
    assert!(search["price_cny_per_million"].is_null());
    assert!(search.get("fallback_from").is_none());
    assert_eq!(
        quote_rate(&tiers["batch"]["base"], "cache_write_1h")["fallback_from"],
        "cache_write"
    );
    assert_eq!(
        quote_rate(&tiers["batch"]["base"], "cache_write_1h")["price_cny_per_million"],
        "12.96"
    );
    assert_eq!(
        quote_rate(
            &tiers["default"]["long_context"][0]["rates"],
            "cache_write_1h"
        )["price_cny_per_million"],
        "64.8"
    );
    discount(&p.w.cp, p.w.tenant, 10000).await;
    let free = prices(&p.w.cp, &owner(&p.w).await).await;
    assert_eq!(free.body["discount_bps"], 10000);
    let free = model_of(&group_of(&free.body, p.a), p.x);
    for rate in free["prices_by_tier"]["default"]["base"]
        .as_array()
        .expect("free rates")
    {
        if rate["dimension"] == "web_search" {
            assert!(rate["price_cny_per_million"].is_null());
        } else {
            assert_eq!(rate["price_cny_per_million"], "0", "{rate}");
        }
    }
}

#[tokio::test]
async fn full_quote_reports_next_revision_boundary_without_using_future_prices() {
    let Some(p) = setup().await else { return };
    let future = db_now(&p.w.cp).await + chrono::TimeDelta::minutes(5);
    insert_baseline(
        &p.w.cp,
        p.x,
        future,
        serde_json::from_value(json!({"default":{"base":{"input":"100","output":"200"}}}))
            .expect("card"),
    )
    .await
    .expect("future baseline");
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    assert_eq!(r.body["valid_until"], json!(future));
    let at =
        DateTime::parse_from_rfc3339(r.body["as_of"].as_str().expect("as_of")).expect("timestamp");
    assert!(at < future);
    let x = model_of(&group_of(&r.body, p.a), p.x);
    assert_eq!(
        quote_rate(&x["prices_by_tier"]["default"]["base"], "input")["price_cny_per_million"],
        "25.92"
    );
}

#[tokio::test]
async fn full_quote_never_falls_back_from_an_invalid_current_revision() {
    let Some(p) = setup().await else { return };
    let before = prices(&p.w.cp, &owner(&p.w).await).await;
    let x = model_of(&group_of(&before.body, p.a), p.x);
    assert!(x["prices_by_tier"].is_object(), "full quote required: {x}");
    // JSON 可解析但违反价卡语义，必须挡住旧 baseline。
    sqlx::query("INSERT INTO pricing_baselines (engine_version,model_id,effective_from,card,source_kind,source) VALUES (2,$1,now(),$2,'manual',$3)")
        .bind(p.x).bind(json!({"default":{"base":{"input":"4","output":"20"},"long_context":[
            {"above_input_tokens":0,"rates":{"input":"8","output":"40"}}]}}))
        .bind(json!({"note":"invalid quote test"}))
        .execute(p.w.cp.db().pool()).await.expect("invalid revision");
    sqlx::query("UPDATE cp_settings SET value = to_jsonb((value #>> '{}')::bigint + 1) WHERE key = 'pricing_version'")
        .execute(p.w.cp.db().pool()).await.expect("bump book version");
    let r = prices(&p.w.cp, &owner(&p.w).await).await;
    for group in r.body["groups"].as_array().expect("groups") {
        assert!(
            group["models"]
                .as_array()
                .expect("models")
                .iter()
                .all(|m| m["model_id"] != json!(p.x)),
            "invalid revision fell back: {r:?}"
        );
    }
}

#[tokio::test]
async fn full_quote_uses_the_shared_versioned_book_and_refreshes_after_price_write() {
    let Some(p) = setup().await else { return };
    let member = owner(&p.w).await;
    let loads_before = p.w.cp.state().prices.loads();
    let first = prices(&p.w.cp, &member).await;
    assert!(
        first.body["pricing_version"].as_i64().is_some(),
        "version metadata required: {first:?}"
    );
    assert!(
        p.w.cp.state().prices.loads() > loads_before,
        "quotes must use the intake price book"
    );
    let cached_loads = p.w.cp.state().prices.loads();
    let second = prices(&p.w.cp, &member).await;
    assert_eq!(
        second.body["pricing_version"],
        first.body["pricing_version"]
    );
    assert_eq!(
        p.w.cp.state().prices.loads(),
        cached_loads,
        "unchanged book reused"
    );
    rate(&p.w.cp, p.a, Some(p.x), "2").await;
    let third = prices(&p.w.cp, &member).await;
    let version: i64 = sqlx::query_scalar(
        "SELECT (value::text)::bigint FROM cp_settings WHERE key='pricing_version'",
    )
    .fetch_one(p.w.cp.db().pool())
    .await
    .expect("version");
    assert_eq!(third.body["pricing_version"], version);
    assert!(third.body["pricing_version"].as_i64() > first.body["pricing_version"].as_i64());
    assert!(p.w.cp.state().prices.loads() > cached_loads);
    let x = model_of(&group_of(&third.body, p.a), p.x);
    assert_eq!(
        quote_rate(&x["prices_by_tier"]["default"]["base"], "input")["price_cny_per_million"],
        "7.2"
    );
    assert_eq!(x["input_per_million"], "7.2");
}

// 卡与分组价在加载中同时变化时，HTTP 报价及版本必须来自同一份价格书。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_quote_never_mixes_revisions_when_a_price_write_races_its_book_load() {
    use sqlx::Connection;
    let Some(p) = setup().await else { return };
    let member = owner(&p.w).await;
    let new_from = db_now(&p.w.cp).await;
    let mut holder = sqlx::PgConnection::connect(p.w.cp.database_url())
        .await
        .expect("connection");
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("LOCK TABLE pricing_group_rates IN ACCESS EXCLUSIVE MODE")
        .execute(&mut holder)
        .await
        .expect("lock rates");
    let response = prices(&p.w.cp, &member);
    let race = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query ILIKE '%FROM pricing_group_rates%')")
                .fetch_one(p.w.cp.db().pool()).await.expect("waiting reader");
            if waiting {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "quote never loaded the shared book"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut tx =
            p.w.cp
                .db()
                .pool()
                .begin()
                .await
                .expect("baseline transaction");
        sqlx::query("INSERT INTO pricing_baselines (engine_version,model_id,effective_from,card,source_kind,source) VALUES (2,$1,$2,$3,'manual',$4)")
            .bind(p.x).bind(new_from).bind(json!({"default":{"base":{"input":"8","output":"40"}}}))
            .bind(json!({"note":"race quote card"})).execute(&mut *tx).await.expect("new baseline");
        sqlx::query("UPDATE cp_settings SET value=to_jsonb((value::text)::bigint+1) WHERE key='pricing_version'")
            .execute(&mut *tx).await.expect("baseline version");
        tx.commit().await.expect("baseline commit");
        sqlx::query("INSERT INTO pricing_group_rates (engine_version,group_id,model_id,effective_from,cny_per_usd_e4,note) VALUES (2,$1,NULL,$2,20000,'race quote rate')")
            .bind(p.a).bind(new_from).execute(&mut holder).await.expect("new rate");
        sqlx::query("UPDATE cp_settings SET value=to_jsonb((value::text)::bigint+1) WHERE key='pricing_version'")
            .execute(&mut holder).await.expect("rate version");
        sqlx::query("COMMIT")
            .execute(&mut holder)
            .await
            .expect("rate commit");
    };
    let (r, ()) = tokio::join!(response, race);
    assert_eq!(r.status, 200, "{r:?}");
    let version: i64 = sqlx::query_scalar(
        "SELECT (value::text)::bigint FROM cp_settings WHERE key='pricing_version'",
    )
    .fetch_one(p.w.cp.db().pool())
    .await
    .expect("version");
    assert_eq!(
        r.body["pricing_version"], version,
        "response version must fence its prices"
    );
    let x = model_of(&group_of(&r.body, p.a), p.x);
    assert_eq!(
        x["input_per_million"], "14.4",
        "old-card/new-rate mixture would be 7.2"
    );
    assert_eq!(
        quote_rate(&x["prices_by_tier"]["default"]["base"], "input")["price_cny_per_million"],
        "14.4"
    );
}

#[tokio::test]
async fn a_public_concrete_model_name_with_hash_keeps_its_catalog_and_quote() {
    let Some(p) = setup().await else { return };
    let name = format!("public#literal-{}", Uuid::new_v4().simple());
    let provider_key = support::world::seed_provider_key(&p.w.cp, &p.w.admin).await;
    let created =
        p.w.cp
            .send(
                "POST",
                "/admin/resources/models",
                Some(p.w.admin.cookie()),
                Some(
                    json!({"doc":{"display_name":name,"internal":false,"provider":"openai",
            "model_name":"gpt-4o-mini","provider_key_id":provider_key}}),
                ),
            )
            .await;
    assert_eq!(
        created.status, 201,
        "a public concrete name is accepted: {created:?}"
    );
    let model = Uuid::parse_str(created.body["id"].as_str().expect("model id")).expect("uuid");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id=$1")
        .bind(p.w.tenant)
        .fetch_one(p.w.cp.db().pool())
        .await
        .expect("tenant version");
    let assigned = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client")
        .patch(p.w.cp.url(&format!("/admin/tenants/{}", p.w.tenant)))
        .header(reqwest::header::COOKIE, p.w.admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_model_ids":[p.x,p.y,model]}))
        .send()
        .await
        .expect("grant public model");
    let status = assigned.status().as_u16();
    let body = assigned.text().await.expect("grant response");
    assert_eq!(
        status, 200,
        "the real grant writer accepts the concrete name: {body}"
    );
    insert_baseline(&p.w.cp, model, db_now(&p.w.cp).await, baseline_card())
        .await
        .expect("public baseline");
    let member = owner(&p.w).await;
    let catalog =
        p.w.cp
            .send("GET", "/api/models", Some(member.cookie()), None)
            .await;
    assert_eq!(catalog.status, 200, "{catalog:?}");
    assert!(
        catalog.body["items"]
            .as_array()
            .expect("catalog")
            .iter()
            .any(|m| m["id"] == json!(model) && m["name"] == name),
        "public call name remains visible: {catalog:?}"
    );
    let r = prices(&p.w.cp, &member).await;
    assert_eq!(r.status, 200, "{r:?}");
    let quote = model_of(&group_of(&r.body, p.a), model);
    assert_eq!(
        quote["name"], name,
        "copy must preserve the complete callable name"
    );
    assert_eq!(quote["input_per_million"], "25.92");
    assert_eq!(
        quote_rate(&quote["prices_by_tier"]["default"]["base"], "input")["price_cny_per_million"],
        "25.92"
    );
}
