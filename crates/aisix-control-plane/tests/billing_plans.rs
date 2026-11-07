//! The operators' price views: each live model's current and upcoming
//! platform price (`GET /admin/pricing/models`) and each tenant's plan, its
//! discount and its overrides (`GET /admin/pricing/tenants/{id}`).
//!
//! "Current" is the price a row priced now is charged at, so the first case
//! compares the view with a charge. Prices are written through the
//! operators' API, and usage arrives over `POST /dp/telemetry` exactly as a
//! gateway sends it.

mod support;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::tenants::TenantStatus;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};
use support::{OperatorSession, Resp, TestCp};
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

/// `GET /admin/pricing/models` as `who`: its items.
async fn price_rows(cp: &TestCp, who: &OperatorSession) -> Vec<Value> {
    let r = get(cp, who, "/admin/pricing/models").await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

/// The row of `model` in `rows`, which must hold exactly one.
fn row_of(rows: &[Value], model: Uuid) -> Value {
    let found: Vec<&Value> = rows
        .iter()
        .filter(|r| r["model_id"] == json!(model))
        .collect();
    assert_eq!(found.len(), 1, "one row of {model} in {rows:?}");
    found[0].clone()
}

/// The `revision_id` of each revision in `list`, in order.
fn revision_ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .unwrap_or_else(|| panic!("a list: {list}"))
        .iter()
        .map(|r| r["revision_id"].as_i64().expect("a revision id"))
        .collect()
}

#[tokio::test]
async fn the_current_price_shown_is_the_one_a_row_is_charged_at() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let name = format!("plans-{}", Uuid::new_v4().simple());
    let model = seed_model(&cp, &admin, &name).await;
    // Two platform prices starting at the same instant.
    let now = db_now(&cp).await;
    let first = sell(&cp, &admin, model, None, now, card("1")).await;
    let second = sell(&cp, &admin, model, None, now, card("2")).await;
    assert_ne!(first, second);

    // A row priced right after.
    let tenant = seed_tenant(&cp, &admin).await;
    let key = create(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label": "plans-7c1"}),
    )
    .await;
    let key = key["key"]["id"].as_str().expect("a key id").to_string();
    let dp = cp.issue_dp_client("gw-plans-7c1").await;
    let event = json!({
        "event_id": "e-plans-current",
        "request_id": "req-plans-current",
        "occurred_at": Utc::now().to_rfc3339(),
        "team_id": tenant.to_string(),
        "api_key_id": key,
        "requested_model_id": model.to_string(),
        "operation": "chat",
        "status_code": 200,
        "prompt_tokens": 1000,
        "completion_tokens": 0,
    });
    let r = dp
        .telemetry(Uuid::new_v4(), &json!({"events": [event]}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = get(
        &cp,
        &admin,
        "/admin/pricing/charges?event_id=e-plans-current",
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let charge = &r.body["items"][0];
    assert_eq!(charge["status"], "charged", "{r:?}");
    let charged_at = charge["sell_revision_id"]
        .as_i64()
        .unwrap_or_else(|| panic!("a sell revision on the charge: {r:?}"));

    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(row["name"], json!(name), "{row}");
    assert_eq!(row["current"]["revision_id"], json!(charged_at), "{row}");
    assert_eq!(row["current"]["model_id"], json!(model), "{row}");
    assert_eq!(row["current"]["tenant_id"], Value::Null, "{row}");
    assert_eq!(revision_ids(&row["upcoming"]), Vec::<i64>::new(), "{row}");

    // A price starting in an hour is upcoming, and the current one stays.
    let in_an_hour = now + TimeDelta::hours(1);
    let third = sell(&cp, &admin, model, None, in_an_hour, card("3")).await;
    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(row["current"]["revision_id"], json!(charged_at), "{row}");
    assert_eq!(revision_ids(&row["upcoming"]), vec![third], "{row}");
    assert_eq!(
        row["upcoming"][0]["effective_from"]
            .as_str()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok()),
        Some(in_an_hour),
        "{row}"
    );

    // Upcoming prices are in the order they start, and of two starting at
    // the same instant only the one that will apply is listed.
    let in_half_an_hour = now + TimeDelta::minutes(30);
    let fourth = sell(&cp, &admin, model, None, in_half_an_hour, card("4")).await;
    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(revision_ids(&row["upcoming"]), vec![fourth, third], "{row}");
    let fifth = sell(&cp, &admin, model, None, in_half_an_hour, card("5")).await;
    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(revision_ids(&row["upcoming"]), vec![fifth, third], "{row}");
}

#[tokio::test]
async fn unpriced_models_come_first() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let suffix = Uuid::new_v4().simple().to_string();
    // By name alone, M would come first and N last.
    let m = seed_model(&cp, &admin, &format!("aa-priced-{suffix}")).await;
    let u = seed_model(&cp, &admin, &format!("bb-upcoming-{suffix}")).await;
    let n = seed_model(&cp, &admin, &format!("zz-unpriced-{suffix}")).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, m, None, now, card("1")).await;
    let later = sell(&cp, &admin, u, None, now + TimeDelta::hours(1), card("1")).await;

    let rows = price_rows(&cp, &admin).await;
    let order: Vec<Value> = rows.iter().map(|r| r["model_id"].clone()).collect();
    assert_eq!(order, vec![json!(u), json!(n), json!(m)], "{rows:?}");
    assert_eq!(rows[0]["current"], Value::Null, "{rows:?}");
    assert_eq!(revision_ids(&rows[0]["upcoming"]), vec![later], "{rows:?}");
    assert_eq!(rows[1]["current"], Value::Null, "{rows:?}");
    assert_eq!(revision_ids(&rows[1]["upcoming"]), Vec::<i64>::new());
    assert!(rows[2]["current"].is_object(), "{rows:?}");

    // A deleted model is not listed, though its price outlives it.
    let deleted = seed_model(&cp, &admin, &format!("cc-deleted-{suffix}")).await;
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM gw_resources WHERE kind = 'models' AND id = $1")
            .bind(deleted)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the model's version");
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let status = client
        .delete(cp.url(&format!("/admin/resources/models/{deleted}")))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .send()
        .await
        .expect("send the delete")
        .status();
    assert_eq!(status, 200, "delete the model");
    let rows = price_rows(&cp, &admin).await;
    let order: Vec<Value> = rows.iter().map(|r| r["model_id"].clone()).collect();
    assert_eq!(order, vec![json!(u), json!(n), json!(m)], "{rows:?}");
}

