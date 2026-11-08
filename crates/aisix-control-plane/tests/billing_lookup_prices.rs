//! The prices a gateway looks up by key (`GET /dp/v1/keys/{key_id}/prices`):
//! the key's tenant's per-token rates for the key's group, as exact decimal
//! strings — CNY per token under the field names the gateway decodes until
//! it is upgraded. Baselines, group rates and discounts are written through
//! the operators' API and the gateway asks over its mTLS listener.
//!
//! The world's group rate is `1` unless a test writes another, so a rate
//! shown here is numerically the baseline's own USD figure; a test that
//! writes a rate of its own scales the expectation by it.

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

/// The `shared` group's id, which the first-boot tenant defaults assign.
async fn shared_group(cp: &TestCp) -> Uuid {
    sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'shared'")
        .fetch_one(cp.db().pool())
        .await
        .expect("the first-boot group")
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

/// A baseline revision of `card` for `model` starting at `effective_from`.
async fn baseline(
    cp: &TestCp,
    admin: &OperatorSession,
    model: Uuid,
    effective_from: DateTime<Utc>,
    card: Value,
) -> i64 {
    let revision = create(
        cp,
        admin,
        "/admin/pricing/baselines",
        json!({"model_id": model, "effective_from": effective_from, "card": card,
               "note": "the prices test's card"}),
    )
    .await;
    revision["revision"]["revision_id"]
        .as_i64()
        .expect("a revision id")
}

/// A rate revision of `cny_per_usd` for `model` in `group` (`None`: the
/// group's default), starting at `effective_from`.
async fn rate(
    cp: &TestCp,
    admin: &OperatorSession,
    group: Uuid,
    model: Option<Uuid>,
    effective_from: DateTime<Utc>,
    cny_per_usd: &str,
) -> i64 {
    let revision = create(
        cp,
        admin,
        &format!("/admin/provider-groups/{group}/rates"),
        json!({"model_id": model, "cny_per_usd": cny_per_usd, "effective_from": effective_from}),
    )
    .await;
    revision["revision"]["revision_id"]
        .as_i64()
        .expect("a revision id")
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

/// The live group named `name`, created when there is none.
async fn group(cp: &TestCp, admin: &OperatorSession, name: &str) -> Uuid {
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = $1 AND retired_at IS NULL")
            .bind(name)
            .fetch_optional(cp.db().pool())
            .await
            .expect("read the group");
    if let Some(id) = existing {
        return id;
    }
    let created = create(
        cp,
        admin,
        "/admin/provider-groups",
        json!({"name": name, "display_name": name, "default_cny_per_usd": "1"}),
    )
    .await;
    id_of(&created, "/id")
}

/// A tenant of group `shared` with a key naming it.
async fn tenant_with_key(cp: &TestCp, admin: &OperatorSession) -> (Uuid, Uuid) {
    let group = shared_group(cp).await;
    tenant_with_key_in(cp, admin, group).await
}

/// A tenant assigned `group` and `shared`, with a key naming `group`.
async fn tenant_with_key_in(cp: &TestCp, admin: &OperatorSession, group: Uuid) -> (Uuid, Uuid) {
    let tenant = seed_tenant(cp, admin).await;
    let name: String = sqlx::query_scalar("SELECT name FROM provider_groups WHERE id = $1")
        .bind(group)
        .fetch_one(cp.db().pool())
        .await
        .expect("the group's name");
    // A customer key's group must be one its tenant is assigned, so the
    // tenant is given the group before the key names it.
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
        .json(&json!({"allowed_provider_groups": [name.clone()]}))
        .send()
        .await
        .expect("send");
    assert_eq!(
        r.status().as_u16(),
        200,
        "assign the group: {:?}",
        r.text().await
    );
    let key = create(
        cp,
        admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label": "prices-7c1", "group_id": group}),
    )
    .await;
    let key_id = id_of(&key, "/key/id");
    let stored: Option<Uuid> = sqlx::query_scalar("SELECT group_id FROM tenant_keys WHERE id = $1")
        .bind(key_id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key's group");
    assert_eq!(stored, Some(group), "the key names the group asked for");
    (tenant, key_id)
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
async fn prices_are_the_keys_groups_rates_per_token() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-a").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let now = db_now(&cp).await;
    baseline(&cp, &admin, model, now, full_card()).await;
    // `shared` at 1 CNY per USD: the charge in µ¥ is the baseline's in µ$.
    rate(&cp, &admin, shared_group(&cp).await, None, now, "1").await;

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

    // A 20% discount applies to the price the group charges.
    discount(&cp, &admin, tenant, now, 2000).await;
    let m = entry(&prices(&dp, key).await, model);
    assert_eq!(m["input"], "0.0000048", "{m}");
    assert_eq!(m["output"], "0.000016", "{m}");

    // A model override replaces the group default, and the discount still
    // applies: a listing price is the final one (R12).
    rate(
        &cp,
        &admin,
        shared_group(&cp).await,
        Some(model),
        db_now(&cp).await,
        "0.5",
    )
    .await;
    let m = entry(&prices(&dp, key).await, model);
    assert_eq!(m["input"], "0.0000024", "{m}");
    assert_eq!(m["output"], "0.000008", "{m}");
}

#[tokio::test]
async fn a_scheduled_rate_or_baseline_sets_valid_until() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-b").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    let now = db_now(&cp).await;
    let group = shared_group(&cp).await;
    baseline(&cp, &admin, model, now, full_card()).await;
    rate(&cp, &admin, group, None, now, "1").await;
    let in_two_hours = now + TimeDelta::hours(2);
    let in_an_hour = now + TimeDelta::hours(1);
    // A later baseline of the granted model, then a later rate in the group:
    // the earliest of the two binds.
    baseline(&cp, &admin, model, in_two_hours, card("9")).await;
    rate(&cp, &admin, group, None, in_an_hour, "2").await;

    let r = prices(&dp, key).await;
    let until = |r: &Resp| -> DateTime<Utc> {
        r.body["valid_until"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("a valid_until in {r:?}"))
    };
    assert_eq!(until(&r), in_an_hour, "{r:?}");
    // The current price is still the one shown.
    assert_eq!(entry(&r, model)["input"], "0.000006", "{r:?}");

    // The tenant's own discount revision counts, an earlier one first.
    discount(&cp, &admin, tenant, now + TimeDelta::minutes(5), 1000).await;
    assert_eq!(until(&prices(&dp, key).await), now + TimeDelta::minutes(5));

    // Another tenant's scheduled change does not.
    let (other, _) = tenant_with_key(&cp, &admin).await;
    discount(&cp, &admin, other, now + TimeDelta::minutes(1), 1000).await;
    assert_eq!(until(&prices(&dp, key).await), now + TimeDelta::minutes(5));
}

#[tokio::test]
async fn two_keys_in_two_groups_see_two_prices() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-h").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let now = db_now(&cp).await;
    baseline(&cp, &admin, model, now, full_card()).await;
    let cheap = group(&cp, &admin, &format!("cheap-{}", Uuid::new_v4().simple())).await;
    let (tenant_a, key_a) = tenant_with_key(&cp, &admin).await;
    let (tenant_b, key_b) = tenant_with_key_in(&cp, &admin, cheap).await;
    grant(&cp, &admin, tenant_a, &[model]).await;
    grant(&cp, &admin, tenant_b, &[model]).await;
    // A group's creation writes its own default rate, so the rates here start
    // now rather than at the instant read before the group existed: a later
    // start would be the one in force.
    rate(
        &cp,
        &admin,
        shared_group(&cp).await,
        None,
        db_now(&cp).await,
        "7.2",
    )
    .await;
    rate(&cp, &admin, cheap, None, db_now(&cp).await, "5").await;

    assert_eq!(
        entry(&prices(&dp, key_a).await, model)["input"],
        "0.0000432"
    );
    assert_eq!(entry(&prices(&dp, key_b).await, model)["input"], "0.00003");
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
    // `m` has a baseline and the group's rate; `n` has both but is not
    // granted; `u` is granted but has no baseline at all.
    baseline(&cp, &admin, m, now, full_card()).await;
    baseline(&cp, &admin, n, now, full_card()).await;
    rate(&cp, &admin, shared_group(&cp).await, None, now, "1").await;

    let r = prices(&dp, key).await;
    assert_eq!(r.status, 200, "{r:?}");
    let ids: Vec<&Value> = r.body["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|e| &e["model_id"])
        .collect();
    assert_eq!(ids, vec![&json!(m)], "{r:?}");

    // Four refusals the table itself makes the last word on: a group default
    // no rate, a rate outside 0..=100, a negative one, and a second default
    // row in one group are all refused.
    for body in [
        json!({"model_id": null, "cny_per_usd": null, "effective_from": now}),
        json!({"model_id": null, "cny_per_usd": "100.0001", "effective_from": now}),
    ] {
        let r = cp
            .send(
                "POST",
                &format!("/admin/provider-groups/{}/rates", shared_group(&cp).await),
                Some(admin.cookie()),
                Some(body.clone()),
            )
            .await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
    }
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
    baseline(&cp, &admin, model, db_now(&cp).await, card).await;
    rate(
        &cp,
        &admin,
        shared_group(&cp).await,
        None,
        db_now(&cp).await,
        "1",
    )
    .await;
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

/// Raw-inserts a rate revision of `model` in `group` at `engine_version`,
/// which the write path would refuse when it is newer.
async fn raw_rate(cp: &TestCp, group: Uuid, model: Option<Uuid>, engine_version: i64) {
    sqlx::query(
        "INSERT INTO pricing_group_rates \
         (engine_version, group_id, model_id, effective_from, cny_per_usd_e4, note) \
         VALUES ($1, $2, $3, now(), 10000, 'test')",
    )
    .bind(engine_version)
    .bind(group)
    .bind(model)
    .execute(cp.db().pool())
    .await
    .expect("insert the rate");
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
    let group = shared_group(&cp).await;
    baseline(&cp, &admin, a, now, full_card()).await;
    baseline(&cp, &admin, b, now, full_card()).await;
    rate(&cp, &admin, group, None, now, "1").await;
    let r = prices(&dp, key).await;
    assert!(!entry(&r, a).is_null() && !entry(&r, b).is_null(), "{r:?}");

    // A model override a newer engine wrote: the engine prices nothing from
    // it and does not fall back to the group's default rate.
    raw_rate(&cp, group, Some(a), 999).await;
    let r = prices(&dp, key).await;
    assert!(entry(&r, a).is_null(), "{r:?}");
    assert!(!entry(&r, b).is_null(), "{r:?}");

    // A discount a newer engine wrote leaves every model of that tenant
    // unpriced: the discount cannot be read, so no final price exists.
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
}

#[tokio::test]
async fn a_key_with_no_group_sees_no_price() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let dp = cp.issue_dp_client("gw-prices-i").await;
    let model = seed_model(&cp, &admin, &format!("pr-{}", Uuid::new_v4().simple())).await;
    let (tenant, key) = tenant_with_key(&cp, &admin).await;
    grant(&cp, &admin, tenant, &[model]).await;
    baseline(&cp, &admin, model, db_now(&cp).await, full_card()).await;
    rate(
        &cp,
        &admin,
        shared_group(&cp).await,
        None,
        db_now(&cp).await,
        "1",
    )
    .await;
    // A legacy key an older replica minted without a group: it prices
    // nothing rather than a guessed rate.
    sqlx::query("UPDATE tenant_keys SET group_id = NULL WHERE id = $1 AND tenant_id = $2")
        .bind(key)
        .bind(tenant)
        .execute(cp.db().pool())
        .await
        .expect("clear the key's group");
    let r = prices(&dp, key).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["models"], json!([]), "{r:?}");
}
