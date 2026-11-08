//! Tenants on the operator side: what a tenant projects to the gateways (its
//! team, and its default limits as a rate-limit policy), how its grants,
//! groups and limits are checked, and how a model delete withdraws a grant.
//!
//! Distinctive tokens (`-7c1`, `grp-eu-5d2`) keep "not present" assertions
//! from passing by accident.

mod support;

use std::sync::Arc;

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::crypto::MasterKey;
use aisix_control_plane::db::DbError;
use aisix_control_plane::resources::validate::{validate_lenient, validate_strict};
use aisix_control_plane::resources::write::{WriteCtx, RESOURCES_LOCK_KEY};
use aisix_control_plane::resources::Kind;
use aisix_control_plane::tenants::{self, BillingMode, TenantLimits, TenantStatus};
use futures::FutureExt;
use serde_json::{json, Value};
use support::{Resp, TestCp, TEST_MASTER_KEY};
use uuid::Uuid;

/// A logged-in operator, with requests that can carry `If-Match`.
struct Admin<'a> {
    cp: &'a TestCp,
    id: Uuid,
    cookie: String,
}

impl Admin<'_> {
    async fn as_role(cp: &TestCp, role: OperatorRole) -> Admin<'_> {
        let session = cp.admin_login_seeded(role).await;
        Admin {
            cp,
            id: session.id,
            cookie: session.cookie().to_string(),
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        if_match: Option<&str>,
        body: Option<Value>,
    ) -> Resp {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("an HTTP client");
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
        // Labelled JSON with or without a body, as the Dashboard sends it.
        let mut request = client
            .request(method, self.cp.url(path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::COOKIE, &self.cookie);
        if let Some(v) = if_match {
            request = request.header(reqwest::header::IF_MATCH, v);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.expect("send the request");
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().await.expect("read the body");
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        Resp {
            status,
            body,
            set_cookie: None,
            headers,
        }
    }

    async fn get(&self, path: &str) -> Resp {
        self.send("GET", path, None, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Resp {
        self.send("POST", path, None, Some(body)).await
    }

    async fn put(&self, path: &str, body: Value) -> Resp {
        self.send("PUT", path, None, Some(body)).await
    }

    async fn delete(&self, path: &str, if_match: Option<&str>) -> Resp {
        self.send("DELETE", path, if_match, None).await
    }

    /// `PATCH /admin/tenants/{id}` with `If-Match: "<version>"`.
    async fn patch_tenant(&self, id: &str, version: i64, body: Value) -> Resp {
        self.send(
            "PATCH",
            &format!("/admin/tenants/{id}"),
            Some(&format!("\"{version}\"")),
            Some(body),
        )
        .await
    }

    /// `POST /admin/tenants/{id}/{action}`.
    async fn act(&self, id: &str, action: &str) -> Resp {
        self.send("POST", &format!("/admin/tenants/{id}/{action}"), None, None)
            .await
    }

    /// `GET /admin/tenants/{id}`, which must answer 200.
    async fn tenant(&self, id: &str) -> Value {
        let r = self.get(&format!("/admin/tenants/{id}")).await;
        assert_eq!(r.status, 200, "read tenant {id}: {r:?}");
        r.body
    }

    /// `POST /admin/tenants` with `name`, which must answer 201.
    async fn create_tenant(&self, name: &str) -> Value {
        let r = self.post("/admin/tenants", json!({ "name": name })).await;
        assert_eq!(r.status, 201, "create tenant {name}: {r:?}");
        r.body
    }

    /// `POST /admin/resources/{kind}` with `{doc}`, which must answer 201;
    /// the created resource's id.
    async fn create(&self, kind: &str, doc: Value) -> String {
        let r = self
            .post(&format!("/admin/resources/{kind}"), json!({ "doc": doc }))
            .await;
        assert_eq!(r.status, 201, "create {kind}: {r:?}");
        r.body["id"].as_str().expect("an id").to_string()
    }
}

fn ctx(cp: &TestCp) -> WriteCtx {
    WriteCtx {
        mk: MasterKey(TEST_MASTER_KEY),
        env: cp.env().clone(),
        cp_version: cp.state().cfg.cp_version.clone(),
        stale_secs: cp.state().cfg.instance_stale_secs as f64,
    }
}

fn id_of(tenant: &Value) -> String {
    tenant["id"].as_str().expect("a tenant id").to_string()
}

/// A stored gateway document, live or a tombstone.
#[derive(Debug)]
struct Row {
    doc: Value,
    version: i64,
    projected_version: Option<i64>,
    deleted: bool,
    managed_by: Option<String>,
}

type RawRow = (Value, i64, Option<i64>, bool, Option<String>);

async fn row(cp: &TestCp, kind: &str, id: &str) -> Option<Row> {
    let raw: Option<RawRow> = sqlx::query_as(
        "SELECT doc, version, projected_version, deleted, managed_by \
         FROM gw_resources WHERE kind = $1 AND id = $2::uuid",
    )
    .bind(kind)
    .bind(id)
    .fetch_optional(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("load {kind}/{id}: {e}"));
    raw.map(
        |(doc, version, projected_version, deleted, managed_by)| Row {
            doc,
            version,
            projected_version,
            deleted,
            managed_by,
        },
    )
}

/// The id the tenant's limits policy is written under, once minted.
async fn limits_policy_id(cp: &TestCp, tenant: &str) -> Option<Uuid> {
    sqlx::query_scalar("SELECT limits_policy_id FROM tenants WHERE id = $1::uuid")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the tenant")
}

/// How many rate-limit policies, live or not, `tenant` owns.
async fn owned_policies(cp: &TestCp, tenant: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE kind = 'rate_limit_policies' AND managed_by = $1",
    )
    .bind(format!("tenant:{tenant}"))
    .fetch_one(cp.db().pool())
    .await
    .expect("count policies")
}

/// The policy a tenant's default limits are projected as.
fn expected_policy(tenant: &str, limits: Value) -> Value {
    json!({
        "name": format!("tenant-{tenant}-limits"),
        "conditions": [{"dimension": "team", "operator": "in", "value": [tenant]}],
        "limits": limits,
    })
}

/// `(action, tenant_id)` of every audit row about tenants, in order.
async fn tenant_audit(cp: &TestCp) -> Vec<(String, Option<Uuid>, Option<String>)> {
    sqlx::query_as(
        "SELECT action, tenant_id, target_id FROM audit_log \
         WHERE target_kind = 'tenant' ORDER BY id",
    )
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log")
}

/// A tenant created the way self-service signup creates one: pending.
async fn pending_tenant(cp: &TestCp, name: &str) -> String {
    let ctx = ctx(cp);
    let name = name.to_string();
    let row = cp
        .db()
        .tx(move |conn| {
            async move {
                tenants::create(
                    conn,
                    &ctx,
                    &name,
                    TenantStatus::Pending,
                    BillingMode::Prepaid,
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect("create a pending tenant");
    row.id.to_string()
}

fn provider_key(name: &str, groups: Value) -> Value {
    json!({"display_name": name, "provider": "openai", "api_key": "sk-tenants-91d2", "groups": groups})
}

fn direct_model(name: &str, provider_key_id: &str) -> Value {
    json!({
        "display_name": name,
        "provider": "openai",
        "model_name": "gpt-4o",
        "provider_key_id": provider_key_id,
    })
}

#[tokio::test]
async fn an_operator_created_tenant_is_active_and_projects_its_team() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let t = admin.create_tenant("acme-7c1").await;
    let id = id_of(&t);
    assert_eq!(t["name"], "acme-7c1");
    assert_eq!(t["status"], "active");
    assert_eq!(t["is_internal"], false);
    assert_eq!(t["billing_mode"], "prepaid");
    assert_eq!(t["version"], 1);
    assert_eq!(t["allowed_model_ids"], json!([]));
    assert_eq!(t["member_count"], 0);
    assert_eq!(t["key_count"], 0);
    assert!(t["created_at"].is_string(), "{t}");
    assert!(t["approved_at"].is_string(), "{t}");
    let approved_by: Option<Uuid> =
        sqlx::query_scalar("SELECT approved_by FROM tenants WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant");
    assert_eq!(approved_by, Some(admin.id));

    // The gateways see the tenant as a team of the same id and name.
    let team = row(&cp, "teams", &id).await.expect("a team document");
    assert_eq!(team.doc, json!({"name": "acme-7c1"}));
    assert_eq!(team.managed_by, Some(format!("tenant:{id}")));
    assert!(!team.deleted);

    let r = admin.get(&format!("/admin/tenants/{id}")).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.headers["etag"], "\"1\"");
    assert_eq!(r.body, t);
    let listed = |r: &Resp| -> Vec<String> {
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(id_of)
            .collect()
    };
    let r = admin.get("/admin/tenants").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(listed(&r).contains(&id), "{r:?}");
    let r = admin.get("/admin/tenants?status=active").await;
    assert!(listed(&r).contains(&id), "{r:?}");
    let r = admin.get("/admin/tenants?status=pending").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(!listed(&r).contains(&id), "{r:?}");
    let r = admin.get("/admin/tenants?status=closed-7c1").await;
    assert_eq!(r.status, 422, "{r:?}");

    let audit = tenant_audit(&cp).await;
    let tenant_id = Uuid::parse_str(&id).expect("a uuid");
    assert_eq!(
        audit,
        [(
            "tenant.create".to_string(),
            Some(tenant_id),
            Some(id.clone())
        )]
    );

    let r = admin
        .post(
            "/admin/tenants",
            json!({"name": "acme-postpaid-7c1", "billing_mode": "postpaid"}),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["billing_mode"], "postpaid");

    // Refused requests create nothing.
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants WHERE NOT is_internal")
            .fetch_one(cp.db().pool())
            .await
            .expect("count tenants")
    };
    let before = count().await;
    for (body, path) in [
        (json!({"name": "   "}), Some("/name")),
        (json!({"name": "n".repeat(129)}), Some("/name")),
        (json!({"name": "bell\u{7}-7c1"}), Some("/name")),
        (json!({"name": "a", "billing_mode": "credit"}), None),
        (json!({"name": "a", "status": "pending"}), None),
        (json!({}), None),
    ] {
        let r = admin.post("/admin/tenants", body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        if let Some(path) = path {
            assert_eq!(r.body["details"][0]["path"], path, "{body}: {r:?}");
        }
    }
    assert_eq!(count().await, before);
}

#[tokio::test]
async fn a_new_tenant_starts_in_the_default_groups() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Admin).await;
    let r = admin.get("/admin/settings/tenant-defaults").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body,
        json!({"concurrency": 20, "rpm": 1200, "key_concurrency_max": 10, "default_provider_groups": ["shared"]})
    );
    let first = admin.create_tenant("first-7c1").await;
    assert_eq!(first["allowed_provider_groups"], json!(["shared"]));
    assert_eq!(
        first["limits"],
        json!({"concurrency": 20, "rpm": 1200, "key_concurrency_max": 10})
    );

    // Changed defaults reach the tenants created afterwards, and only them.
    let changed = json!({"rpm": 600, "tpm": 50000, "key_rpm_max": 60, "default_provider_groups": ["grp-eu-5d2", "shared"]});
    let r = admin
        .put("/admin/settings/tenant-defaults", changed.clone())
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body, changed);
    let second = admin.create_tenant("second-7c1").await;
    assert_eq!(
        second["allowed_provider_groups"],
        json!(["grp-eu-5d2", "shared"])
    );
    assert_eq!(
        second["limits"],
        json!({"rpm": 600, "tpm": 50000, "key_rpm_max": 60})
    );
    assert_eq!(admin.tenant(&id_of(&first)).await, first);

    // Refused defaults change nothing.
    for (body, path) in [
        (
            json!({"rpm": 0, "default_provider_groups": ["shared"]}),
            Some("/rpm"),
        ),
        // No group at all would open every upstream to new tenants, and so
        // would leaving the groups out.
        (
            json!({"default_provider_groups": []}),
            Some("/default_provider_groups"),
        ),
        (
            json!({"default_provider_groups": [" shared"]}),
            Some("/default_provider_groups/0"),
        ),
        (
            json!({"default_provider_groups": [""]}),
            Some("/default_provider_groups/0"),
        ),
        (
            json!({"default_provider_groups": ["shared", "g".repeat(65)]}),
            Some("/default_provider_groups/1"),
        ),
        (json!({"rpm": 5}), Some("/default_provider_groups")),
        (
            json!({"rpm": 5, "default_provider_groups": [], "groups": []}),
            None,
        ),
    ] {
        let r = admin
            .put("/admin/settings/tenant-defaults", body.clone())
            .await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        if let Some(path) = path {
            assert_eq!(r.body["details"][0]["path"], path, "{body}: {r:?}");
        }
    }
    let r = admin.get("/admin/settings/tenant-defaults").await;
    assert_eq!(r.body, changed);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'settings.tenant_defaults'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("count audit rows");
    assert_eq!(audited, 1);

    // A group named twice is stored once.
    let r = admin
        .put(
            "/admin/settings/tenant-defaults",
            json!({"rpm": 600, "default_provider_groups": ["shared", "grp-eu-5d2", "shared"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["default_provider_groups"],
        json!(["shared", "grp-eu-5d2"])
    );

    // Defaults a newer release stored, with a field this one does not know,
    // still serve the tenants created here.
    sqlx::query(
        "UPDATE cp_settings SET value = value || '{\"future_cap_7c1\": 3}' \
         WHERE key = 'tenant_defaults'",
    )
    .execute(cp.db().pool())
    .await
    .expect("store a newer field");
    let third = admin.create_tenant("third-7c1").await;
    assert_eq!(third["limits"], json!({"rpm": 600}));
    assert_eq!(
        third["allowed_provider_groups"],
        json!(["shared", "grp-eu-5d2"])
    );
    let r = admin.get("/admin/settings/tenant-defaults").await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// Defaults that name no group (an edit made outside the API, or a row
/// written before the API refused one) would open every upstream to each
/// new customer tenant, so no tenant is created from them.
#[tokio::test]
async fn a_tenant_is_never_created_from_defaults_that_name_no_group() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    sqlx::query(
        "UPDATE cp_settings SET value = jsonb_set(value, '{default_provider_groups}', '[]') \
         WHERE key = 'tenant_defaults'",
    )
    .execute(cp.db().pool())
    .await
    .expect("store defaults with no group");
    let counts = || async {
        sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT (SELECT count(*) FROM tenants), \
                    (SELECT count(*) FROM gw_resources WHERE kind = 'teams'), \
                    (SELECT count(*) FROM audit_log WHERE target_kind = 'tenant')",
        )
        .fetch_one(cp.db().pool())
        .await
        .expect("count")
    };
    let before = counts().await;

    let r = admin
        .post("/admin/tenants", json!({"name": "open-7c1"}))
        .await;
    assert_eq!(r.status, 500, "{r:?}");
    assert_eq!(r.body["code"], "internal");
    let ctx = ctx(&cp);
    let signup = cp
        .db()
        .tx(move |conn| {
            async move {
                tenants::create(
                    conn,
                    &ctx,
                    "open-signup-7c1",
                    TenantStatus::Pending,
                    BillingMode::Prepaid,
                    Actor::System,
                )
                .await
            }
            .boxed()
        })
        .await
        .expect_err("a pending tenant from the same defaults");
    assert_eq!(signup.code, "internal");
    assert_eq!(counts().await, before);
}

#[tokio::test]
async fn approving_a_pending_tenant_writes_its_default_limits_policy() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let id = pending_tenant(&cp, "signup-7c1").await;
    // A pending tenant is not served, so the gateways are given nothing of
    // it: no team, and no limit either.
    assert!(row(&cp, "teams", &id).await.is_none());
    assert_eq!(owned_policies(&cp, &id).await, 0);
    let t = admin.tenant(&id).await;
    assert_eq!(t["status"], "pending");
    assert_eq!(t["approved_at"], Value::Null);

    let r = admin.act(&id, "approve").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["status"], "active");
    assert_eq!(r.body["version"], 2);
    assert!(r.body["approved_at"].is_string(), "{r:?}");
    let approved_by: Option<Uuid> =
        sqlx::query_scalar("SELECT approved_by FROM tenants WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant");
    assert_eq!(approved_by, Some(admin.id));
    let team = row(&cp, "teams", &id).await.expect("a team document");
    assert_eq!(team.doc, json!({"name": "signup-7c1"}));
    assert_eq!(team.managed_by, Some(format!("tenant:{id}")));
    let pid = limits_policy_id(&cp, &id)
        .await
        .expect("a limits policy id");
    let policy = row(&cp, "rate_limit_policies", &pid.to_string())
        .await
        .expect("a limits policy");
    assert_eq!(
        policy.doc,
        expected_policy(&id, json!({"concurrency": 20, "rpm": 1200}))
    );
    assert_eq!(policy.managed_by, Some(format!("tenant:{id}")));
    assert!(!policy.deleted);
    assert_eq!(policy.projected_version, None);

    // The policy is the tenant's to change, not the generic API's.
    let r = admin
        .send(
            "PUT",
            &format!("/admin/resources/rate_limit_policies/{pid}"),
            Some("\"1\""),
            Some(json!({"doc": expected_policy(&id, json!({"rpm": 1}))})),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "managed_resource");

    // An active tenant is not approved twice.
    let r = admin.act(&id, "approve").await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(admin.tenant(&id).await["version"], 2);
    let tenant_id = Uuid::parse_str(&id).expect("a uuid");
    assert_eq!(
        tenant_audit(&cp).await,
        [("tenant.approve".to_string(), Some(tenant_id), Some(id))]
    );
}

