//! The prices a gateway looks up by key (`GET /dp/v1/keys/{key_id}/prices`):
//! the key's tenant's effective per-token rates, as exact decimal strings in
//! USD per token. Prices and grants are written through the operators' API
//! and the gateway asks over its mTLS listener.

mod support;

use aisix_control_plane::auth::OperatorRole;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use support::{DpClient, OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// The database's clock: what a price write compares `effective_from` with.
async fn db_now(cp: &TestCp) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(cp.db().pool())
        .await
        .expect("read now()")
}

/// A card charging `input` USD per million input tokens.
fn card(input: &str) -> Value {
    json!({"default": {"base": {"input": input, "output": "2"}}})
}

/// `GET path` as `who`.
async fn get(cp: &TestCp, who: &OperatorSession, path: &str) -> Resp {
    cp.send("GET", path, Some(who.cookie()), None).await
}

/// `POST path` as `admin`, which must answer 201.
async fn create(cp: &TestCp, admin: &OperatorSession, path: &str, body: Value) -> Value {
    let r = cp
        .send("POST", path, Some(admin.cookie()), Some(body))
        .await;
    assert_eq!(r.status, 201, "POST {path}: {r:?}");
    r.body
}

fn id_of(v: &Value, pointer: &str) -> Uuid {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(|| panic!("a uuid at {pointer} in {v}"))
}

/// A live model named `name`, on a provider key of its own.
async fn seed_model(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let pk = create(
        cp,
        admin,
        "/admin/resources/provider_keys",
        json!({"doc": {
            "display_name": format!("acct-plans-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "api_key": "sk-plans-upstream-7c1",
            "groups": ["shared"],
        }}),
    )
    .await;
    let model = create(
        cp,
        admin,
        "/admin/resources/models",
        json!({"doc": {
            "display_name": name,
            "provider": "openai",
            "model_name": "gpt-4o-mini",
            "provider_key_id": id_of(&pk, "/id"),
        }}),
    )
    .await;
    id_of(&model, "/id")
}

async fn seed_tenant(cp: &TestCp, admin: &OperatorSession) -> Uuid {
    let tenant = create(
        cp,
        admin,
        "/admin/tenants",
        json!({"name": format!("tenant-{}", Uuid::new_v4().simple())}),
    )
    .await;
    id_of(&tenant, "/id")
}

/// A sell price: the platform's for `model`, or with `tenant` that tenant's
/// override; `card` null ends the override.
async fn sell(
    cp: &TestCp,
    admin: &OperatorSession,
    model: Uuid,
    tenant: Option<Uuid>,
    effective_from: DateTime<Utc>,
    card: Value,
) -> i64 {
    let mut body = json!({"model_id": model, "effective_from": effective_from, "card": card});
    if let Some(tenant) = tenant {
        body["tenant_id"] = json!(tenant);
    }
    let revision = create(cp, admin, "/admin/pricing/sell", body).await;
    revision["revision_id"].as_i64().expect("a revision id")
}

async fn discount(
    cp: &TestCp,
    admin: &OperatorSession,
    tenant: Uuid,
    effective_from: DateTime<Utc>,
    bps: u32,
) -> i64 {
    let revision = create(
        cp,
        admin,
        "/admin/pricing/discounts",
        json!({"tenant_id": tenant, "effective_from": effective_from, "discount_bps": bps}),
    )
    .await;
    revision["revision_id"].as_i64().expect("a revision id")
}

/// A card with every rate of the brief's example.
fn full_card() -> Value {
    json!({"default": {"base": {"input": "6", "output": "20", "cache_read": "0.6"}}})
}

/// Grants `models` to `tenant`.
async fn grant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, models: &[Uuid]) {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the tenant");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client")
        .patch(cp.url(&format!("/admin/tenants/{tenant}")))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_model_ids": models}))
        .send()
        .await
        .expect("send");
    assert_eq!(r.status().as_u16(), 200, "grant: {:?}", r.text().await);
}

/// A tenant with a key.
async fn tenant_with_key(cp: &TestCp, admin: &OperatorSession) -> (Uuid, Uuid) {
    let tenant = seed_tenant(cp, admin).await;
    let key = create(
        cp,
        admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label": "prices-7c1"}),
    )
    .await;
    (tenant, id_of(&key, "/key/id"))
}

async fn prices(dp: &DpClient, key: Uuid) -> Resp {
    dp.send("GET", &format!("/dp/v1/keys/{key}/prices"), None)
        .await
}

