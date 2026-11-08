//! Provider groups as data, and their effective-dated CNY-per-USD rates.
//!
//! A group carries a routing name (immutable, operator-only), a display name
//! and a description (what tenants see), and a default rate. Its rates are
//! per (group, model), effective-dated and append-only, with the group
//! default as the fallback. A group something still names cannot be retired;
//! one group is designated the default, and cannot be retired while it is.
//!
//! Every expected value comes from what the test itself writes.

mod support;

use chrono::{TimeDelta, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use support::migrate::{database_at, drop_database, migrate_rest};
use support::world::{db_now, seed_model, seed_tenant};
use support::{OperatorSession, Resp, TestCp};
use uuid::Uuid;

const GROUPS: &str = "/admin/provider-groups";

/// `(status, code)` an answer must carry.
fn assert_status(r: &Resp, status: u16, code: &str) {
    assert_eq!(
        (r.status, r.body["code"].as_str()),
        (status, Some(code)),
        "{r:?}"
    );
}

/// `r` is a 422 at exactly `path`.
fn assert_422_at(r: &Resp, path: &str) {
    assert_status(r, 422, "validation_failed");
    let paths: Vec<&str> = r.body["details"]
        .as_array()
        .map(|d| d.iter().filter_map(|x| x["path"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(paths, vec![path], "{r:?}");
}

/// A group body: `name`, what tenants see, and its default rate.
fn body(name: &str, display_name: &str, rate: Value) -> Value {
    json!({
        "name": name,
        "display_name": display_name,
        "description": "标准分组 7c1",
        "default_cny_per_usd": rate,
    })
}

/// `POST /admin/provider-groups` as `who`.
async fn post_group(cp: &TestCp, who: &OperatorSession, group: Value) -> Resp {
    cp.send("POST", GROUPS, Some(who.cookie()), Some(group))
        .await
}

/// `POST /admin/provider-groups` as `who`, which must answer 201; the group
/// it answered.
async fn create_group(cp: &TestCp, who: &OperatorSession, group: Value) -> Value {
    let r = post_group(cp, who, group).await;
    assert_eq!(r.status, 201, "{r:?}");
    r.body
}

fn id_of(group: &Value) -> String {
    group["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a group id in {group}"))
        .to_string()
}

/// `POST /admin/provider-groups/{id}/rates` as `who`.
async fn post_rate(cp: &TestCp, who: &OperatorSession, id: &str, rate: Value) -> Resp {
    cp.send(
        "POST",
        &format!("{GROUPS}/{id}/rates"),
        Some(who.cookie()),
        Some(rate),
    )
    .await
}

/// `GET /admin/provider-groups/{id}/rates` as `who`, which must answer 200.
async fn rates(cp: &TestCp, who: &OperatorSession, id: &str) -> Value {
    let r = cp
        .send(
            "GET",
            &format!("{GROUPS}/{id}/rates"),
            Some(who.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body
}

/// `POST /admin/provider-groups/{id}/retire` as `who`.
async fn retire(cp: &TestCp, who: &OperatorSession, id: &str) -> Resp {
    cp.send(
        "POST",
        &format!("{GROUPS}/{id}/retire"),
        Some(who.cookie()),
        Some(json!({})),
    )
    .await
}

/// `POST /admin/provider-groups/{id}/make-default` as `who`.
async fn make_default(cp: &TestCp, who: &OperatorSession, id: &str) -> Resp {
    cp.send(
        "POST",
        &format!("{GROUPS}/{id}/make-default"),
        Some(who.cookie()),
        Some(json!({})),
    )
    .await
}

/// The `cny_per_usd` a plan's current revision states, or `None` when there
/// is no current revision or it ends the override.
fn current_rate(plan: &Value) -> Option<Value> {
    match &plan["current"] {
        Value::Object(current) => match &current["cny_per_usd"] {
            Value::Null | Value::Object(_) => None,
            rate => Some(rate.clone()),
        },
        _ => None,
    }
}

async fn pricing_version(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT (value::text)::bigint FROM cp_settings WHERE key = 'pricing_version'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pricing_version")
}

/// The names of every stored group, by name.
async fn group_names(cp: &TestCp) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM provider_groups ORDER BY name")
        .fetch_all(cp.db().pool())
        .await
        .expect("read the groups")
}

/// `(display_name, description, is_default)` of the group named `name`.
async fn group_row(cp: &TestCp, name: &str) -> (String, String, bool) {
    sqlx::query_as(
        "SELECT display_name, description, is_default FROM provider_groups WHERE name = $1",
    )
    .bind(name)
    .fetch_one(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("read group {name}: {e}"))
}

/// Waits until a session is queued for `holder`'s transaction: a `FOR
/// UPDATE` behind a `FOR SHARE` waits on the row's transaction id, not on a
/// lock on the table. Fails after 10 s.
async fn until_blocked(pool: &PgPool, holder: i32) -> i32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: Option<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_locks l WHERE NOT l.granted AND l.locktype = 'transactionid' \
             AND $1 = ANY(pg_blocking_pids(l.pid)) LIMIT 1",
        )
        .bind(holder)
        .fetch_optional(pool)
        .await
        .expect("read pg_locks");
        if let Some(pid) = waiting {
            return pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no session waits on the group's holder"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_migration_backfills_a_group_for_every_name_in_use() {
    let Some(pool) = database_at(22).await else {
        return;
    };
    let tenant = Uuid::new_v4();
    let live_key = Uuid::new_v4();
    let revoked_key = Uuid::new_v4();
    let provider_key = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO gw_resources (kind, id, doc) VALUES ('provider_keys', '{provider_key}', \
             '{{\"display_name\": \"acct-backfill-7c1\", \"groups\": [\"grp-a-7c1\"], \"provider\": \
             \"openai\", \"api_key\": \"sk-backfill-7c1\"}}')"
        ),
        format!(
            "INSERT INTO tenants (id, name, status, allowed_provider_groups) VALUES \
             ('{tenant}', 'backfill-7c1', 'active', '{{\"grp-b-7c1\"}}')"
        ),
        format!(
            "INSERT INTO tenant_keys (id, tenant_id, label, key_hash, key_prefix, \
             allowed_provider_groups, created_by_kind) VALUES \
             ('{live_key}', '{tenant}', 'live-7c1', 'hash-live-7c1', 'sk-live', \
             '{{\"grp-c-7c1\"}}', 'operator'), \
             ('{revoked_key}', '{tenant}', 'gone-7c1', 'hash-gone-7c1', 'sk-gone', \
             '{{\"grp-gone-7c1\"}}', 'operator')"
        ),
        format!(
            "UPDATE tenant_keys SET revoked_at = now() WHERE id = '{revoked_key}'"
        ),
        "INSERT INTO cp_settings (key, value) VALUES ('tenant_defaults', \
         '{\"default_provider_groups\": [\"grp-d-7c1\"]}')"
            .to_string(),
    ] {
        sqlx::query(&sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    migrate_rest(&pool).await.expect("the rest migrates");

    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM provider_groups ORDER BY name")
        .fetch_all(&pool)
        .await
        .expect("read the groups");
    assert_eq!(
        names,
        vec![
            "default".to_string(),
            "grp-a-7c1".to_string(),
            "grp-b-7c1".to_string(),
            "grp-c-7c1".to_string(),
            "grp-d-7c1".to_string(),
        ],
        "the revoked key's group is not in use and must not be backfilled"
    );
    for name in ["grp-a-7c1", "grp-b-7c1", "grp-c-7c1", "grp-d-7c1"] {
        let (display_name, description, is_default): (String, String, bool) = sqlx::query_as(
            "SELECT display_name, description, is_default FROM provider_groups WHERE name = $1",
        )
        .bind(name)
        .fetch_one(&pool)
        .await
        .expect("a backfilled group");
        assert_eq!((display_name.as_str(), description.as_str()), (name, ""));
        assert!(!is_default, "{name} is not the designated default");
    }
    // A backfilled group gets no rate: its rows are `missing_rate` until an
    // operator sets one.
    let rates: i64 = sqlx::query_scalar("SELECT count(*) FROM pricing_group_rates")
        .fetch_one(&pool)
        .await
        .expect("count the rates");
    assert_eq!(rates, 0);
    drop_database(pool).await;
}

#[tokio::test]
async fn a_fresh_database_has_the_first_boot_default_group() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Viewer)
        .await;
    let r = cp
        .send(
            "GET",
            "/admin/settings/tenant-defaults",
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["default_provider_groups"],
        json!(["shared"]),
        "the first boot's tenant defaults"
    );
    let names = group_names(&cp).await;
    assert!(names.contains(&"shared".to_string()), "{names:?}");
    // A fresh database has both: the first-boot group, and the designated
    // default (`default`, display name 默认).
    assert!(
        names.contains(&"default".to_string()),
        "the designated default group exists from the migration: {names:?}"
    );
    let (display_name, _, is_default) = group_row(&cp, "default").await;
    assert_eq!((display_name.as_str(), is_default), ("默认", true));
    let designated: Vec<String> =
        sqlx::query_scalar("SELECT name FROM provider_groups WHERE is_default")
            .fetch_all(cp.db().pool())
            .await
            .expect("read the designated group");
    assert_eq!(designated, vec!["default".to_string()]);
}

#[tokio::test]
async fn creating_a_group_writes_its_default_rate_in_the_same_transaction() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let version = pricing_version(&cp).await;
    let group = create_group(&cp, &admin, body("grp-gp-7c1", "标准", json!("7.2"))).await;
    assert_eq!(group["name"], "grp-gp-7c1");
    assert_eq!(group["display_name"], "标准");
    assert_eq!(group["description"], "标准分组 7c1");
    assert_eq!(group["is_default"], false);
    assert_eq!(current_rate(&group["default_rate"]), Some(json!("7.2")));
    assert_eq!(group["tenants"], 0);
    assert_eq!(group["defaults"], 0);
    assert_eq!(group["keys"], 0);
    assert_eq!(group["provider_keys"], json!([]));

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pricing_group_rates r JOIN provider_groups g ON g.id = r.group_id \
         WHERE g.name = 'grp-gp-7c1' AND r.model_id IS NULL",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("count the group's rates");
    assert_eq!(rows, 1, "one default rate row");
    assert_eq!(pricing_version(&cp).await, version + 1);

    let audit: Vec<(String, String, Option<Uuid>, Value)> = sqlx::query_as(
        "SELECT action, target_kind, tenant_id, detail FROM audit_log \
         WHERE action = 'provider_group.create' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log");
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].1, "provider_group");
    assert_eq!(audit[0].2, None);
    assert_eq!(audit[0].3["name"], "grp-gp-7c1");
}

