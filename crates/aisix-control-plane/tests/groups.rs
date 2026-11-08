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

use aisix_control_plane::resources::write::RESOURCES_LOCK_KEY;
use chrono::TimeDelta;
use serde_json::{json, Value};
use sqlx::PgPool;
use support::migrate::{database_at, drop_database, migrate_rest};
use support::world::{db_now, seed_key, seed_model, seed_tenant};
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
    // The rate route answers a JSON number exactly as the group route does.
    for rate in [json!(7.2), json!("100.0001"), json!(null)] {
        let r = post_rate(
            &cp,
            &admin,
            &id,
            json!({"model_id": null, "cny_per_usd": rate, "effective_from": db_now(&cp).await}),
        )
        .await;
        assert_422_at(&r, "/cny_per_usd");
    }
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
        // A charge references the rate it was priced at, so a truncate that
        // names only this table is refused by the foreign key before the
        // trigger runs; `CASCADE` carries it to the referencing tables, and
        // the append-only triggers are what refuse them.
        "TRUNCATE pricing_group_rates CASCADE",
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
            // The tenant is assigned exactly this group, so the key takes it
            // without naming it: a key's own group is fixed at creation.
            Some(json!({"label": "inuse-7c1"})),
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
    // The key naming it is revoked, which leaves nothing naming the group.
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
    // Everything else is 0 on a fresh database, so the designated flag alone
    // is what refuses the retire.
    assert_eq!(
        r.body["counts"],
        json!({"tenants": 0, "defaults": 0, "keys": 0, "provider_keys": 0, "default_group": 1}),
        "{r:?}"
    );
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
}

/// `group_id` of each key by label, from `tenant_keys`.
async fn key_groups(pool: &PgPool) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT k.label, g.name FROM tenant_keys k \
         LEFT JOIN provider_groups g ON g.id = k.group_id ORDER BY k.label",
    )
    .fetch_all(pool)
    .await
    .expect("read the keys' groups")
}

/// `group_id` of the key labelled `label`.
async fn key_group(pool: &PgPool, label: &str) -> Option<String> {
    let groups = key_groups(pool).await;
    groups
        .into_iter()
        .find(|(l, _)| l == label)
        .unwrap_or_else(|| panic!("a key labelled {label}"))
        .1
}

/// The `detail` of the one audit row `action` names. Exactly one: a migration
/// that wrote it twice would otherwise read as correct.
async fn audit_detail(pool: &PgPool, action: &str) -> Value {
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = $1 AND actor_kind = 'system' ORDER BY id",
    )
    .bind(action)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| panic!("read the {action} audit rows: {e}"));
    assert_eq!(details.len(), 1, "one {action} row: {details:?}");
    details[0].clone()
}

/// How many audit rows carry `action`.
async fn audit_rows(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("count the {action} audit rows: {e}"))
}