/// The entry of `model` in an answer.
fn entry(r: &Resp, model: Uuid) -> Value {
    r.body["models"]
        .as_array()
        .unwrap_or_else(|| panic!("models in {r:?}"))
        .iter()
        .find(|m| m["model_id"] == json!(model))
        .cloned()
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn prices_are_the_tenants_effective_rates_per_token() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-a").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, model, None, now, full_card()).await;

    let r = prices(&dp, key).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["tenant_id"], json!(tenant));
    assert_eq!(r.body["valid_until"], Value::Null, "{r:?}");
    let m = entry(&r, model);
    assert_eq!(m["input"], "0.000006", "{r:?}");
    assert_eq!(m["output"], "0.00002", "{r:?}");
    assert_eq!(m["input_cache_read"], "0.0000006", "{r:?}");
    assert!(m.get("input_cache_write").is_none(), "{m}");
    assert!(m.get("input_cache_write_1h").is_none(), "{m}");

    // A 20% discount applies to the platform card.
    discount(&cp, &admin, tenant, now, 2000).await;
    let m = entry(&prices(&dp, key).await, model);
    assert_eq!(m["input"], "0.0000048", "{m}");
    assert_eq!(m["output"], "0.000016", "{m}");

    // An override replaces the card, and the discount is not applied to it.
    sell(
        &cp,
        &admin,
        model,
        Some(tenant),
        now,
        json!({"default": {"base": {"input": "3", "output": "4"}}}),
    )
    .await;
    let m = entry(&prices(&dp, key).await, model);
    assert_eq!(m["input"], "0.000003", "{m}");
    assert_eq!(m["output"], "0.000004", "{m}");
    assert!(m.get("input_cache_read").is_none(), "{m}");
}

#[tokio::test]
async fn a_scheduled_price_sets_valid_until() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-b").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, model, None, now, full_card()).await;
    let in_two_hours = now + TimeDelta::hours(2);
    let in_an_hour = now + TimeDelta::hours(1);
    sell(&cp, &admin, model, None, in_two_hours, card("9")).await;

    let r = prices(&dp, key).await;
    let until = |r: &Resp| -> DateTime<Utc> {
        r.body["valid_until"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("a valid_until in {r:?}"))
    };
    assert_eq!(until(&r), in_two_hours, "{r:?}");
    // The current price is still the one shown.
    assert_eq!(entry(&r, model)["input"], "0.000006", "{r:?}");

    // The tenant's own discount revision counts, an earlier one first.
    discount(&cp, &admin, tenant, in_an_hour, 1000).await;
    assert_eq!(until(&prices(&dp, key).await), in_an_hour);

    // Another tenant's scheduled change does not.
    let (other, _) = tenant_with_key(&cp, &admin).await;
    discount(&cp, &admin, other, now + TimeDelta::minutes(5), 1000).await;
    assert_eq!(until(&prices(&dp, key).await), in_an_hour);
}

#[tokio::test]
async fn an_ungranted_or_unpriced_model_is_absent_from_prices() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-c").await;
    let tag = Uuid::new_v4().simple();
    let m = seed_model(&cp, &admin, &format!("pr-m-{tag}")).await;
    let n = seed_model(&cp, &admin, &format!("pr-n-{tag}")).await;
    let u = seed_model(&cp, &admin, &format!("pr-u-{tag}")).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[m, u]).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, m, None, now, full_card()).await;
    sell(&cp, &admin, n, None, now, full_card()).await;

    let r = prices(&dp, key).await;
    assert_eq!(r.status, 200, "{r:?}");
    let ids: Vec<&Value> = r.body["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|e| &e["model_id"])
        .collect();
    assert_eq!(ids, vec![&json!(m)], "{r:?}");
}

#[tokio::test]
async fn a_tiered_card_shows_its_default_base_rates() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-d").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let card = json!({
        "default": {
            "base": {"input": "2", "output": "8", "cache_read": "0.2",
                     "cache_write": "2.5", "cache_write_1h": "4"},
            "long_context": [{"above_input_tokens": 200000,
                              "rates": {"input": "4", "output": "12", "cache_read": "0.4",
                                        "cache_write": "5", "cache_write_1h": "8"}}],
        },
        "priority": {
            "base": {"input": "5", "output": "20", "cache_read": "0.5",
                     "cache_write": "6", "cache_write_1h": "9"},
            "long_context": [{"above_input_tokens": 200000,
                              "rates": {"input": "6", "output": "24", "cache_read": "0.6",
                                        "cache_write": "7", "cache_write_1h": "10"}}],
        },
    });
    sell(&cp, &admin, model, None, db_now(&cp).await, card).await;
    let m = entry(&prices(&dp, key).await, model);
    assert_eq!(m["input"], "0.000002", "{m}");
    assert_eq!(m["output"], "0.000008", "{m}");
    assert_eq!(m["input_cache_read"], "0.0000002", "{m}");
    assert_eq!(m["input_cache_write"], "0.0000025", "{m}");
    assert_eq!(m["input_cache_write_1h"], "0.000004", "{m}");
}