#[tokio::test]
async fn a_group_name_is_lowercase_and_unique() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let long = "a".repeat(65);
    for name in ["Grp", "-a", "a b", long.as_str()] {
        let r = post_group(&cp, &admin, body(name, "标准", json!("7.2"))).await;
        assert_422_at(&r, "/name");
    }
    create_group(&cp, &admin, body("grp-uniq-7c1", "标准", json!("7.2"))).await;
    let r = post_group(&cp, &admin, body("grp-uniq-7c1", "另一个", json!("7.2"))).await;
    assert_status(&r, 422, "validation_failed");
    let details = r.body["details"].as_array().expect("details");
    assert_eq!(details.len(), 1, "{r:?}");
    assert_eq!(details[0]["path"], "/name");
    assert_eq!(details[0]["message"], "分组名称已存在");
}

#[tokio::test]
async fn a_rate_is_zero_to_one_hundred_with_four_decimals() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    for rate in [
        json!("100.0001"),
        json!("abc"),
        json!("7.23456"),
        json!(7.2),
    ] {
        let r = post_group(&cp, &admin, body("grp-rate-7c1", "标准", rate)).await;
        assert_422_at(&r, "/default_cny_per_usd");
    }
    let group = create_group(&cp, &admin, body("grp-rate-7c1", "标准", json!("100"))).await;
    let id = id_of(&group);
    // A rate of 0 is a free group, and is written.
    let now = db_now(&cp).await;
    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": null, "cny_per_usd": "0", "effective_from": now}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["revision"]["cny_per_usd"], "0");
    assert_eq!(
        current_rate(&rates(&cp, &admin, &id).await["default"]),
        Some(json!("0"))
    );
}