#[tokio::test]
async fn the_migration_gives_each_key_the_group_it_can_only_mean() {
    let Some(pool) = database_at(23).await else {
        return;
    };
    // Two customer groups, created as an operator would before the upgrade,
    // and one tenant of each shape the backfill reads.
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let customer_one = Uuid::new_v4();
    let customer_two = Uuid::new_v4();
    let internal = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO provider_groups (id, name, display_name) VALUES \
             ('{a}', 'grp-a-7c1', '分组甲'), ('{b}', 'grp-b-7c1', '分组乙')"
        ),
        format!(
            "INSERT INTO tenants (id, name, status, allowed_provider_groups) VALUES \
             ('{customer_one}', 'one-7c1', 'active', '{{\"grp-b-7c1\"}}'), \
             ('{customer_two}', 'two-7c1', 'active', '{{\"grp-a-7c1\",\"grp-b-7c1\"}}')"
        ),
        format!(
            "INSERT INTO tenants (id, name, status, is_internal) VALUES \
             ('{internal}', 'platform-7c1', 'active', true)"
        ),
        format!(
            "INSERT INTO tenant_keys (id, tenant_id, label, key_hash, key_prefix, \
             allowed_provider_groups, created_by_kind) VALUES \
             (gen_random_uuid(), '{customer_one}', 'override-one-7c1', 'h1-7c1', 'k1', \
              '{{\"grp-a-7c1\"}}', 'operator'), \
             (gen_random_uuid(), '{customer_one}', 'tenant-one-7c1', 'h2-7c1', 'k2', \
              NULL, 'operator'), \
             (gen_random_uuid(), '{customer_two}', 'tenant-two-7c1', 'h3-7c1', 'k3', \
              NULL, 'operator'), \
             (gen_random_uuid(), '{internal}', 'internal-override-7c1', 'h4-7c1', 'k4', \
              '{{\"grp-a-7c1\"}}', 'operator'), \
             (gen_random_uuid(), '{internal}', 'internal-none-7c1', 'h5-7c1', 'k5', \
              NULL, 'operator'), \
             (gen_random_uuid(), '{internal}', 'internal-two-7c1', 'h6-7c1', 'k6', \
              '{{\"grp-a-7c1\",\"grp-b-7c1\"}}', 'operator'), \
             (gen_random_uuid(), '{customer_one}', 'revoked-7c1', 'h7-7c1', 'k7', \
              '{{\"grp-a-7c1\"}}', 'operator')"
        ),
        "UPDATE tenant_keys SET revoked_at = now() WHERE label = 'revoked-7c1'".to_string(),
    ] {
        sqlx::query(&sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    migrate_rest(&pool).await.expect("the rest migrates");

    // The one group a key's own override names is the group it can only
    // mean, whoever its tenant is.
    assert_eq!(
        key_group(&pool, "override-one-7c1").await.as_deref(),
        Some("grp-a-7c1")
    );
    assert_eq!(
        key_group(&pool, "internal-override-7c1").await.as_deref(),
        Some("grp-a-7c1")
    );
    // A customer key with no override takes the one group its tenant names.
    assert_eq!(
        key_group(&pool, "tenant-one-7c1").await.as_deref(),
        Some("grp-b-7c1")
    );
    // Two groups are two readings, and the key is closed until one is set.
    assert_eq!(key_group(&pool, "tenant-two-7c1").await, None);
    // Every internal key still without one takes the designated default,
    // which an operator can see it reaches.
    assert_eq!(
        key_group(&pool, "internal-none-7c1").await.as_deref(),
        Some("default")
    );
    assert_eq!(
        key_group(&pool, "internal-two-7c1").await.as_deref(),
        Some("default")
    );
    // A revoked key takes none: it is not routed, and not priced.
    assert_eq!(key_group(&pool, "revoked-7c1").await, None);
    drop_database(pool).await;
}

#[tokio::test]
async fn the_upgrade_reports_what_the_default_group_reaches() {
    let Some(pool) = database_at(23).await else {
        return;
    };
    let internal = Uuid::new_v4();
    for sql in [
        format!(
            "INSERT INTO tenants (id, name, status, is_internal) VALUES \
             ('{internal}', 'platform-7c1', 'active', true)"
        ),
        format!(
            "INSERT INTO tenant_keys (id, tenant_id, label, key_hash, key_prefix, \
             created_by_kind) VALUES \
             (gen_random_uuid(), '{internal}', 'internal-none-7c1', 'h1-7c1', 'k1', 'operator'), \
             (gen_random_uuid(), '{internal}', 'internal-none-too-7c1', 'h2-7c1', 'k2', 'operator')"
        ),
        "INSERT INTO gw_resources (kind, id, doc) VALUES ('provider_keys', gen_random_uuid(), \
         '{\"display_name\": \"acct-def-7c1\", \"provider\": \"openai\", \"api_key\": \
         \"sk-def-7c1\", \"groups\": [\"default\"]}')"
            .to_string(),
    ] {
        sqlx::query(&sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    migrate_rest(&pool).await.expect("the rest migrates");

    // The report names the keys it gave the group, and the upstreams that
    // group reaches, so an operator can see what they now reach.
    let detail = audit_detail(&pool, "provider_group.default_assigned").await;
    assert_eq!(detail["name"], "default");
    assert_eq!(detail["keys"], 2, "{detail}");
    assert_eq!(detail["provider_keys"], json!(["acct-def-7c1"]));
    drop_database(pool).await;
}

#[tokio::test]
async fn the_upgrade_reports_a_default_group_that_reaches_no_upstream() {
    let Some(pool) = database_at(23).await else {
        return;
    };
    sqlx::query(
        "INSERT INTO tenants (id, name, status, is_internal) VALUES \
         (gen_random_uuid(), 'platform-7c1', 'active', true)",
    )
    .execute(&pool)
    .await
    .expect("seed the internal tenant");
    sqlx::query(
        "INSERT INTO tenant_keys (id, tenant_id, label, key_hash, key_prefix, created_by_kind) \
         SELECT gen_random_uuid(), id, 'internal-none-7c1', 'h1-7c1', 'k1', 'operator' \
         FROM tenants WHERE is_internal",
    )
    .execute(&pool)
    .await
    .expect("seed an internal key");
    // An upstream account that lists another group: the report names the
    // accounts the default group reaches, not every account there is.
    sqlx::query(
        "INSERT INTO gw_resources (kind, id, doc) VALUES ('provider_keys', gen_random_uuid(), \
         '{\"display_name\": \"acct-shared-7c1\", \"provider\": \"openai\", \"api_key\": \
         \"sk-shared-7c1\", \"groups\": [\"shared\"]}')",
    )
    .execute(&pool)
    .await
    .expect("seed an account of another group");

    migrate_rest(&pool).await.expect("the rest migrates");

    // routio.ai's shape: the internal tenant, and no account that reaches the
    // default group. The report says so rather than leaving the question open.
    let detail = audit_detail(&pool, "provider_group.default_assigned").await;
    assert_eq!(detail["keys"], 1, "{detail}");
    assert_eq!(detail["provider_keys"], json!([]), "{detail}");
    drop_database(pool).await;
}

#[tokio::test]
async fn a_fresh_install_writes_no_default_assigned_row() {
    let Some(pool) = database_at(23).await else {
        return;
    };
    // A fresh install's shape: the migrations run before
    // `ensure_internal_tenant` creates the tenant, so there is no key to give
    // the default group, and the upgrade records no change.
    migrate_rest(&pool).await.expect("the rest migrates");
    assert_eq!(
        audit_rows(&pool, "provider_group.default_assigned").await,
        0,
        "an assignment that touched no key records no change"
    );
    // The default group is still designated, and the table still works.
    let designated: Vec<String> =
        sqlx::query_scalar("SELECT name FROM provider_groups WHERE is_default")
            .fetch_all(&pool)
            .await
            .expect("read the designated group");
    assert_eq!(designated, vec!["default".to_string()]);
    drop_database(pool).await;
}

#[tokio::test]
async fn retiring_counts_keys_by_their_group() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(&cp, &admin, body("grp-keycount-7c1", "标准", json!("7.2"))).await;
    let id = id_of(&group);
    let tenant = seed_tenant(&cp, &admin).await;
    let key = seed_key(&cp, &admin, tenant, None).await;
    let key_id = Uuid::parse_str(&key).expect("a key id");
    // The column, as Task 11 sets it: a key names its group, and its own
    // override may say nothing at all.
    sqlx::query("UPDATE tenant_keys SET group_id = $1::uuid WHERE id = $2")
        .bind(&id)
        .bind(key_id)
        .execute(cp.db().pool())
        .await
        .expect("set the key's group");

    let r = retire(&cp, &admin, &id).await;
    assert_status(&r, 409, "group_in_use");
    assert_eq!(r.body["counts"]["keys"], 1, "{r:?}");

    // A key of the same tenant with no group counts for nothing: an absent
    // override must not read as naming the group.
    let ungrouped = seed_key(&cp, &admin, tenant, None).await;
    let ungrouped: Uuid = Uuid::parse_str(&ungrouped).expect("a key id");
    sqlx::query("UPDATE tenant_keys SET group_id = NULL WHERE id = $1")
        .bind(ungrouped)
        .execute(cp.db().pool())
        .await
        .expect("clear the second key's group");
    let r = retire(&cp, &admin, &id).await;
    assert_status(&r, 409, "group_in_use");
    assert_eq!(r.body["counts"]["keys"], 1, "{r:?}");

    sqlx::query("UPDATE tenant_keys SET group_id = NULL WHERE id = $1")
        .bind(key_id)
        .execute(cp.db().pool())
        .await
        .expect("clear the key's group");
    let r = retire(&cp, &admin, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["keys"], 0, "{r:?}");
}

/// Whether some session is queued for `RESOURCES_LOCK_KEY`.
async fn resources_lock_queued(pool: &PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted \
           AND classid::bigint = $1 AND objid::bigint = $2 AND objsubid = 1)",
    )
    .bind(RESOURCES_LOCK_KEY >> 32)
    .bind(RESOURCES_LOCK_KEY & 0xffff_ffff)
    .fetch_one(pool)
    .await
    .expect("read the queued locks")
}