#[tokio::test]
async fn clearing_every_tenant_limit_removes_its_policy() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let id = id_of(&admin.create_tenant("limits-7c1").await);
    let pid = limits_policy_id(&cp, &id)
        .await
        .expect("an active tenant with limits has a policy")
        .to_string();
    assert!(
        !row(&cp, "rate_limit_policies", &pid)
            .await
            .expect("a policy")
            .deleted
    );

    // Key caps alone are no tenant-wide limit.
    let r = admin
        .patch_tenant(&id, 1, json!({"limits": {"key_concurrency_max": 5}}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["limits"], json!({"key_concurrency_max": 5}));
    assert_eq!(r.body["version"], 2);
    assert!(
        row(&cp, "rate_limit_policies", &pid)
            .await
            .expect("a policy")
            .deleted
    );
    assert!(!row(&cp, "teams", &id).await.expect("a team").deleted);

    let r = admin
        .patch_tenant(&id, 2, json!({"limits": {"tpm": 1000}}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let policy = row(&cp, "rate_limit_policies", &pid)
        .await
        .expect("a policy");
    assert!(!policy.deleted);
    assert_eq!(policy.doc, expected_policy(&id, json!({"tpm": 1000})));

    let r = admin.patch_tenant(&id, 3, json!({"limits": {}})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["limits"], json!({}));
    assert!(
        row(&cp, "rate_limit_policies", &pid)
            .await
            .expect("a policy")
            .deleted
    );
    assert_eq!(
        owned_policies(&cp, &id).await,
        1,
        "one id, revived and retired"
    );

    // A limit of zero, or one no tenant has, is refused where it sits.
    for (limits, path) in [
        (json!({"concurrency": 0}), "/limits/concurrency"),
        (json!({"rpm": 60, "concurency": 5}), "/limits/concurency"),
        (json!({"rpm": -1}), "/limits"),
    ] {
        let r = admin
            .patch_tenant(&id, 4, json!({ "limits": limits }))
            .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{r:?}");
    }
    assert_eq!(admin.tenant(&id).await["version"], 4);

    // A PATCH that does not carry `limits` leaves them as stored, fields
    // this release does not know included.
    sqlx::query(
        "UPDATE tenants SET limits = '{\"rpm\": 60, \"future_limit_7c1\": 5}' WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(cp.db().pool())
    .await
    .expect("store a newer limit");
    let r = admin
        .patch_tenant(&id, 4, json!({"name": "limits-renamed-7c1"}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let stored: Value = sqlx::query_scalar("SELECT limits FROM tenants WHERE id = $1::uuid")
        .bind(&id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the limits");
    assert_eq!(stored, json!({"rpm": 60, "future_limit_7c1": 5}));
}

#[tokio::test]
async fn suspending_a_tenant_removes_its_policy_and_keeps_its_team() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let id = id_of(&admin.create_tenant("suspend-7c1").await);
    let pid = limits_policy_id(&cp, &id)
        .await
        .expect("a policy id")
        .to_string();
    let team_before = row(&cp, "teams", &id).await.expect("a team");

    let r = admin.act(&id, "suspend").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["status"], "suspended");
    assert_eq!(r.body["version"], 2);
    assert!(
        row(&cp, "rate_limit_policies", &pid)
            .await
            .expect("a policy")
            .deleted
    );
    let team = row(&cp, "teams", &id).await.expect("a team");
    assert!(!team.deleted);
    assert_eq!(
        (team.doc, team.version),
        (team_before.doc, team_before.version)
    );
    let suspended_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT suspended_at FROM tenants WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant");
    assert!(suspended_at.is_some());

    let r = admin.act(&id, "suspend").await;
    assert_eq!(r.status, 409, "{r:?}");

    // Limits changed while suspended are kept, and limit nothing yet.
    let r = admin
        .patch_tenant(&id, 2, json!({"limits": {"rpm": 30}}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        row(&cp, "rate_limit_policies", &pid)
            .await
            .expect("a policy")
            .deleted
    );

    let actions: Vec<String> = tenant_audit(&cp).await.into_iter().map(|a| a.0).collect();
    assert_eq!(
        actions,
        ["tenant.create", "tenant.suspend", "tenant.update"]
    );
}

#[tokio::test]
async fn reactivating_a_tenant_rewrites_its_limits_policy() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let id = id_of(&admin.create_tenant("reactivate-7c1").await);
    let pid = limits_policy_id(&cp, &id)
        .await
        .expect("a policy id")
        .to_string();
    assert_eq!(admin.act(&id, "suspend").await.status, 200);
    let gone = row(&cp, "rate_limit_policies", &pid)
        .await
        .expect("a policy");
    assert!(gone.deleted);

    let r = admin.act(&id, "reactivate").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["status"], "active");
    // The same id, revived, and due for projection.
    let policy = row(&cp, "rate_limit_policies", &pid)
        .await
        .expect("a policy");
    assert!(!policy.deleted);
    assert_eq!(policy.version, gone.version + 1);
    assert_eq!(
        policy.doc,
        expected_policy(&id, json!({"concurrency": 20, "rpm": 1200}))
    );
    assert_ne!(policy.projected_version, Some(policy.version));
    assert_eq!(limits_policy_id(&cp, &id).await, Uuid::parse_str(&pid).ok());

    // Each action moves one state to another, and only that one.
    assert_eq!(admin.act(&id, "reactivate").await.status, 409);
    assert_eq!(admin.act(&id, "approve").await.status, 409);
    assert_eq!(admin.act(&id, "suspend").await.status, 200);
    let r = admin.act(&id, "approve").await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(admin.tenant(&id).await["status"], "suspended");
    let pending = pending_tenant(&cp, "pending-7c1").await;
    assert_eq!(admin.act(&pending, "reactivate").await.status, 409);
    assert_eq!(admin.tenant(&pending).await["status"], "pending");
    // A tenant that does not exist is not found, whatever the action.
    for action in ["approve", "suspend", "reactivate"] {
        let r = admin.act(&Uuid::new_v4().to_string(), action).await;
        assert_eq!(r.status, 404, "{action}: {r:?}");
    }

    let actions: Vec<String> = tenant_audit(&cp).await.into_iter().map(|a| a.0).collect();
    assert_eq!(
        actions,
        [
            "tenant.create",
            "tenant.suspend",
            "tenant.reactivate",
            "tenant.suspend"
        ]
    );
}

#[tokio::test]
async fn a_wildcard_or_ensemble_model_cannot_be_granted_to_a_customer() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-grant-7c1", json!([])))
        .await;
    let direct = admin
        .create("models", direct_model("chat-grant-7c1", &pk))
        .await;
    let member = admin
        .create("models", direct_model("chat-member-7c1", &pk))
        .await;
    let mut wild = direct_model("wild-grant-*", &pk);
    wild["model_name"] = json!("*");
    let wildcard = admin.create("models", wild).await;
    let ensemble = admin
        .create(
            "models",
            json!({"display_name": "ensemble-grant-7c1", "ensemble": {
                "panel": [{"model": "chat-grant-7c1"}],
                "judge": {"model": "chat-grant-7c1"},
            }}),
        )
        .await;
    let gone = admin
        .create("models", direct_model("gone-grant-7c1", &pk))
        .await;
    let r = admin
        .delete(&format!("/admin/resources/models/{gone}"), Some("\"1\""))
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    let id = id_of(&admin.create_tenant("customer-7c1").await);
    let r = admin
        .patch_tenant(
            &id,
            1,
            json!({"allowed_model_ids": [
                direct, wildcard, ensemble, Uuid::new_v4(), "not-a-uuid-7c1", gone,
            ]}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    let paths: Vec<&str> = r.body["details"]
        .as_array()
        .expect("details")
        .iter()
        .map(|d| d["path"].as_str().expect("a path"))
        .collect();
    assert_eq!(
        paths,
        [
            "/allowed_model_ids/1",
            "/allowed_model_ids/2",
            "/allowed_model_ids/3",
            "/allowed_model_ids/4",
            "/allowed_model_ids/5",
        ]
    );
    let t = admin.tenant(&id).await;
    assert_eq!(
        (t["version"].clone(), t["allowed_model_ids"].clone()),
        (json!(1), json!([]))
    );

    let r = admin
        .patch_tenant(&id, 1, json!({"allowed_model_ids": [direct, direct]}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["allowed_model_ids"], json!([direct]));

    // Nor can a model a customer holds become one afterwards.
    let mut renamed = direct_model("chat-grant-*", &pk);
    renamed["model_name"] = json!("*");
    let regrouped = json!({"display_name": "chat-grant-7c1", "ensemble": {
        "panel": [{"model_id": member}],
        "judge": {"model_id": member},
    }});
    for (doc, path) in [(renamed, "/display_name"), (regrouped, "/ensemble")] {
        let r = admin
            .send(
                "PUT",
                &format!("/admin/resources/models/{direct}"),
                Some("\"1\""),
                Some(json!({ "doc": doc })),
            )
            .await;
        assert_eq!(r.status, 422, "{r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{r:?}");
        let model = row(&cp, "models", &direct).await.expect("the model");
        assert_eq!(
            (model.version, model.doc["display_name"].clone()),
            (1, json!("chat-grant-7c1"))
        );
    }

    // The platform's own tenant may use every model, but only live ones.
    let r = admin.get("/admin/tenants").await;
    let internal = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|t| t["is_internal"] == true)
        .expect("the internal tenant")
        .clone();
    let internal_id = id_of(&internal);
    let version = internal["version"].as_i64().expect("a version");
    let r = admin
        .patch_tenant(&internal_id, version, json!({"allowed_model_ids": [gone]}))
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/allowed_model_ids/0");
    let r = admin
        .patch_tenant(
            &internal_id,
            version,
            json!({"allowed_model_ids": [wildcard, ensemble]}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mut granted: Vec<String> = r.body["allowed_model_ids"]
        .as_array()
        .expect("ids")
        .iter()
        .map(|v| v.as_str().expect("an id").to_string())
        .collect();
    granted.sort();
    let mut expected = vec![wildcard.clone(), ensemble.clone()];
    expected.sort();
    assert_eq!(granted, expected);
    // A model only the platform's own tenant holds may change shape.
    let held_inside = admin
        .create("models", direct_model("chat-inside-7c1", &pk))
        .await;
    let version = r.body["version"].as_i64().expect("a version");
    let r = admin
        .patch_tenant(
            &internal_id,
            version,
            json!({"allowed_model_ids": [wildcard, ensemble, held_inside]}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let mut wild_inside = direct_model("chat-inside-*", &pk);
    wild_inside["model_name"] = json!("*");
    let r = admin
        .send(
            "PUT",
            &format!("/admin/resources/models/{held_inside}"),
            Some("\"1\""),
            Some(json!({ "doc": wild_inside })),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    // Group names are checked as well.
    let r = admin
        .patch_tenant(
            &id,
            2,
            json!({"allowed_provider_groups": ["grp-eu-5d2", " "]}),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/allowed_provider_groups/1");
    let r = admin
        .patch_tenant(&id, 2, json!({"allowed_provider_groups": ["grp-eu-5d2"]}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["allowed_provider_groups"], json!(["grp-eu-5d2"]));
    for (groups, path) in [
        // No group reaches every upstream: a customer names its groups.
        (json!([]), "/allowed_provider_groups"),
        (json!([" grp-eu-5d2"]), "/allowed_provider_groups/0"),
        (
            json!(["grp-eu-5d2", "shared "]),
            "/allowed_provider_groups/1",
        ),
    ] {
        let r = admin
            .patch_tenant(&id, 3, json!({ "allowed_provider_groups": groups }))
            .await;
        assert_eq!(r.status, 422, "{groups}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{groups}");
    }
    let r = admin
        .patch_tenant(
            &id,
            3,
            json!({"allowed_provider_groups": ["shared", "grp-eu-5d2", "shared"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["allowed_provider_groups"],
        json!(["shared", "grp-eu-5d2"])
    );
    // The platform's own tenant is meant to reach every upstream.
    let version = admin.tenant(&internal_id).await["version"]
        .as_i64()
        .expect("a version");
    let r = admin
        .patch_tenant(
            &internal_id,
            version,
            json!({"allowed_provider_groups": []}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["allowed_provider_groups"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_internal_tenant_exists_exactly_once_and_has_no_groups() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Viewer).await;
    let internal = |r: Resp| -> Vec<Value> {
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter(|t| t["is_internal"] == true)
            .cloned()
            .collect()
    };
    let found = internal(admin.get("/admin/tenants").await);
    assert_eq!(found.len(), 1, "{found:?}");
    let t = &found[0];
    let id = id_of(t);
    assert_eq!(t["name"], "平台内部");
    assert_eq!(t["status"], "active");
    assert_eq!(t["allowed_provider_groups"], json!([]));
    assert_eq!(t["limits"], json!({}));
    let team = row(&cp, "teams", &id).await.expect("a team document");
    assert_eq!(team.doc, json!({"name": "平台内部"}));
    assert_eq!(team.managed_by, Some(format!("tenant:{id}")));
    assert_eq!(owned_policies(&cp, &id).await, 0);

    // A later boot finds the same one.
    drop(cp.restart().await);
    let found = internal(admin.get("/admin/tenants").await);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0], *t);

    // Suspending it would stop every operator key.
    let operator = Admin::as_role(&cp, OperatorRole::Operator).await;
    let r = operator.act(&id, "suspend").await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "tenant_state");
    assert_eq!(operator.tenant(&id).await["status"], "active");

    // Two first boots at once create one between them.
    let internal_count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants WHERE is_internal")
            .fetch_one(cp.db().pool())
            .await
            .expect("count")
    };
    for round in 0..10 {
        sqlx::query(
            "DELETE FROM billing_accounts WHERE tenant_id IN (SELECT id FROM tenants WHERE is_internal)",
        )
        .execute(cp.db().pool())
        .await
        .expect("the internal tenant's account goes with it");
        sqlx::query("DELETE FROM tenants WHERE is_internal")
            .execute(cp.db().pool())
            .await
            .expect("back to a first boot");
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let boots: Vec<_> = (0..2)
            .map(|_| {
                let db = cp.db().clone();
                let ctx = ctx(&cp);
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    tenants::ensure_internal_tenant(&db, &ctx).await
                })
            })
            .collect();
        let mut ids = Vec::new();
        for boot in boots {
            let row = boot
                .await
                .expect("join")
                .unwrap_or_else(|e| panic!("round {round}: {e}"));
            ids.push(row.id);
        }
        assert_eq!(ids[0], ids[1], "round {round}");
        assert_eq!(internal_count().await, 1, "round {round}");
    }

    // The database refuses a second one outright.
    let e = sqlx::query(
        "INSERT INTO tenants (id, name, status, is_internal) VALUES ($1, 'x', 'active', true)",
    )
    .bind(Uuid::new_v4())
    .execute(cp.db().pool())
    .await
    .expect_err("a second internal tenant");
    assert!(
        matches!(DbError::from(e), DbError::UniqueViolation { constraint } if constraint == "tenants_one_internal")
    );
}

#[tokio::test]
async fn deleting_a_model_withdraws_it_from_every_tenant_grant() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-cascade-7c1", json!([])))
        .await;
    let gone = admin
        .create("models", direct_model("chat-gone-7c1", &pk))
        .await;
    let kept = admin
        .create("models", direct_model("chat-kept-7c1", &pk))
        .await;
    let mut ids = Vec::new();
    for (name, grants) in [
        ("both-7c1", json!([gone, kept])),
        ("gone-only-7c1", json!([gone])),
        ("kept-only-7c1", json!([kept])),
    ] {
        let id = id_of(&admin.create_tenant(name).await);
        let r = admin
            .patch_tenant(&id, 1, json!({ "allowed_model_ids": grants }))
            .await;
        assert_eq!(r.status, 200, "{r:?}");
        ids.push(id);
    }
    let (both, gone_only, kept_only) = (&ids[0], &ids[1], &ids[2]);
    let teams_before: Vec<i64> = {
        let mut v = Vec::new();
        for id in &ids {
            v.push(row(&cp, "teams", id).await.expect("a team").version);
        }
        v
    };

    let r = admin
        .delete(&format!("/admin/resources/models/{gone}"), Some("\"1\""))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let t = admin.tenant(both).await;
    assert_eq!(
        (t["allowed_model_ids"].clone(), t["version"].clone()),
        (json!([kept]), json!(3))
    );
    let t = admin.tenant(gone_only).await;
    assert_eq!(
        (t["allowed_model_ids"].clone(), t["version"].clone()),
        (json!([]), json!(3))
    );
    // A tenant that never held the model is not touched.
    let t = admin.tenant(kept_only).await;
    assert_eq!(
        (t["allowed_model_ids"].clone(), t["version"].clone()),
        (json!([kept]), json!(2))
    );
    for (id, before) in ids.iter().zip(&teams_before) {
        assert_eq!(
            row(&cp, "teams", id).await.expect("a team").version,
            *before
        );
    }

    // A model the configuration still uses is not deleted, and the grants
    // that hold it stay as they were.
    let policy = admin
        .create(
            "rate_limit_policies",
            json!({"name": "rl-kept-7c1", "scope": "model", "scope_ref": kept, "window": "minute", "max_requests": 60}),
        )
        .await;
    let r = admin
        .delete(&format!("/admin/resources/models/{kept}"), Some("\"1\""))
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "in_use");
    assert_eq!(
        r.body["referenced_by"],
        json!([{"kind": "rate_limit_policies", "id": policy, "name": "rl-kept-7c1"}])
    );
    let t = admin.tenant(both).await;
    assert_eq!(
        (t["allowed_model_ids"].clone(), t["version"].clone()),
        (json!([kept]), json!(3))
    );
    let t = admin.tenant(kept_only).await;
    assert_eq!(
        (t["allowed_model_ids"].clone(), t["version"].clone()),
        (json!([kept]), json!(2))
    );
}

#[tokio::test]
async fn tenant_names_may_repeat() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let first = id_of(&admin.create_tenant("dup-7c1").await);
    let second = id_of(&admin.create_tenant("dup-7c1").await);
    assert_ne!(first, second);
    for id in [&first, &second] {
        let team = row(&cp, "teams", id).await.expect("a team");
        assert_eq!(team.doc, json!({"name": "dup-7c1"}));
    }
    // Renaming one onto the other's name is fine too.
    let third = id_of(&admin.create_tenant("other-7c1").await);
    let r = admin
        .patch_tenant(&third, 1, json!({"name": "dup-7c1"}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        row(&cp, "teams", &third).await.expect("a team").doc,
        json!({"name": "dup-7c1"})
    );
}

#[tokio::test]
async fn provider_groups_are_summarised_from_provider_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let beta = admin
        .create(
            "provider_keys",
            provider_key("acct-beta-7c1", json!(["grp-b-7c1", "grp-a-7c1"])),
        )
        .await;
    let alpha = admin
        .create(
            "provider_keys",
            provider_key("acct-alpha-7c1", json!(["grp-a-7c1"])),
        )
        .await;
    admin
        .create("provider_keys", provider_key("acct-none-7c1", json!([])))
        .await;
    let gone = admin
        .create(
            "provider_keys",
            provider_key("acct-gone-7c1", json!(["grp-gone-7c1"])),
        )
        .await;
    let r = admin
        .delete(
            &format!("/admin/resources/provider_keys/{gone}"),
            Some("\"1\""),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");

    let viewer = Admin::as_role(&cp, OperatorRole::Viewer).await;
    let r = viewer.get("/admin/provider-groups").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body,
        json!({"items": [
            {"name": "grp-a-7c1", "provider_keys": [
                {"id": alpha, "display_name": "acct-alpha-7c1"},
                {"id": beta, "display_name": "acct-beta-7c1"},
            ]},
            {"name": "grp-b-7c1", "provider_keys": [
                {"id": beta, "display_name": "acct-beta-7c1"},
            ]},
        ]})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_tenant_patch_is_412() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let id = id_of(&admin.create_tenant("stale-7c1").await);
    let r = admin
        .patch_tenant(&id, 1, json!({"name": "fresh-7c1"}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["version"], 2);
    assert_eq!(
        row(&cp, "teams", &id).await.expect("a team").doc,
        json!({"name": "fresh-7c1"})
    );

    let r = admin
        .patch_tenant(&id, 1, json!({"name": "stale-again-7c1"}))
        .await;
    assert_eq!(r.status, 412, "{r:?}");
    assert_eq!(r.body["code"], "version_mismatch");
    assert_eq!(r.body["current_version"], 2);
    let r = admin
        .send(
            "PATCH",
            &format!("/admin/tenants/{id}"),
            None,
            Some(json!({"name": "unconditional-7c1"})),
        )
        .await;
    assert_eq!(r.status, 428, "{r:?}");
    let t = admin.tenant(&id).await;
    assert_eq!(
        (t["name"].clone(), t["version"].clone()),
        (json!("fresh-7c1"), json!(2))
    );
    assert_eq!(
        row(&cp, "teams", &id).await.expect("a team").doc,
        json!({"name": "fresh-7c1"})
    );

    // Two edits made from the same version at once: one wins, the other is
    // told the tenant moved on.
    let mut version = 2;
    for round in 0..20 {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let edits: Vec<_> = (0..2)
            .map(|i| {
                let barrier = barrier.clone();
                let admin = &admin;
                let id = id.clone();
                async move {
                    barrier.wait().await;
                    admin
                        .patch_tenant(&id, version, json!({"name": format!("race-{round}-{i}")}))
                        .await
                        .status
                }
            })
            .collect();
        let mut statuses = futures::future::join_all(edits).await;
        statuses.sort_unstable();
        assert_eq!(statuses, [200, 412], "round {round}");
        version += 1;
    }
    assert_eq!(admin.tenant(&id).await["version"], version);

    // A tenant that does not exist is not found, however its id is written.
    for path in [
        format!("/admin/tenants/{}", Uuid::new_v4()),
        "/admin/tenants/not-a-uuid".to_string(),
    ] {
        let r = admin.get(&path).await;
        assert_eq!(r.status, 404, "{path}: {r:?}");
        let r = admin
            .send("PATCH", &path, Some("\"1\""), Some(json!({"name": "x"})))
            .await;
        assert_eq!(r.status, 404, "{path}: {r:?}");
    }
}

#[tokio::test]
async fn a_tenant_view_carries_the_worst_rollout_of_its_documents() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let t = admin.create_tenant("rollout-7c1").await;
    let id = id_of(&t);
    let pid = limits_policy_id(&cp, &id)
        .await
        .expect("a policy id")
        .to_string();
    assert_eq!(t["rollout"]["state"], "pending");

    // What the projector records once it has written both documents.
    sqlx::query(
        "UPDATE gw_resources SET projected_version = version, kine_mod_revision = 41, \
         kine_revision = 41 WHERE managed_by = $1",
    )
    .bind(format!("tenant:{id}"))
    .execute(cp.db().pool())
    .await
    .expect("record a projection");
    let state = |r: &Resp| r.body["rollout"]["state"].clone();
    let team_path = format!("/admin/resources/teams/{id}");
    let policy_path = format!("/admin/resources/rate_limit_policies/{pid}");
    assert_eq!(state(&admin.get(&team_path).await), "no_gateways");
    assert_eq!(state(&admin.get(&policy_path).await), "no_gateways");
    let t = admin.tenant(&id).await;
    assert_eq!(t["rollout"]["state"], "no_gateways", "{t}");

    // A rename rewrites the team, which the projector has yet to write; the
    // policy it left alone is still where it was.
    let r = admin
        .patch_tenant(&id, 1, json!({"name": "rollout-renamed-7c1"}))
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(state(&admin.get(&team_path).await), "pending");
    assert_eq!(state(&admin.get(&policy_path).await), "no_gateways");
    assert_eq!(r.body["rollout"]["state"], "pending", "{r:?}");
    let listed = admin.get("/admin/tenants").await;
    let entry = listed.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|t| id_of(t) == id)
        .expect("the tenant")
        .clone();
    assert_eq!(entry["rollout"], r.body["rollout"]);
}

fn patient(cfg: &mut aisix_control_plane::config::CpConfig) {
    cfg.db_lock_timeout_ms = 120_000;
}

/// Sessions of this test's database waiting for a lock.
async fn lock_waiters(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("count lock waiters")
}

async fn until_lock_waiters(cp: &TestCp, n: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while lock_waiters(cp).await < n {
        assert!(
            std::time::Instant::now() < deadline,
            "{n} lock waiters never appeared"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Whether some other transaction holds the resources lock right now.
async fn resources_lock_held(cp: &TestCp) -> bool {
    let mut probe = cp.db().pool().begin().await.expect("begin");
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .fetch_one(&mut *probe)
        .await
        .expect("try the resources lock");
    probe.rollback().await.expect("end the probe");
    !free
}

/// `method path` as `cookie`, labelled JSON, with `If-Match` and a body
/// when given; the status.
async fn fire(
    cp: Arc<TestCp>,
    method: &'static str,
    path: String,
    cookie: String,
    if_match: Option<i64>,
    body: Option<Value>,
) -> u16 {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    let mut request = client
        .request(method, cp.url(&path))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::COOKIE, cookie);
    if let Some(v) = if_match {
        request = request.header(reqwest::header::IF_MATCH, format!("\"{v}\""));
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    request
        .send()
        .await
        .expect("send the request")
        .status()
        .as_u16()
}

/// A request in flight, as [`fire`] sends it: its status.
type Request = std::pin::Pin<Box<dyn std::future::Future<Output = u16> + Send>>;

/// Runs `request` while another transaction holds tenant `tenant`'s row,
/// and requires that, while it waits for the tenant, it holds no
/// resources lock: the lock order is the tenant first. Then lets it finish;
/// its status.
async fn waits_for_the_tenant_first(
    cp: &Arc<TestCp>,
    tenant: Uuid,
    what: &str,
    request: impl std::future::Future<Output = u16> + Send + 'static,
) -> u16 {
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the tenant");
    let request = tokio::spawn(request);
    until_lock_waiters(cp, 1).await;
    let inverted = resources_lock_held(cp).await;
    holder.rollback().await.expect("release the tenant");
    let status = request.await.expect("join");
    assert!(
        !inverted,
        "{what} took the resources lock before the tenant's: the lock order is inverted"
    );
    status
}

/// Every path that writes a tenant's documents takes the tenant's lock
/// before the resources lock, which `write_resource` takes: a path that
/// took them the other way round would deadlock with one that does not.
/// Each request below is sent while the tenant's row is held elsewhere,
/// and while it waits for that row the resources lock must be free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_tenant_write_takes_the_tenant_before_the_resources_lock() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let session = cp.admin_login_seeded(OperatorRole::Admin).await;
    let op = session.cookie().to_string();
    let admin = Admin::as_role(&cp, OperatorRole::Admin).await;
    let created = admin.create_tenant("order-7c1").await;
    let tenant = Uuid::parse_str(&id_of(&created)).expect("a uuid");
    let pk = admin
        .create(
            "provider_keys",
            provider_key("acct-order-7c1", json!(["shared"])),
        )
        .await;
    let model = admin
        .create("models", direct_model("order-model-7c1", &pk))
        .await;
    let r = admin
        .patch_tenant(
            &tenant.to_string(),
            1,
            json!({"allowed_model_ids": [model]}),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await;
    let leaver = cp.seed_member(tenant, MemberRole::Member).await;
    let r = admin
        .post(
            &format!("/admin/tenants/{tenant}/invites"),
            json!({"email": format!("order-{}@tenant.test", Uuid::new_v4().simple()), "role": "member"}),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let invite = r.body["invite_token"]
        .as_str()
        .expect("a token")
        .to_string();
    let mut topups = Vec::new();
    for _ in 0..2 {
        let r = cp
            .send(
                "POST",
                "/api/billing/topups",
                Some(owner.cookie()),
                Some(json!({"amount_usd": "0.05"})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        topups.push(r.body["id"].as_str().expect("an id").to_string());
    }

    let cases: Vec<(&str, Request, u16)> = vec![
        (
            "PATCH /admin/tenants/{id}",
            Box::pin(fire(
                cp.clone(),
                "PATCH",
                format!("/admin/tenants/{tenant}"),
                op.clone(),
                Some(2),
                Some(json!({"name": "order-renamed-7c1"})),
            )),
            200,
        ),
        (
            "POST /admin/tenants/{id}/keys",
            Box::pin(fire(
                cp.clone(),
                "POST",
                format!("/admin/tenants/{tenant}/keys"),
                op.clone(),
                None,
                Some(json!({"label": "order-op-key-7c1"})),
            )),
            201,
        ),
        (
            "POST /api/keys",
            Box::pin(fire(
                cp.clone(),
                "POST",
                "/api/keys".to_string(),
                owner.cookie().to_string(),
                None,
                Some(json!({"label": "order-key-7c1"})),
            )),
            201,
        ),
        (
            "PATCH /admin/members/{id}",
            Box::pin(fire(
                cp.clone(),
                "PATCH",
                format!("/admin/members/{}", member.id),
                op.clone(),
                None,
                Some(json!({"disabled": true})),
            )),
            200,
        ),
        (
            "DELETE /api/members/{id}",
            Box::pin(fire(
                cp.clone(),
                "DELETE",
                format!("/api/members/{}", leaver.id),
                owner.cookie().to_string(),
                None,
                None,
            )),
            204,
        ),
        (
            "POST /api/invites/accept",
            Box::pin(fire(
                cp.clone(),
                "POST",
                "/api/invites/accept".to_string(),
                String::new(),
                None,
                Some(json!({"token": invite, "password": "order-password-7c1"})),
            )),
            201,
        ),
        (
            "POST /admin/tenants/{id}/suspend",
            Box::pin(fire(
                cp.clone(),
                "POST",
                format!("/admin/tenants/{tenant}/suspend"),
                op.clone(),
                None,
                None,
            )),
            200,
        ),
        (
            "POST /admin/tenants/{id}/reactivate",
            Box::pin(fire(
                cp.clone(),
                "POST",
                format!("/admin/tenants/{tenant}/reactivate"),
                op.clone(),
                None,
                None,
            )),
            200,
        ),
        // A model delete withdraws the model from its holders' grants, so it
        // locks those tenants first.
        (
            "DELETE /admin/resources/models/{id}",
            Box::pin(fire(
                cp.clone(),
                "DELETE",
                format!("/admin/resources/models/{model}"),
                op.clone(),
                Some(1),
                None,
            )),
            200,
        ),
        // A credit limit settles the balance mark, which writes a document.
        (
            "PATCH /admin/tenants/{id}/billing",
            Box::pin(fire(
                cp.clone(),
                "PATCH",
                format!("/admin/tenants/{tenant}/billing"),
                op.clone(),
                None,
                Some(json!({"credit_limit_usd": "1.00"})),
            )),
            200,
        ),
        // An approval posts to the ledger and settles the balance mark.
        (
            "POST /admin/billing/topups/{id}/approve",
            Box::pin(fire(
                cp.clone(),
                "POST",
                format!("/admin/billing/topups/{}/approve", topups[0]),
                op.clone(),
                None,
                Some(json!({})),
            )),
            200,
        ),
        (
            "POST /admin/billing/topups/{id}/reject",
            Box::pin(fire(
                cp.clone(),
                "POST",
                format!("/admin/billing/topups/{}/reject", topups[1]),
                op.clone(),
                None,
                Some(json!({"reason": "未收到款项"})),
            )),
            200,
        ),
        // A credit posts to the ledger and settles the balance mark.
        (
            "POST /admin/tenants/{id}/credits",
            Box::pin(fire(
                cp.clone(),
                "POST",
                format!("/admin/tenants/{tenant}/credits"),
                op.clone(),
                None,
                Some(json!({"amount_usd": "0.05", "reason": "线下收款 7c1"})),
            )),
            201,
        ),
    ];
    for (what, request, expected) in cases {
        let status = waits_for_the_tenant_first(&cp, tenant, what, request).await;
        assert_eq!(status, expected, "{what}");
    }
}

/// Several tenants are locked in ascending id order, whatever order they
/// are named in, so two transactions locking overlapping sets cannot
/// deadlock. While the higher id is held elsewhere, a transaction locking
/// both has already taken the lower one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn several_tenants_are_locked_in_ascending_id_order() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let mut created = Vec::new();
    for name in ["lock-a-7c1", "lock-b-7c1", "lock-c-7c1"] {
        created.push(Uuid::parse_str(&id_of(&admin.create_tenant(name).await)).expect("a uuid"));
    }
    let mut sorted = created.clone();
    sorted.sort();
    let (low, high) = (sorted[0], sorted[2]);

    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(high)
        .execute(&mut *holder)
        .await
        .expect("hold the highest tenant");
    let locking = tokio::spawn({
        let cp = cp.clone();
        let mut ids = vec![sorted[2], sorted[0], sorted[1], sorted[0]];
        async move {
            cp.db()
                .tx(move |conn| {
                    async move {
                        let rows = tenants::lock_many(conn, &mut ids).await?;
                        Ok::<_, aisix_control_plane::error::ApiError>((ids, rows))
                    }
                    .boxed()
                })
                .await
        }
    });
    until_lock_waiters(&cp, 1).await;
    // The lower ids are taken already: a probe cannot have the lowest.
    let mut probe = cp.db().pool().begin().await.expect("begin");
    let taken = sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE NOWAIT")
        .bind(low)
        .execute(&mut *probe)
        .await
        .is_err();
    probe.rollback().await.expect("end the probe");
    holder.rollback().await.expect("release the highest tenant");
    let locked = locking.await.expect("join").expect("lock");
    assert!(
        taken,
        "the lowest id was not locked while the highest was awaited: not in ascending order"
    );
    assert_eq!(locked.0, sorted);
    assert_eq!(locked.1.iter().map(|t| t.id).collect::<Vec<_>>(), sorted);
    let missing = cp
        .db()
        .tx(move |conn| {
            async move { tenants::lock_many(conn, &mut vec![Uuid::new_v4()]).await }.boxed()
        })
        .await
        .expect_err("a tenant that does not exist");
    assert_eq!(missing.code, "not_found");
}

/// A grant racing the delete of the model it names: whichever commits
/// first, no tenant is left holding a model that is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_racing_its_models_delete_leaves_no_grant_of_a_deleted_model() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let pk = admin
        .create("provider_keys", provider_key("acct-race-7c1", json!([])))
        .await;
    let id = id_of(&admin.create_tenant("race-7c1").await);
    let (mut deleted, mut granted) = (0, 0);
    for round in 0..30 {
        let model = admin
            .create("models", direct_model(&format!("chat-race-{round}"), &pk))
            .await;
        let version = admin.tenant(&id).await["version"]
            .as_i64()
            .expect("a version");
        let barrier = tokio::sync::Barrier::new(2);
        let grant = async {
            barrier.wait().await;
            admin
                .patch_tenant(&id, version, json!({"allowed_model_ids": [model]}))
                .await
                .status
        };
        let delete = async {
            barrier.wait().await;
            admin
                .delete(&format!("/admin/resources/models/{model}"), Some("\"1\""))
                .await
                .status
        };
        let (grant, delete) = tokio::join!(grant, delete);
        assert!(matches!(grant, 200 | 422), "round {round}: grant {grant}");
        assert!(
            matches!(delete, 200 | 503),
            "round {round}: delete {delete}"
        );
        if delete == 200 {
            deleted += 1;
        }
        if grant == 200 {
            granted += 1;
        }
        let dangling: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM tenants t, unnest(t.allowed_model_ids) AS m(id) \
             WHERE NOT EXISTS (SELECT 1 FROM gw_resources r \
               WHERE r.kind = 'models' AND r.id = m.id AND NOT r.deleted)",
        )
        .fetch_one(cp.db().pool())
        .await
        .expect("look for dangling grants");
        assert_eq!(dangling, 0, "round {round}: grant {grant}, delete {delete}");
    }
    eprintln!("deletes {deleted}, grants {granted} of 30 rounds");
}

/// A tenant's limits policy name is the tenant's: an operator's policy that
/// took it would keep the tenant's own from being written.
#[tokio::test]
async fn an_operator_cannot_take_a_tenants_limits_policy_name() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = Admin::as_role(&cp, OperatorRole::Operator).await;
    let id = id_of(&admin.create_tenant("reserved-7c1").await);
    assert_eq!(admin.act(&id, "suspend").await.status, 200);
    let taken = json!({
        "name": format!("tenant-{id}-limits"),
        "conditions": [{"dimension": "team", "operator": "in", "value": [id]}],
        "limits": {"rpm": 1},
    });
    let r = admin
        .post(
            "/admin/resources/rate_limit_policies",
            json!({ "doc": taken }),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/name");
    // Any tenant's, whether or not it exists.
    let mut other = taken.clone();
    other["name"] = json!(format!("tenant-{}-limits", Uuid::new_v4()));
    let r = admin
        .post(
            "/admin/resources/rate_limit_policies",
            json!({ "doc": other }),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    // Renaming an operator's policy onto it is refused the same way.
    let mut own = taken.clone();
    own["name"] = json!("rl-operator-7c1");
    let policy = admin.create("rate_limit_policies", own).await;
    let r = admin
        .send(
            "PUT",
            &format!("/admin/resources/rate_limit_policies/{policy}"),
            Some("\"1\""),
            Some(json!({ "doc": taken })),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/name");
    // The tenant's own policy is written again on reactivation.
    let r = admin.act(&id, "reactivate").await;
    assert_eq!(r.status, 200, "{r:?}");
    let pid = limits_policy_id(&cp, &id)
        .await
        .expect("a policy id")
        .to_string();
    let policy = row(&cp, "rate_limit_policies", &pid)
        .await
        .expect("a policy");
    assert!(!policy.deleted);
    assert_eq!(policy.doc["name"], format!("tenant-{id}-limits"));
}

#[test]
fn every_tenant_derived_document_loads_on_the_lenient_schema() {
    let tenant = Uuid::new_v4();
    let mut docs = vec![
        (Kind::Teams, tenants::team_doc("平台内部")),
        (Kind::Teams, tenants::team_doc("acme-7c1")),
    ];
    let full = TenantLimits {
        concurrency: Some(20),
        rpm: Some(1200),
        tpm: Some(50_000),
        key_concurrency_max: Some(10),
        key_rpm_max: Some(60),
    };
    let only_tpm = TenantLimits {
        tpm: Some(1000),
        ..TenantLimits::default()
    };
    for limits in [&full, &only_tpm] {
        let doc = tenants::limits_policy_doc(tenant, limits).expect("a policy");
        docs.push((Kind::RateLimitPolicies, doc));
    }
    assert_eq!(
        tenants::limits_policy_doc(tenant, &only_tpm),
        Some(expected_policy(&tenant.to_string(), json!({"tpm": 1000})))
    );
    // Key caps alone, or nothing, project no policy.
    let caps_only = TenantLimits {
        key_concurrency_max: Some(10),
        key_rpm_max: Some(60),
        ..TenantLimits::default()
    };
    assert_eq!(tenants::limits_policy_doc(tenant, &caps_only), None);
    assert_eq!(
        tenants::limits_policy_doc(tenant, &TenantLimits::default()),
        None
    );
    for (kind, doc) in docs {
        validate_strict(kind, &doc).unwrap_or_else(|e| panic!("{kind:?} strict: {e:?} {doc}"));
        validate_lenient(kind, &doc).unwrap_or_else(|e| panic!("{kind:?} lenient: {e} {doc}"));
    }
}