#[tokio::test]
async fn a_tenant_plan_shows_its_discount_and_overrides() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let tenant = seed_tenant(&cp, &admin).await;
    let other = seed_tenant(&cp, &admin).await;
    let m = seed_model(&cp, &admin, &format!("m-{}", Uuid::new_v4().simple())).await;
    let n = seed_model(&cp, &admin, &format!("n-{}", Uuid::new_v4().simple())).await;
    let now = db_now(&cp).await;
    sell(&cp, &admin, m, None, now, card("1")).await;
    sell(&cp, &admin, n, None, now, card("1")).await;
    // Another tenant's override and discount are not this tenant's plan.
    sell(&cp, &admin, n, Some(other), now, card("5")).await;
    discount(&cp, &admin, other, now, 9000).await;

    let path = format!("/admin/pricing/tenants/{tenant}");
    let r = get(&cp, &viewer, &path).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body,
        json!({
            "tenant_id": tenant,
            "discount": {"current": null, "upcoming": []},
            "overrides": [],
        }),
        "{r:?}"
    );

    let current = discount(&cp, &admin, tenant, now, 2000).await;
    let later = discount(&cp, &admin, tenant, now + TimeDelta::hours(1), 3000).await;
    let r#override = sell(&cp, &admin, m, Some(tenant), now, card("3")).await;
    let r = get(&cp, &viewer, &path).await;
    assert_eq!(r.status, 200, "{r:?}");
    let plan = &r.body;
    assert_eq!(plan["tenant_id"], json!(tenant), "{plan}");
    assert_eq!(
        plan["discount"]["current"]["revision_id"],
        json!(current),
        "{plan}"
    );
    assert_eq!(plan["discount"]["current"]["discount_bps"], 2000, "{plan}");
    assert_eq!(
        revision_ids(&plan["discount"]["upcoming"]),
        vec![later],
        "{plan}"
    );
    let overrides = plan["overrides"].as_array().expect("overrides");
    assert_eq!(overrides.len(), 1, "{plan}");
    assert_eq!(overrides[0]["model_id"], json!(m), "{plan}");
    assert_eq!(
        overrides[0]["current"]["revision_id"],
        json!(r#override),
        "{plan}"
    );
    assert_eq!(
        overrides[0]["current"]["tenant_id"],
        json!(tenant),
        "{plan}"
    );
    assert!(overrides[0]["current"]["card"].is_object(), "{plan}");
    assert_eq!(revision_ids(&overrides[0]["upcoming"]), Vec::<i64>::new());

    // Ending the override: the row stays, with no card.
    let ended = sell(&cp, &admin, m, Some(tenant), db_now(&cp).await, Value::Null).await;
    let r = get(&cp, &viewer, &path).await;
    assert_eq!(r.status, 200, "{r:?}");
    let overrides = r.body["overrides"].as_array().expect("overrides").clone();
    assert_eq!(overrides.len(), 1, "{r:?}");
    assert_eq!(
        overrides[0]["current"]["revision_id"],
        json!(ended),
        "{r:?}"
    );
    assert_eq!(overrides[0]["current"]["card"], Value::Null, "{r:?}");
}

#[tokio::test]
async fn only_operators_read_price_views() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let plan = format!("/admin/pricing/tenants/{tenant}");
    for path in ["/admin/pricing/models", plan.as_str()] {
        let r = get(&cp, &viewer, path).await;
        assert_eq!(r.status, 200, "{path} as a viewer: {r:?}");
        let r = cp.send("GET", path, Some(member.cookie()), None).await;
        assert_eq!(
            (r.status, r.body["code"].clone()),
            (401, json!("unauthenticated")),
            "{path} as a member: {r:?}"
        );
        let r = cp.send("GET", path, None, None).await;
        assert_eq!(r.status, 401, "{path} without a session: {r:?}");
    }
    for unknown in [Uuid::new_v4().to_string(), "not-a-tenant".to_string()] {
        let r = get(&cp, &viewer, &format!("/admin/pricing/tenants/{unknown}")).await;
        assert_eq!(
            (r.status, r.body["code"].clone()),
            (404, json!("not_found")),
            "{unknown}: {r:?}"
        );
    }
}