/// Waits until a session is queued for the resources lock; fails after 10 s.
async fn until_resources_lock_queued(pool: &PgPool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !resources_lock_queued(pool).await {
        assert!(
            std::time::Instant::now() < deadline,
            "no session queued for the resources lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A provider-key document listing `groups`, as an operator posts it.
fn provider_key_body(display_name: &str, groups: &[&str]) -> Value {
    json!({"doc": {
        "display_name": display_name,
        "provider": "openai",
        "api_key": "sk-write-race-7c1",
        "groups": groups,
    }})
}

async fn post_provider_key(cp: &TestCp, who: &OperatorSession, body: Value) -> Resp {
    cp.send(
        "POST",
        "/admin/resources/provider_keys",
        Some(who.cookie()),
        Some(body),
    )
    .await
}

/// A provider-key write and a retire of the group it names, raced on the
/// resources lock, never end with a retired group named by a live provider
/// key: whichever gets the lock first, the other sees it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_key_write_and_a_retire_never_end_with_a_retired_group_named() {
    let Some(cp) = TestCp::start_with(|cfg| cfg.db_lock_timeout_ms = 60_000).await else {
        return;
    };
    let admin = cp
        .admin_login_seeded(aisix_control_plane::auth::OperatorRole::Operator)
        .await;
    let group = create_group(
        &cp,
        &admin,
        body("grp-write-race-7c1", "标准", json!("7.2")),
    )
    .await;
    let id = id_of(&group);

    // The write gets the resources lock first: it commits, and the retire
    // then counts it and refuses.
    {
        let mut holder = cp.db().pool().begin().await.expect("begin");
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(RESOURCES_LOCK_KEY)
            .execute(&mut *holder)
            .await
            .expect("hold the resources lock");
        let cookie = admin.cookie().to_string();
        let url = cp.url("/admin/resources/provider_keys");
        let write = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("an HTTP client")
                .post(url)
                .header(reqwest::header::COOKIE, cookie)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&provider_key_body(
                    "acct-first-7c1",
                    &["grp-write-race-7c1"],
                ))
                .send()
                .await
                .expect("send the write")
                .status()
                .as_u16()
        });
        until_resources_lock_queued(cp.db().pool()).await;
        // The retire takes the group row and then the resources lock, so it
        // is behind the write.
        let retire_cp = &cp;
        let cookie = admin.cookie().to_string();
        let retire_url = cp.url(&format!("{GROUPS}/{id}/retire"));
        let retire_request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("an HTTP client")
                .post(retire_url)
                .header(reqwest::header::COOKIE, cookie)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body("{}")
                .send()
                .await
                .expect("send the retire")
                .status()
                .as_u16()
        });
        let _ = retire_cp;
        holder.commit().await.expect("release the resources lock");
        assert_eq!(write.await.expect("join the write"), 201);
        assert_eq!(retire_request.await.expect("join the retire"), 409);
    }
    let r = rates(&cp, &admin, &id).await;
    assert_eq!(
        r["group"]["provider_keys"][0]["display_name"],
        "acct-first-7c1"
    );

    // The retire gets it first: it retires, and the write naming the group
    // is refused, so no live provider key is left naming a retired group.
    let group = create_group(
        &cp,
        &admin,
        body("grp-write-late-7c1", "标准", json!("7.2")),
    )
    .await;
    let id = id_of(&group);
    let r = post_provider_key(
        &cp,
        &admin,
        provider_key_body("acct-second-7c1", &["grp-write-late-7c1"]),
    )
    .await;
    assert_eq!(
        r.status, 201,
        "the group is live, so the account joins it: {r:?}"
    );
    // Clear it, so the group has no account and can retire.
    let pk = r.body["id"]
        .as_str()
        .expect("a provider key id")
        .to_string();
    let version: i64 = sqlx::query_scalar(
        "SELECT version FROM gw_resources WHERE kind = 'provider_keys' AND id = $1",
    )
    .bind(Uuid::parse_str(&pk).expect("a provider key id"))
    .fetch_one(cp.db().pool())
    .await
    .expect("the provider key's version");
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client")
        .delete(cp.url(&format!("/admin/resources/provider_keys/{pk}")))
        .header(reqwest::header::COOKIE, admin.cookie())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .send()
        .await
        .expect("send the delete");
    assert_eq!(response.status(), 200);
    let r = retire(&cp, &admin, &id).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = post_provider_key(
        &cp,
        &admin,
        provider_key_body("acct-third-7c1", &["grp-write-late-7c1"]),
    )
    .await;
    assert_422_at(&r, "/groups/0");
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE kind = 'provider_keys' AND NOT deleted \
           AND doc->'groups' ? 'grp-write-late-7c1'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("count the live provider keys");
    assert_eq!(live, 0, "no live provider key names the retired group");
}

