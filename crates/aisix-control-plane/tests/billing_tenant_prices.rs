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
            "output_per_million"
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