#[tokio::test]
async fn a_rate_never_starts_in_the_past() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let past = db_now(&cp).await - TimeDelta::minutes(1);
    let r = cp
        .send(
            "POST",
            GROUPS,
            Some(admin.cookie()),
            Some(json!({
                "name": "grp-past-7c1",
                "display_name": "标准",
                "default_cny_per_usd": "7.2",
                "effective_from": past,
            })),
        )
        .await;
    assert_422_at(&r, "/effective_from");
    // Nothing was written, the group included.
    assert!(group_names(&cp).await.iter().all(|n| n != "grp-past-7c1"));
    assert_eq!(cp.count("pricing_group_rates").await, 0);

    let group = create_group(&cp, &admin, body("grp-past-7c1", "标准", json!("7.2"))).await;
    let r = post_rate(
        &cp,
        &admin,
        &id_of(&group),
        json!({"model_id": null, "cny_per_usd": "8", "effective_from": past}),
    )
    .await;
    assert_422_at(&r, "/effective_from");
}

#[tokio::test]
async fn a_model_override_and_its_end() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let model = seed_model(&cp, &admin).await;
    let group = create_group(&cp, &admin, body("grp-model-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);

    let now = db_now(&cp).await;
    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": model, "cny_per_usd": "6", "effective_from": now}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let all = rates(&cp, &admin, &id).await;
    let item = all["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|m| m["model_id"] == json!(model))
        .unwrap_or_else(|| panic!("model {model} is listed: {all:?}"));
    assert_eq!(current_rate(&item["rate"]), Some(json!("6")));
    // The group default is untouched by the override.
    assert_eq!(current_rate(&all["default"]), Some(json!("7.2")));

    // A model row with no rate ends the override: the default applies again.
    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": model, "cny_per_usd": null, "effective_from": db_now(&cp).await}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let all = rates(&cp, &admin, &id).await;
    let item = all["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|m| m["model_id"] == json!(model))
        .expect("the model is listed");
    assert_eq!(current_rate(&item["rate"]), None);
    assert_eq!(
        item["rate"]["current"]["cny_per_usd"],
        Value::Null,
        "the current revision ends the override"
    );

    // The group default has no such end: only a model's override ends.
    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": null, "cny_per_usd": null, "effective_from": db_now(&cp).await}),
    )
    .await;
    assert_422_at(&r, "/cny_per_usd");

    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE action LIKE 'pricing.group_rate.%' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log");
    assert_eq!(
        audit,
        vec![
            "pricing.group_rate.create".to_string(),
            "pricing.group_rate.create".to_string(),
            "pricing.group_rate.end_override".to_string(),
        ],
        "the group's default rate, the override, and its end"
    );
}

#[tokio::test]
async fn a_rate_already_scheduled_later_is_reported() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(&cp, &admin, body("grp-later-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);
    let later = db_now(&cp).await + TimeDelta::hours(1);
    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": null, "cny_per_usd": "8", "effective_from": later}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let scheduled = r.body["revision"]["revision_id"].clone();
    assert_eq!(r.body["warnings"], json!([]));

    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": null, "cny_per_usd": "7.5", "effective_from": db_now(&cp).await}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let warnings = r.body["warnings"].as_array().expect("warnings");
    assert_eq!(warnings.len(), 1, "{r:?}");
    assert_eq!(warnings[0]["code"], "superseded_later");
    assert_eq!(warnings[0]["revision_id"], scheduled);
}

#[tokio::test]
async fn rate_revisions_refuse_update_delete_and_truncate() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    create_group(&cp, &admin, body("grp-append-7c1", "标准", json!("7.2"))).await;
    for sql in [
        "UPDATE pricing_group_rates SET cny_per_usd_e4 = 1",
        "DELETE FROM pricing_group_rates",
        "TRUNCATE pricing_group_rates",
    ] {
        let e = sqlx::query(sql)
            .execute(cp.db().pool())
            .await
            .expect_err(sql);
        assert!(e.to_string().contains("append-only"), "{sql}: {e}");
    }
    assert_eq!(cp.count("pricing_group_rates").await, 1);
}

#[tokio::test]
async fn a_group_in_use_cannot_be_retired() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Admin)
        .await;
    let group = create_group(&cp, &admin, body("grp-inuse-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);

    // A tenant naming it, the defaults naming it, a live key's own override,
    // and a provider key listing it.
    let tenant = seed_tenant(&cp, &admin).await;
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("the tenant's version");
    let r = cp
        .send_raw(
            "PATCH",
            &format!("/admin/tenants/{tenant}"),
            Some(admin.cookie()),
            Some("application/json"),
            Some(
                serde_json::to_vec(&json!({"allowed_provider_groups": ["grp-inuse-7c1"]}))
                    .expect("a body"),
            ),
        )
        .await;
    // The PATCH carries its If-Match, as every tenant edit does.
    assert_eq!(r.status, 428, "a PATCH without If-Match: {r:?}");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .patch(cp.url(&format!("/admin/tenants/{tenant}")))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_provider_groups": ["grp-inuse-7c1"]}))
        .send()
        .await
        .expect("send the PATCH");
    assert_eq!(r.status(), 200);
    // The defaults are an Admin's, and so is every tenant PATCH below.
    let r = cp
        .send(
            "PUT",
            "/admin/settings/tenant-defaults",
            Some(admin.cookie()),
            Some(json!({"concurrency": 20, "default_provider_groups": ["grp-inuse-7c1"]})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let unconfigured = admin;
    let key = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(unconfigured.cookie()),
            Some(json!({"label": "inuse-7c1", "allowed_provider_groups": ["grp-inuse-7c1"]})),
        )
        .await;
    assert_eq!(key.status, 201, "{key:?}");
    let provider_key = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(unconfigured.cookie()),
            Some(json!({"doc": {
                "display_name": "acct-inuse-7c1",
                "provider": "openai",
                "api_key": "sk-inuse-7c1",
                "groups": ["grp-inuse-7c1"],
            }})),
        )
        .await;
    assert_eq!(provider_key.status, 201, "{provider_key:?}");

    let r = retire(&cp, &unconfigured, &id).await;
    assert_status(&r, 409, "group_in_use");
    assert_eq!(
        r.body["counts"],
        json!({"tenants": 1, "defaults": 1, "keys": 1, "provider_keys": 1, "default_group": 0}),
        "{r:?}"
    );
    // Still live: nothing was written.
    let retired: Option<String> =
        sqlx::query_scalar("SELECT retired_at::text FROM provider_groups WHERE id = $1")
            .bind(Uuid::parse_str(&id).expect("a group id"))
            .fetch_one(cp.db().pool())
            .await
            .expect("read retired_at");
    assert_eq!(retired, None);

    // Each naming is cleared, and then it retires.
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("the tenant's version");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .patch(cp.url(&format!("/admin/tenants/{tenant}")))
        .header(reqwest::header::COOKIE, unconfigured.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&json!({"allowed_provider_groups": ["shared"]}))
        .send()
        .await
        .expect("send the PATCH");
    assert_eq!(r.status(), 200);
    let r = cp
        .send(
            "PUT",
            "/admin/settings/tenant-defaults",
            Some(unconfigured.cookie()),
            Some(json!({"concurrency": 20, "default_provider_groups": ["shared"]})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // The key's own override is revoked with the key.
    let key_id = key.body["key"]["id"]
        .as_str()
        .expect("a key id")
        .to_string();
    let key_version: i64 = sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1")
        .bind(Uuid::parse_str(&key_id).expect("a key id"))
        .fetch_one(cp.db().pool())
        .await
        .expect("the key's version");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .delete(cp.url(&format!("/admin/keys/{key_id}")))
        .header(reqwest::header::COOKIE, unconfigured.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{key_version}\""))
        .send()
        .await
        .expect("send the delete");
    assert_eq!(r.status(), 204);
    let pk_id = provider_key.body["id"]
        .as_str()
        .expect("a provider key id")
        .to_string();
    let pk_version: i64 = sqlx::query_scalar(
        "SELECT version FROM gw_resources WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(Uuid::parse_str(&pk_id).expect("a provider key id"))
    .fetch_one(cp.db().pool())
    .await
    .expect("the provider key's version");
    let r = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .delete(cp.url(&format!("/admin/resources/provider_keys/{pk_id}")))
        .header(reqwest::header::COOKIE, unconfigured.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{pk_version}\""))
        .send()
        .await
        .expect("send the delete");
    assert_eq!(r.status(), 200);

    let r = retire(&cp, &unconfigured, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body["retired_at"].is_string(), "{r:?}");
    assert_eq!(r.body["tenants"], 0);
    assert_eq!(r.body["defaults"], 0);
    assert_eq!(r.body["keys"], 0);
    assert_eq!(r.body["provider_keys"], json!([]));
}

#[tokio::test]
async fn a_retired_group_takes_no_rate_and_keeps_its_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(&cp, &admin, body("grp-retired-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);
    let r = retire(&cp, &admin, &id).await;
    assert_eq!(r.status, 200, "{r:?}");

    let r = post_rate(
        &cp,
        &admin,
        &id,
        json!({"model_id": null, "cny_per_usd": "8", "effective_from": db_now(&cp).await}),
    )
    .await;
    assert_status(&r, 409, "group_retired");
    let r = make_default(&cp, &admin, &id).await;
    assert_status(&r, 409, "group_retired");

    // The routing name is never freed, retired or not.
    let r = post_group(&cp, &admin, body("grp-retired-7c1", "另一个", json!("7.2"))).await;
    assert_status(&r, 422, "validation_failed");
    assert_eq!(r.body["details"][0]["path"], "/name");
    assert_eq!(r.body["details"][0]["message"], "分组名称已存在");
    // Retiring again changes nothing and answers the same view.
    let r = retire(&cp, &admin, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["name"], "grp-retired-7c1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retiring_waits_for_a_rate_write_on_the_group() {
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(&cp, &admin, body("grp-lock-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);
    let group_uuid = Uuid::parse_str(&id).expect("a group id");

    // A rate write holds the group `FOR SHARE`; retire needs `FOR UPDATE` and
    // must wait for it.
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM provider_groups WHERE id = $1 FOR SHARE")
        .bind(group_uuid)
        .execute(&mut *holder)
        .await
        .expect("hold the group");
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .expect("the holder's pid");

    let path = format!("{GROUPS}/{id}/retire");
    let cookie = admin.cookie().to_string();
    let url = cp.url(&path);
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client")
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("send the retire")
            .status()
            .as_u16()
    });
    until_blocked(cp.db().pool(), holder_pid).await;
    assert!(
        !request.is_finished(),
        "retire answered while a rate write held the group"
    );
    holder.commit().await.expect("commit");
    let status = request.await.expect("join the retire");
    assert_eq!(status, 200);
}

#[tokio::test]
async fn who_may_call_the_group_routes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let operator = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(&cp, &operator, body("grp-roles-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);
    let viewer = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Viewer)
        .await;

    let r = cp
        .send(
            "GET",
            &format!("{GROUPS}/{id}/rates"),
            Some(viewer.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "a viewer reads the rates: {r:?}");
    let r = post_rate(
        &cp,
        &viewer,
        &id,
        json!({"model_id": null, "cny_per_usd": "8", "effective_from": db_now(&cp).await}),
    )
    .await;
    assert_eq!(r.status, 403, "a viewer writes no rate: {r:?}");
    let r = retire(&cp, &viewer, &id).await;
    assert_eq!(r.status, 403, "a viewer retires nothing: {r:?}");
    let r = make_default(&cp, &viewer, &id).await;
    assert_eq!(r.status, 403, "a viewer designates nothing: {r:?}");
    let r = cp
        .send(
            "PATCH",
            &format!("{GROUPS}/{id}"),
            Some(viewer.cookie()),
            Some(json!({"display_name": "改名"})),
        )
        .await;
    assert_eq!(r.status, 403, "a viewer edits nothing: {r:?}");
    // An operator edits, and the display name is what changes.
    let r = cp
        .send(
            "PATCH",
            &format!("{GROUPS}/{id}"),
            Some(operator.cookie()),
            Some(json!({"display_name": "改名", "description": "新的说明"})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["display_name"], "改名");
    assert_eq!(r.body["description"], "新的说明");
    assert_eq!(
        r.body["name"], "grp-roles-7c1",
        "the routing name never changes"
    );
}

#[tokio::test]
async fn the_upgrade_designates_a_default_group() {
    let Some(pool) = database_at(22).await else {
        return;
    };
    // A database whose names in use already include `default` (and the
    // first-boot `shared`), but which has no designated group yet: the
    // migration designates `default`, and inserts no second one.
    sqlx::query(
        "INSERT INTO tenants (id, name, status, allowed_provider_groups) VALUES \
         (gen_random_uuid(), 'defaulted-7c1', 'active', '{\"default\"}')",
    )
    .execute(&pool)
    .await
    .expect("seed a tenant naming default");
    migrate_rest(&pool).await.expect("the rest migrates");
    let rows: Vec<(String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT name, display_name, is_default, retired_at::text FROM provider_groups \
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("read the groups");
    let names: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(names, vec!["default", "shared"], "{rows:?}");
    let (name, display_name, is_default, retired_at) = rows
        .iter()
        .find(|r| r.0 == "default")
        .expect("a default group");
    assert_eq!(
        (name.as_str(), display_name.as_str(), *is_default),
        ("default", "default", true),
        "a backfilled `default` keeps its name as its display name, and is designated"
    );
    assert_eq!(retired_at, &None);
    let designated: Vec<String> =
        sqlx::query_scalar("SELECT name FROM provider_groups WHERE is_default")
            .fetch_all(&pool)
            .await
            .expect("read the designated group");
    assert_eq!(designated, vec!["default".to_string()]);
    drop_database(pool).await;
}

#[tokio::test]
async fn a_default_group_cannot_be_retired() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let default_id: Uuid =
        sqlx::query_scalar("SELECT id FROM provider_groups WHERE name = 'default'")
            .fetch_one(cp.db().pool())
            .await
            .expect("the designated default");
    let r = retire(&cp, &admin, &default_id.to_string()).await;
    assert_status(&r, 409, "group_in_use");
    assert_eq!(r.body["counts"]["default_group"], 1, "{r:?}");
}

#[tokio::test]
async fn make_default_moves_the_flag() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(
        &cp,
        &admin,
        body("grp-newdefault-7c1", "新默认", json!("7.2")),
    )
    .await;
    let id = id_of(&group);
    let r = make_default(&cp, &admin, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["is_default"], true);
    assert_eq!(r.body["defaults"], 0, "designation is not a tenant default");

    let designated: Vec<String> =
        sqlx::query_scalar("SELECT name FROM provider_groups WHERE is_default")
            .fetch_all(cp.db().pool())
            .await
            .expect("read the designated group");
    assert_eq!(designated, vec!["grp-newdefault-7c1".to_string()]);
    // The previous default is no longer designated, and may now retire.
    let r = retire(
        &cp,
        &admin,
        &Uuid::parse_str(
            &sqlx::query_scalar::<_, Uuid>("SELECT id FROM provider_groups WHERE name = 'default'")
                .fetch_one(cp.db().pool())
                .await
                .expect("the old default")
                .to_string(),
        )
        .expect("a group id")
        .to_string(),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");

    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'provider_group.make_default'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read the audit log");
    assert_eq!(audit, 1);
}

#[tokio::test]
async fn a_group_lists_its_provider_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(&cp, &admin, body("grp-keys-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);
    let r = cp
        .send(
            "POST",
            "/admin/resources/provider_keys",
            Some(admin.cookie()),
            Some(json!({"doc": {
                "display_name": "acct-keys-7c1",
                "provider": "openai",
                "api_key": "sk-keys-7c1",
                "groups": ["grp-keys-7c1"],
            }})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let pk = r.body["id"].clone();
    let r = cp
        .send(
            "GET",
            &format!("{GROUPS}/{id}/rates"),
            Some(admin.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["group"]["provider_keys"],
        json!([{"id": pk, "display_name": "acct-keys-7c1"}])
    );
    let _ = Utc::now();
}