/// The migration's own designation block, read from the file, applied as a
/// replay would apply it: the test pins the SQL that ships, not a copy.
async fn apply_designation(pool: &PgPool) -> Result<(), sqlx::Error> {
    let sql = std::fs::read_to_string(format!(
        "{}/migrations/0023_provider_groups.sql",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("read 0023");
    let start = sql
        .find("-- designation: begin")
        .expect("the block's start")
        + "-- designation: begin".len();
    let end = sql.find("-- designation: end").expect("the block's end");
    sqlx::raw_sql(&sql[start..end]).execute(pool).await?;
    Ok(())
}

/// The designation leaves a designated default, even when a database already
/// carries a retired group named `default` and the block runs again: a live
/// group becomes the default rather than none.
#[tokio::test]
async fn designation_leaves_a_live_default_even_when_default_is_retired() {
    let Some(pool) = database_at(23).await else {
        return;
    };
    // The state a replayed designation meets: a retired `default` that is
    // still flagged, and a live group that must become the designated one.
    sqlx::query(
        "UPDATE provider_groups SET retired_at = now(), is_default = true WHERE name = 'default'",
    )
    .execute(&pool)
    .await
    .expect("retire the default group");
    sqlx::query(
        "INSERT INTO provider_groups (name, display_name) VALUES ('grp-live-7c1', 'live-7c1')",
    )
    .execute(&pool)
    .await
    .expect("a live group");

    apply_designation(&pool)
        .await
        .expect("the designation leaves a default");

    let designated: Vec<String> =
        sqlx::query_scalar("SELECT name FROM provider_groups WHERE is_default ORDER BY name")
            .fetch_all(&pool)
            .await
            .expect("read the designated group");
    assert_eq!(
        designated,
        vec!["grp-live-7c1".to_string()],
        "a retired `default` does not block designating a live group"
    );
    drop_database(pool).await;
}
