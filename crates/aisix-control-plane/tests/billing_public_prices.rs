//! 公开价格页使用真实控制面、Postgres 和管理员正常写入路径。

mod support;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::pricing::store::{self, NewDiscount};
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use support::world::{
    db_now, insert_baseline, public_model, seed_model, seed_provider_key, seed_tenant,
};
use support::{OperatorSession, TestCp};
use uuid::Uuid;

async fn link(cp: &TestCp, admin: &OperatorSession, account: Uuid, model: Uuid) {
    let r = cp
        .send(
            "PUT",
            &format!("/admin/provider-keys/{account}/channel-models/{model}"),
            Some(admin.cookie()),
            Some(json!({"upstream_model":"private-upstream-price-test"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

async fn create_group(cp: &TestCp, admin: &OperatorSession, name: &str, rate: &str) -> Uuid {
    let r = cp
        .send(
            "POST",
            "/admin/provider-groups",
            Some(admin.cookie()),
            Some(json!({"name":name,"display_name":name,
                        "default_cny_per_usd":rate})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    Uuid::parse_str(r.body["id"].as_str().expect("group id")).expect("uuid")
}

async fn make_default(cp: &TestCp, admin: &OperatorSession, group: Uuid) {
    let r = cp
        .send(
            "POST",
            &format!("/admin/provider-groups/{group}/make-default"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

async fn signup_groups(cp: &TestCp, admin: &OperatorSession, names: &[&str]) {
    let r = cp
        .send(
            "PUT",
            "/admin/settings/tenant-defaults",
            Some(admin.cookie()),
            Some(json!({"default_provider_groups":names})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
}

async fn baseline(cp: &TestCp, model: Uuid, at: DateTime<Utc>, input: &str) {
    let card = serde_json::from_value(json!({
        "default": {
            "base": {"input": input, "output": "20", "cache_read": "0.125", "cache_write": "0"},
            "long_context":[{"above_input_tokens":200000,"rates":{"input":"8","output":"30",
                           "cache_read":"0.25","cache_write":"0"}}]
        },
        "batch": {"base":{"input":"2","output":"10","cache_read":"0.125","cache_write":"0"},
                  "long_context":[{"above_input_tokens":200000,"rates":{"input":"4","output":"15","cache_read":"0.25","cache_write":"0"}}]}
    }))
    .expect("valid card");
    insert_baseline(cp, model, at, card)
        .await
        .expect("baseline");
}

fn model(body: &Value, id: Uuid) -> &Value {
    body["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|row| row["id"] == json!(id))
        .expect("public model")
}

#[tokio::test]
async fn anonymous_prices_show_only_live_public_models_at_the_undiscounted_default_rate() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let group = create_group(&cp, &admin, "private-price-test-group", "7.2").await;
    let internal = create_group(&cp, &admin, "legacy-internal-group", "2.1").await;
    make_default(&cp, &admin, internal).await;
    let account = seed_provider_key(&cp, &admin).await;
    let priced = public_model(&cp, &admin, "release-price-demo").await;
    let unpriced = public_model(&cp, &admin, "unpriced-demo").await;
    let draft = public_model(&cp, &admin, "never-published-demo").await;
    let retired = public_model(&cp, &admin, "retired-demo").await;
    let direct = seed_model(&cp, &admin).await;
    for id in [priced, unpriced, retired] {
        link(&cp, &admin, account, id).await;
    }
    for id in [priced, retired, direct] {
        baseline(&cp, id, db_now(&cp).await, "4").await;
    }

    let r = cp
        .send(
            "DELETE",
            &format!("/admin/provider-keys/{account}/channel-models/{retired}"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = cp
        .send(
            "POST",
            &format!("/admin/public-models/{retired}/retire"),
            Some(admin.cookie()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    let tenant = seed_tenant(&cp, &admin).await;
    let who = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    signup_groups(&cp, &admin, &["private-price-test-group"]).await;
    let effective_from = db_now(&cp).await;
    cp.db()
        .tx(move |conn| {
            async move {
                store::insert_discount(
                    conn,
                    &NewDiscount {
                        tenant_id: tenant,
                        effective_from,
                        discount_bps: 2500,
                    },
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("private discount");

    let anonymous = cp.send("GET", "/api/public/prices", None, None).await;
    assert_eq!(anonymous.status, 200, "{anonymous:?}");
    assert_eq!(anonymous.body["currency"], "CNY");
    assert_eq!(anonymous.body["available"], true);
    assert!(anonymous.set_cookie.is_none());
    assert_eq!(
        anonymous.body["models"].as_array().expect("models").len(),
        2
    );
    let shown = model(&anonymous.body, priced);
    assert_eq!(shown["name"], "release-price-demo");
    assert_eq!(shown["input_per_million"], "28.8");
    assert_eq!(shown["output_per_million"], "144");
    assert_eq!(shown["cache_read_per_million"], "0.9");
    assert_eq!(
        shown["prices_by_tier"]["default"]["long_context"][0]["above_input_tokens"],
        200000,
    );
    assert_eq!(
        shown["prices_by_tier"]["batch"]["base"][0]["price_cny_per_million"],
        "14.4",
    );
    let explicit_zero = shown["prices_by_tier"]["default"]["base"]
        .as_array()
        .expect("base rates")
        .iter()
        .find(|rate| rate["dimension"] == "cache_write")
        .expect("configured free cache write");
    assert_eq!(explicit_zero["price_cny_per_million"], "0");
    let missing = model(&anonymous.body, unpriced);
    assert_eq!(
        missing,
        &json!({"id":unpriced,"name":"unpriced-demo","input_per_million":null,
                "output_per_million":null,"cache_read_per_million":null,"prices_by_tier":null}),
    );
    let raw = anonymous.body.to_string();
    for private in [
        draft.to_string(),
        retired.to_string(),
        direct.to_string(),
        account.to_string(),
        tenant.to_string(),
        group.to_string(),
        internal.to_string(),
        "discount_bps".to_owned(),
        "private-upstream-price-test".to_owned(),
        "private-price-test-group".to_owned(),
        "legacy-internal-group".to_owned(),
    ] {
        assert!(!raw.contains(&private), "{private}: {raw}");
    }

    let member = cp
        .send("GET", "/api/public/prices", Some(who.cookie()), None)
        .await;
    assert_eq!(member.status, 200, "{member:?}");
    assert_eq!(member.body["models"], anonymous.body["models"]);
    let private = cp.send("GET", "/api/prices", None, None).await;
    assert_eq!(private.status, 401, "{private:?}");

    // 平台为旧密钥设置的默认分组没有分配给新租户，不能发布其价格。
    let second = create_group(&cp, &admin, "second-signup-group", "3").await;
    signup_groups(
        &cp,
        &admin,
        &["private-price-test-group", "second-signup-group"],
    )
    .await;
    let no_group = cp.send("GET", "/api/public/prices", None, None).await;
    assert_eq!(no_group.status, 200, "{no_group:?}");
    assert_eq!(no_group.body["available"], false);
    assert_eq!(
        model(&no_group.body, priced)["input_per_million"],
        Value::Null
    );

    make_default(&cp, &admin, second).await;
    let designated_signup_group = cp.send("GET", "/api/public/prices", None, None).await;
    assert_eq!(
        designated_signup_group.status, 200,
        "{designated_signup_group:?}"
    );
    assert_eq!(designated_signup_group.body["available"], true);
    assert_eq!(
        model(&designated_signup_group.body, priced)["input_per_million"],
        "12"
    );
}

#[tokio::test]
async fn a_scheduled_price_bounds_the_browser_cache() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    create_group(&cp, &admin, "private-price-test-group", "7.2").await;
    signup_groups(&cp, &admin, &["private-price-test-group"]).await;
    let account = seed_provider_key(&cp, &admin).await;
    let priced = public_model(&cp, &admin, "scheduled-demo").await;
    link(&cp, &admin, account, priced).await;
    let now = db_now(&cp).await;
    baseline(&cp, priced, now, "4").await;
    let next = now + TimeDelta::seconds(8);
    baseline(&cp, priced, next, "5").await;

    let r = cp.send("GET", "/api/public/prices", None, None).await;
    assert_eq!(r.status, 200, "{r:?}");
    let as_of: DateTime<Utc> = r.body["as_of"]
        .as_str()
        .expect("snapshot time")
        .parse()
        .expect("RFC3339");
    assert!(
        as_of < next,
        "the fixture should still have a scheduled revision"
    );
    assert_eq!(model(&r.body, priced)["input_per_million"], "28.8");
    let cache_control = r
        .headers
        .get("cache-control")
        .expect("shared cache policy")
        .to_str()
        .expect("ASCII");
    assert!(cache_control.contains("public"));
    let age: i64 = cache_control
        .split(',')
        .find_map(|value| value.trim().strip_prefix("max-age="))
        .expect("bounded max-age")
        .parse()
        .expect("whole seconds");
    assert!(
        (0..=(next - as_of).num_seconds()).contains(&age),
        "{cache_control}"
    );
}
