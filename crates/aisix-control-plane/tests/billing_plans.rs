//! The operators' price views: every live model against every live group
//! (`GET /admin/pricing/models`) and each tenant's discount
//! (`GET /admin/pricing/tenants/{id}`).
//!
//! "Current" is the price a row priced now is charged at, so the first case
//! compares the view with a charge. Baselines, rates and discounts are
//! written through the operators' API, and usage arrives over
//! `POST /dp/telemetry` exactly as a gateway sends it.
//!
//! A group's rate is `1` in these tests unless one is written, so a figure
//! in CNY per million is the baseline's own USD per million times the rate.

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
               "note": "the plans test's card"}),
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

/// `GET /admin/pricing/models` as `who`: its items.
async fn price_rows(cp: &TestCp, who: &OperatorSession) -> Vec<Value> {
    let r = get(cp, who, "/admin/pricing/models").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["currency"], "CNY", "{r:?}");
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
        .map(|r| {
            r.get("revision_id")
                .and_then(Value::as_i64)
                .unwrap_or_else(|| panic!("a revision id: {r}"))
        })
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
    // Two baselines: input 1, then input 2 a moment later, so both are in
    // force now and the later one is what a row priced now is charged at.
    let now = db_now(&cp).await;
    let first = baseline(&cp, &admin, model, now, card("1")).await;
    std::thread::sleep(std::time::Duration::from_millis(10));
    let second = baseline(&cp, &admin, model, now, card("2")).await;
    assert!(first < second);
    let group = shared_group(&cp).await;
    rate(&cp, &admin, group, None, now, "1").await;

    // A row priced right after, on a key naming the group.
    let tenant = seed_tenant(&cp, &admin).await;
    let key = create(
        &cp,
        &admin,
        &format!("/admin/tenants/{tenant}/keys"),
        json!({"label": "plans-7c1", "group_id": group}),
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
    assert_eq!(
        charge["baseline_revision_id"].as_i64(),
        Some(second),
        "{r:?}"
    );
    assert_eq!(charge["group_id"], json!(group), "{r:?}");
    assert_eq!(charge["charge_micro_cny"], json!(2_000), "{r:?}");

    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(row["name"], json!(name), "{row}");
    assert_eq!(
        row["baseline"]["current"]["revision_id"],
        json!(second),
        "{row}"
    );
    assert_eq!(
        row["baseline"]["current"]["model_id"],
        json!(model),
        "{row}"
    );
    assert_eq!(
        revision_ids(&row["baseline"]["upcoming"]),
        Vec::<i64>::new(),
        "{row}"
    );
    let price = row["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["group_id"] == json!(group))
        .unwrap_or_else(|| panic!("the group's price in {row}"));
    assert_eq!(price["source"], "default", "{row}");
    assert_eq!(price["cny_per_usd"], "1", "{row}");
    // 1 CNY per USD: the baseline's USD per million is the CNY per million.
    assert_eq!(price["input_per_million_cny"], "2", "{row}");
    assert_eq!(price["output_per_million_cny"], "2", "{row}");

    // A baseline starting in an hour is upcoming, and the current one stays.
    let in_an_hour = now + TimeDelta::hours(1);
    let third = baseline(&cp, &admin, model, in_an_hour, card("3")).await;
    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(
        row["baseline"]["current"]["revision_id"],
        json!(second),
        "{row}"
    );
    assert_eq!(
        revision_ids(&row["baseline"]["upcoming"]),
        vec![third],
        "{row}"
    );
    assert_eq!(
        row["baseline"]["upcoming"][0]["effective_from"]
            .as_str()
            .and_then(|s| s.parse::<DateTime<Utc>>().ok()),
        Some(in_an_hour),
        "{row}"
    );

    // Upcoming baselines are in the order they start, and of two starting at
    // the same instant only the one that will apply is listed.
    let in_half_an_hour = now + TimeDelta::minutes(30);
    let fourth = baseline(&cp, &admin, model, in_half_an_hour, card("4")).await;
    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(
        revision_ids(&row["baseline"]["upcoming"]),
        vec![fourth, third],
        "{row}"
    );
    let fifth = baseline(&cp, &admin, model, in_half_an_hour, card("5")).await;
    let row = row_of(&price_rows(&cp, &viewer).await, model);
    assert_eq!(
        revision_ids(&row["baseline"]["upcoming"]),
        vec![fifth, third],
        "{row}"
    );
}

#[tokio::test]
async fn the_model_price_list_shows_each_groups_cny_per_million() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let model = seed_model(&cp, &admin, &format!("gp-{}", Uuid::new_v4().simple())).await;
    // Baseline 4 input and 20 output USD per million.
    let now = db_now(&cp).await;
    baseline(
        &cp,
        &admin,
        model,
        now,
        json!({"default": {"base": {"input": "4", "output": "20"}}}),
    )
    .await;
    let group = shared_group(&cp).await;
    rate(&cp, &admin, group, None, now, "7.2").await;
    // A model override at 6 CNY per USD overrides the group's 7.2.
    let overridden = rate(&cp, &admin, group, Some(model), db_now(&cp).await, "6").await;

    let row = row_of(&price_rows(&cp, &admin).await, model);
    let price = row["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["group_id"] == json!(group))
        .unwrap_or_else(|| panic!("the group's price in {row}"));
    assert_eq!(price["cny_per_usd"], "6", "{row}");
    assert_eq!(price["source"], "model", "{row}");
    // 4 × 7.2 is 28.8, and 20 × 7.2 is 144; the model's own 6 gives 24 and
    // 120. The override in force is the one shown.
    assert_eq!(price["input_per_million_cny"], "24", "{row}");
    assert_eq!(price["output_per_million_cny"], "120", "{row}");
    let _ = overridden;

    // Each live group appears, with its own rate for the model.
    let cheap = create(
        &cp,
        &admin,
        "/admin/provider-groups",
        json!({"name": format!("cheap-{}", Uuid::new_v4().simple()), "display_name": "便宜",
               "default_cny_per_usd": "5"}),
    )
    .await;
    let cheap_id = id_of(&cheap, "/id");
    let row = row_of(&price_rows(&cp, &admin).await, model);
    let price = row["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .find(|g| g["group_id"] == json!(cheap_id))
        .unwrap_or_else(|| panic!("the cheap group's price in {row}"));
    assert_eq!(price["cny_per_usd"], "5", "{row}");
    assert_eq!(price["source"], "default", "{row}");
    assert_eq!(price["input_per_million_cny"], "20", "{row}");
    assert_eq!(price["output_per_million_cny"], "100", "{row}");
    // The display name is what the price list shows, never the routing name.
    assert_eq!(price["display_name"], "便宜", "{row}");
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
    baseline(&cp, &admin, m, now, card("1")).await;
    let later = baseline(&cp, &admin, u, now + TimeDelta::hours(1), card("1")).await;
    rate(&cp, &admin, shared_group(&cp).await, None, now, "1").await;

    let rows = price_rows(&cp, &admin).await;
    let order: Vec<Value> = rows.iter().map(|r| r["model_id"].clone()).collect();
    assert_eq!(order, vec![json!(u), json!(n), json!(m)], "{rows:?}");
    assert_eq!(rows[0]["baseline"]["current"], Value::Null, "{rows:?}");
    assert_eq!(
        revision_ids(&rows[0]["baseline"]["upcoming"]),
        vec![later],
        "{rows:?}"
    );
    assert_eq!(rows[1]["baseline"]["current"], Value::Null, "{rows:?}");
    assert_eq!(
        revision_ids(&rows[1]["baseline"]["upcoming"]),
        Vec::<i64>::new()
    );
    assert!(rows[2]["baseline"]["current"].is_object(), "{rows:?}");
    // The row order after the delete is the same: the two that cannot be
    // priced now first, the one that can last.

    // A deleted model is not listed, though its baseline outlives it.
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
async fn a_tenant_plan_has_a_discount_and_no_overrides() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let tenant = seed_tenant(&cp, &admin).await;
    let other = seed_tenant(&cp, &admin).await;
    let now = db_now(&cp).await;
    // Another tenant's discount is not this tenant's plan.
    discount(&cp, &admin, other, now, 9000).await;

    let path = format!("/admin/pricing/tenants/{tenant}");
    let r = get(&cp, &viewer, &path).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body,
        json!({
            "tenant_id": tenant,
            "discount": {"current": null, "upcoming": []},
        }),
        "{r:?}"
    );
    // There is no per-tenant price any more: the plan carries no overrides.
    assert!(r.body.get("overrides").is_none(), "{r:?}");

    let current = discount(&cp, &admin, tenant, now, 2000).await;
    let later = discount(&cp, &admin, tenant, now + TimeDelta::hours(1), 3000).await;
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
}

#[tokio::test]
async fn the_sell_routes_are_gone() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    for (method, path) in [
        ("GET", "/admin/pricing/sell"),
        ("POST", "/admin/pricing/sell"),
    ] {
        let body = (method == "POST").then(
            || json!({"model_id": Uuid::new_v4(), "effective_from": Utc::now(), "card": card("1")}),
        );
        let r = cp.send(method, path, Some(admin.cookie()), body).await;
        assert_eq!(r.status, 404, "{method} {path}: {r:?}");
    }
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