#[tokio::test]
async fn an_unknown_or_revoked_key_is_not_found() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-e").await;
    let (_, key) = tenant_with_key(&cp, &admin).await;
    assert_eq!(prices(&dp, key).await.status, 200);
    assert_eq!(prices(&dp, Uuid::new_v4()).await.status, 404);
    let r = dp.send("GET", "/dp/v1/keys/not-a-uuid/prices", None).await;
    assert_eq!(r.status, 404, "{r:?}");
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1")
        .bind(key)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("a client")
        .delete(cp.url(&format!("/admin/keys/{key}")))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .send()
        .await
        .expect("send");
    assert!(r.status().is_success(), "{r:?}");
    assert_eq!(prices(&dp, key).await.status, 404);
}

#[tokio::test]
async fn the_prices_lookup_is_served_only_on_the_gateway_listener() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let (_, key) = tenant_with_key(&cp, &admin).await;
    let r = get(&cp, &admin, &format!("/dp/v1/keys/{key}/prices")).await;
    assert_eq!(r.status, 404, "{r:?}");
}

/// Raw-inserts an override of `model` for `tenant` at engine version
/// `engine_version`, which the write path would refuse when it is newer.
async fn raw_override(
    cp: &TestCp,
    model: Uuid,
    tenant: Uuid,
    engine_version: i64,
    card: Option<Value>,
) {
    sqlx::query(
        "INSERT INTO pricing_sell_revisions \
         (engine_version, model_id, tenant_id, effective_from, card, source) \
         VALUES ($1, $2, $3, now(), $4, 'test')",
    )
    .bind(engine_version)
    .bind(model)
    .bind(tenant)
    .bind(card)
    .execute(cp.db().pool())
    .await
    .expect("insert the override");
}

#[tokio::test]
async fn an_ended_override_gives_way_to_the_discounted_platform_price() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-f").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, model, None, now, full_card()).await;
    discount(&cp, &admin, tenant, now, 2000).await;
    sell(&cp, &admin, model, Some(tenant), now, card("3")).await;
    assert_eq!(entry(&prices(&dp, key).await, model)["input"], "0.000003");
    // A later revision ends the override.
    raw_override(&cp, model, tenant, 1, None).await;
    let m = entry(&prices(&dp, key).await, model);
    assert_eq!(m["input"], "0.0000048", "{m}");
}

#[tokio::test]
async fn an_unreadable_revision_in_force_leaves_the_model_unpriced() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-g").await;
    let tag = Uuid::new_v4().simple();
    let a = seed_model(&cp, &admin, &format!("pr-a-{tag}")).await;
    let b = seed_model(&cp, &admin, &format!("pr-b-{tag}")).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[a, b]).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, a, None, now, full_card()).await;
    sell(&cp, &admin, b, None, now, full_card()).await;
    let r = prices(&dp, key).await;
    assert!(!entry(&r, a).is_null() && !entry(&r, b).is_null(), "{r:?}");

    // An override a newer engine wrote: the engine prices nothing from it
    // and does not fall back to the platform card.
    raw_override(&cp, a, tenant, 999, Some(card("1"))).await;
    let r = prices(&dp, key).await;
    assert!(entry(&r, a).is_null(), "{r:?}");
    assert!(!entry(&r, b).is_null(), "{r:?}");

    // A discount a newer engine wrote leaves platform-priced models
    // unpriced, and an override (never discounted) stays priced.
    sqlx::query(
        "INSERT INTO pricing_discount_revisions (engine_version, tenant_id, effective_from, \
         discount_bps) VALUES (999, $1, now(), 1000)",
    )
    .bind(tenant)
    .execute(cp.db().pool())
    .await
    .expect("insert the discount");
    let r = prices(&dp, key).await;
    assert!(entry(&r, b).is_null(), "{r:?}");
    sell(&cp, &admin, b, Some(tenant), db_now(&cp).await, card("3")).await;
    assert_eq!(entry(&prices(&dp, key).await, b)["input"], "0.000003");
}
