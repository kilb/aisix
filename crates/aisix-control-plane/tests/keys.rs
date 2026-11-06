//! Tenant API keys: minting, the gateway document each key projects, and
//! how keys follow their tenant's and their member's state.
//!
//! Distinctive tokens (`-7c1`, `grp-eu-5d2`) keep "not present" assertions
//! from passing by accident.

mod support;

use std::collections::BTreeSet;

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::keys::{self, KEY_DOC_VERSION};
use aisix_control_plane::resources::write::WriteCtx;
use aisix_control_plane::tenants::TenantStatus;
use aisix_core::models::ApiKey;
use serde_json::{json, Value};
use support::members::MemberSession;
use support::{DpClient, OperatorSession, Resp, TestCp};
use uuid::Uuid;

/// `method path`, with `cookie` (a `Cookie` header value), `If-Match` and a
/// JSON body, each when given.
async fn request(
    cp: &TestCp,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    if_match: Option<&str>,
    body: Option<Value>,
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    let mut request = client.request(method, cp.url(path));
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    if let Some(v) = if_match {
        request = request.header(reqwest::header::IF_MATCH, v);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    request.send().await.expect("send the request")
}

async fn call(
    cp: &TestCp,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    if_match: Option<&str>,
    body: Option<Value>,
) -> Resp {
    let response = request(cp, method, path, cookie, if_match, body).await;
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

/// An answer's status, `Content-Type` and body bytes, for comparing two
/// answers byte for byte.
async fn raw(
    cp: &TestCp,
    method: &str,
    path: &str,
    cookie: &str,
    if_match: Option<&str>,
    body: Option<Value>,
) -> (u16, Option<String>, Vec<u8>) {
    let response = request(cp, method, path, Some(cookie), if_match, body).await;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .map(|v| v.to_str().expect("an ASCII Content-Type").to_string());
    let bytes = response.bytes().await.expect("read the body").to_vec();
    (status, content_type, bytes)
}

fn text(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} in {v}"))
        .to_string()
}

async fn audit_rows(cp: &TestCp) -> i64 {
    cp.count("audit_log").await
}

/// Creates a provider key in the shared group and a direct model on it for
/// each of `names`; the models' ids, in order.
async fn models(cp: &TestCp, admin: &OperatorSession, names: &[&str]) -> Vec<String> {
    let pk = call(
        cp,
        "POST",
        "/admin/resources/provider_keys",
        Some(admin.cookie()),
        None,
        Some(json!({"doc": {
            "display_name": format!("acct-keys-{}", Uuid::new_v4().simple()),
            "provider": "openai",
            "api_key": "sk-keys-upstream-7c1",
            "groups": ["shared"],
        }})),
    )
    .await;
    assert_eq!(pk.status, 201, "{pk:?}");
    let pk = text(&pk.body, "/id");
    let mut ids = Vec::new();
    for name in names {
        let r = call(
            cp,
            "POST",
            "/admin/resources/models",
            Some(admin.cookie()),
            None,
            Some(json!({"doc": {
                "display_name": name,
                "provider": "openai",
                "model_name": "gpt-4o-mini",
                "provider_key_id": pk,
            }})),
        )
        .await;
        assert_eq!(r.status, 201, "create model {name}: {r:?}");
        ids.push(text(&r.body, "/id"));
    }
    ids
}

/// `PATCH /admin/tenants/{tenant}` with `body`, at the tenant's current
/// version.
async fn patch_tenant(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, body: Value) -> Resp {
    let version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the tenant");
    call(
        cp,
        "PATCH",
        &format!("/admin/tenants/{tenant}"),
        Some(admin.cookie()),
        Some(&format!("\"{version}\"")),
        Some(body),
    )
    .await
}

async fn patch_tenant_ok(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, body: Value) {
    let r = patch_tenant(cp, admin, tenant, body.clone()).await;
    assert_eq!(r.status, 200, "patch tenant with {body}: {r:?}");
}

/// `POST /api/keys` as `by`.
async fn mint(cp: &TestCp, by: &MemberSession, body: Value) -> Resp {
    call(cp, "POST", "/api/keys", Some(by.cookie()), None, Some(body)).await
}

/// `POST /api/keys` as `by`, which must answer 201: the key's id and the
/// plaintext.
async fn mint_ok(cp: &TestCp, by: &MemberSession, body: Value) -> (String, String) {
    let r = mint(cp, by, body.clone()).await;
    assert_eq!(r.status, 201, "mint {body}: {r:?}");
    (text(&r.body, "/key/id"), text(&r.body, "/plaintext"))
}

/// `POST /admin/tenants/{tenant}/keys` as `admin`.
async fn admin_mint(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, body: Value) -> Resp {
    call(
        cp,
        "POST",
        &format!("/admin/tenants/{tenant}/keys"),
        Some(admin.cookie()),
        None,
        Some(body),
    )
    .await
}

/// The stored version of key `id`.
async fn key_version(cp: &TestCp, id: &str) -> i64 {
    sqlx::query_scalar("SELECT version FROM tenant_keys WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key")
}

/// `method /api/keys/{id}` as `by`, at the key's current version.
async fn on_key(
    cp: &TestCp,
    by: &MemberSession,
    method: &str,
    id: &str,
    body: Option<Value>,
) -> Resp {
    let version = key_version(cp, id).await;
    call(
        cp,
        method,
        &format!("/api/keys/{id}"),
        Some(by.cookie()),
        Some(&format!("\"{version}\"")),
        body,
    )
    .await
}

/// `method /admin/keys/{id}` as `admin`, at the key's current version.
async fn on_admin_key(
    cp: &TestCp,
    admin: &OperatorSession,
    method: &str,
    id: &str,
    body: Option<Value>,
) -> Resp {
    let version = key_version(cp, id).await;
    call(
        cp,
        method,
        &format!("/admin/keys/{id}"),
        Some(admin.cookie()),
        Some(&format!("\"{version}\"")),
        body,
    )
    .await
}

/// A stored gateway document: its document, whether it is a tombstone, its
/// `seq` and its `managed_by`.
#[derive(Debug)]
struct Stored {
    doc: Value,
    deleted: bool,
    seq: i64,
    managed_by: Option<String>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

async fn stored(cp: &TestCp, kind: &str, id: &str) -> Stored {
    type Raw = (
        Value,
        bool,
        i64,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
    );
    let (doc, deleted, seq, managed_by, updated_at): Raw = sqlx::query_as(
        "SELECT doc, deleted, seq, managed_by, updated_at FROM gw_resources \
         WHERE kind = $1 AND id = $2::uuid",
    )
    .bind(kind)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("load {kind}/{id}: {e}"));
    Stored {
        doc,
        deleted,
        seq,
        managed_by,
        updated_at,
    }
}

/// The live `api_keys` document of key `id`.
async fn key_doc(cp: &TestCp, id: &str) -> Value {
    let row = stored(cp, "api_keys", id).await;
    assert!(!row.deleted, "api_keys/{id} is deleted");
    row.doc
}

/// The tenant's limits policy id.
async fn policy_id(cp: &TestCp, tenant: Uuid) -> String {
    let id: Option<Uuid> = sqlx::query_scalar("SELECT limits_policy_id FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the tenant");
    id.expect("a limits policy id").to_string()
}

fn sorted(ids: &[&str]) -> Value {
    let mut ids: Vec<&str> = ids.to_vec();
    ids.sort();
    json!(ids)
}

async fn admin_patch_member(cp: &TestCp, admin: &OperatorSession, id: Uuid, body: Value) -> Resp {
    call(
        cp,
        "PATCH",
        &format!("/admin/members/{id}"),
        Some(admin.cookie()),
        None,
        Some(body),
    )
    .await
}

async fn act(cp: &TestCp, admin: &OperatorSession, tenant: Uuid, action: &str) -> Resp {
    call(
        cp,
        "POST",
        &format!("/admin/tenants/{tenant}/{action}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn the_plaintext_is_returned_once_and_never_stored() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;

    let r = mint(&cp, &owner, json!({"label": "ci-key-7c1"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let plaintext = text(&r.body, "/plaintext");
    let id = text(&r.body, "/key/id");
    let hex = plaintext
        .strip_prefix("sk-aisix-")
        .unwrap_or_else(|| panic!("{plaintext}"));
    assert_eq!(hex.len(), 64, "{plaintext}");
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{plaintext}");
    assert_eq!(r.body["key"]["prefix"], json!(plaintext[..12]));
    assert_eq!(r.body["key"]["label"], "ci-key-7c1");
    assert_eq!(r.body["key"]["version"], 1);
    let hash = ApiKey::hash_bearer(&plaintext);

    // Only the hash is kept, in the key row and in its gateway document.
    let (stored_hash, prefix): (String, String) =
        sqlx::query_as("SELECT key_hash, key_prefix FROM tenant_keys WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the key");
    assert_eq!(
        (stored_hash, prefix),
        (hash.clone(), plaintext[..12].to_string())
    );
    let doc = stored(&cp, "api_keys", &id).await;
    assert_eq!(doc.doc["key_hash"], json!(hash));
    assert_eq!(doc.managed_by, Some(format!("key:{id}")));
    for table in ["tenant_keys", "gw_resources", "audit_log", "sessions"] {
        let dump: Vec<String> = sqlx::query_scalar(&format!("SELECT t::text FROM {table} t"))
            .fetch_all(cp.db().pool())
            .await
            .unwrap_or_else(|e| panic!("dump {table}: {e}"));
        assert!(
            dump.iter().all(|row| !row.contains(hex)),
            "{table} holds the plaintext"
        );
    }
    let audit: (String, String, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT action, target_kind, target_id, tenant_id FROM audit_log \
         WHERE action = 'key.create'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("one key.create row");
    assert_eq!(
        audit,
        (
            "key.create".to_string(),
            "api_key".to_string(),
            Some(id.clone()),
            Some(tenant)
        )
    );

    // No read shows it again, nor the hash.
    let reads = [
        call(&cp, "GET", "/api/keys", Some(owner.cookie()), None, None).await,
        call(
            &cp,
            "GET",
            &format!("/api/keys/{id}"),
            Some(owner.cookie()),
            None,
            None,
        )
        .await,
        call(
            &cp,
            "GET",
            &format!("/admin/tenants/{tenant}/keys"),
            Some(admin.cookie()),
            None,
            None,
        )
        .await,
        call(
            &cp,
            "GET",
            &format!("/admin/keys/{id}"),
            Some(admin.cookie()),
            None,
            None,
        )
        .await,
    ];
    for read in reads {
        assert_eq!(read.status, 200, "{read:?}");
        let body = read.body.to_string();
        assert!(!body.contains(hex), "a read shows the plaintext: {body}");
        assert!(!body.contains(&hash), "a read shows the hash: {body}");
        assert!(body.contains(&id), "{body}");
    }
}

#[tokio::test]
async fn a_member_sees_and_edits_only_their_own_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let admin = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    let m1 = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let m2 = cp.member_login_seeded(tenant, MemberRole::Member).await;

    let (k1, _) = mint_ok(&cp, &m1, json!({"label": "m1-own-7c1"})).await;
    let (k2, _) = mint_ok(&cp, &m2, json!({"label": "m2-own-7c1"})).await;
    let (k3, _) = mint_ok(&cp, &owner, json!({"label": "owner-own-7c1"})).await;
    // An owner may hand a key to a member, or keep one bound to nobody.
    let (k4, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "for-m1-7c1", "member_id": m1.id}),
    )
    .await;
    let (k5, _) = mint_ok(
        &cp,
        &admin,
        json!({"label": "nobodys-7c1", "member_id": null}),
    )
    .await;

    let listed = |r: &Resp| -> BTreeSet<String> {
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|k| text(k, "/id"))
            .collect()
    };
    let r = call(&cp, "GET", "/api/keys", Some(m1.cookie()), None, None).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(listed(&r), BTreeSet::from([k1.clone(), k4.clone()]));
    let mine = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|k| k["id"] == json!(k4))
        .expect("k4")
        .clone();
    assert_eq!(mine["owner"]["id"], json!(m1.id));
    for manager in [&owner, &admin] {
        let r = call(&cp, "GET", "/api/keys", Some(manager.cookie()), None, None).await;
        assert_eq!(
            listed(&r),
            BTreeSet::from([k1.clone(), k2.clone(), k3.clone(), k4.clone(), k5.clone()])
        );
    }
    let r = call(
        &cp,
        "GET",
        &format!("/api/keys/{k5}"),
        Some(owner.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["owner"], Value::Null, "{r:?}");

    // Another member's key, an unbound one and a manager's answer as a key
    // that does not exist, and change nothing.
    let before = audit_rows(&cp).await;
    let random = Uuid::new_v4().to_string();
    for (method, body) in [
        ("GET", None),
        ("PATCH", Some(json!({"label": "taken-7c1"}))),
        ("DELETE", None),
    ] {
        let unknown = raw(
            &cp,
            method,
            &format!("/api/keys/{random}"),
            m1.cookie(),
            Some("\"1\""),
            body.clone(),
        )
        .await;
        assert_eq!(unknown.0, 404, "{method}");
        for other in [&k2, &k3, &k5] {
            let theirs = raw(
                &cp,
                method,
                &format!("/api/keys/{other}"),
                m1.cookie(),
                Some("\"1\""),
                body.clone(),
            )
            .await;
            assert_eq!(theirs, unknown, "{method} {other}");
        }
    }
    assert_eq!(audit_rows(&cp).await, before);
    assert_eq!(key_version(&cp, &k2).await, 1);
    assert_eq!(key_doc(&cp, &k2).await["display_name"], "m2-own-7c1");

    // A member mints only for themselves.
    for member_id in [json!(m2.id), json!(null), json!(Uuid::new_v4())] {
        let r = mint(
            &cp,
            &m1,
            json!({"label": "not-mine-7c1", "member_id": member_id}),
        )
        .await;
        assert_eq!(r.status, 403, "{member_id}: {r:?}");
        assert_eq!(r.body["code"], "forbidden");
    }
    let (k6, _) = mint_ok(
        &cp,
        &m1,
        json!({"label": "mine-again-7c1", "member_id": m1.id}),
    )
    .await;
    assert_eq!(key_doc(&cp, &k6).await["user_id"], json!(m1.id));

    // Their own keys they edit and revoke, whoever minted them.
    let r = on_key(
        &cp,
        &m1,
        "PATCH",
        &k4,
        Some(json!({"label": "renamed-7c1"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        (r.body["label"].clone(), r.body["version"].clone()),
        (json!("renamed-7c1"), json!(2))
    );
    assert_eq!(key_doc(&cp, &k4).await["display_name"], "renamed-7c1");
    let r = on_key(&cp, &m1, "DELETE", &k1, None).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert!(stored(&cp, "api_keys", &k1).await.deleted);
    let r = call(&cp, "GET", "/api/keys", Some(m1.cookie()), None, None).await;
    assert_eq!(listed(&r), BTreeSet::from([k4.clone(), k6.clone()]));
}

#[tokio::test]
async fn a_billing_member_cannot_list_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let billing = cp.member_login_seeded(tenant, MemberRole::Billing).await;
    let (key, _) = mint_ok(&cp, &owner, json!({"label": "seen-by-owner-7c1"})).await;
    let before = audit_rows(&cp).await;
    for (method, path, body) in [
        ("GET", "/api/keys".to_string(), None),
        ("GET", format!("/api/keys/{key}"), None),
        (
            "POST",
            "/api/keys".to_string(),
            Some(json!({"label": "b-7c1"})),
        ),
        (
            "PATCH",
            format!("/api/keys/{key}"),
            Some(json!({"label": "b-7c1"})),
        ),
        ("DELETE", format!("/api/keys/{key}"), None),
    ] {
        let r = call(
            &cp,
            method,
            &path,
            Some(billing.cookie()),
            Some("\"1\""),
            body,
        )
        .await;
        assert_eq!(r.status, 403, "{method} {path}: {r:?}");
        assert_eq!(r.body["code"], "forbidden");
    }
    assert_eq!(audit_rows(&cp).await, before);
    assert_eq!(cp.count("tenant_keys").await, 1);
}

#[tokio::test]
async fn a_key_may_not_exceed_its_tenants_grants_or_caps() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let ids = models(&cp, &admin, &["chat-a-7c1", "chat-b-7c1", "chat-c-7c1"]).await;
    let (m1, m2, m3) = (&ids[0], &ids[1], &ids[2]);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    patch_tenant_ok(
        &cp,
        &admin,
        tenant,
        json!({
            "allowed_model_ids": [m1, m2],
            "limits": {"concurrency": 20, "rpm": 1200, "key_concurrency_max": 5, "key_rpm_max": 60},
        }),
    )
    .await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let before = (audit_rows(&cp).await, cp.count("gw_resources").await);

    let long = "k".repeat(65);
    for (body, path) in [
        (
            json!({"label": "x-7c1", "model_ids": [m1, m3]}),
            "/model_ids/1",
        ),
        (
            json!({"label": "x-7c1", "model_ids": ["not-an-id"]}),
            "/model_ids/0",
        ),
        (
            json!({"label": "x-7c1", "model_ids": [Uuid::new_v4()]}),
            "/model_ids/0",
        ),
        (
            json!({"label": "x-7c1", "rate_limit": {"concurrency": 6}}),
            "/rate_limit/concurrency",
        ),
        (
            json!({"label": "x-7c1", "rate_limit": {"rpm": 61}}),
            "/rate_limit/rpm",
        ),
        (
            json!({"label": "x-7c1", "rate_limit": {"concurrency": 0}}),
            "/rate_limit/concurrency",
        ),
        (
            json!({"label": "x-7c1", "expires_at": "2001-01-01T00:00:00Z"}),
            "/expires_at",
        ),
        (
            json!({"label": "x-7c1", "expires_at": "tomorrow"}),
            "/expires_at",
        ),
        (json!({"label": "  "}), "/label"),
        (json!({"label": long}), "/label"),
        (json!({"label": "a\u{7}b"}), "/label"),
    ] {
        let r = mint(&cp, &owner, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["code"], "validation_failed", "{body}");
        assert_eq!(r.body["details"][0]["path"], path, "{body}: {r:?}");
        assert_eq!(r.body["details"].as_array().map(Vec::len), Some(1), "{r:?}");
    }
    // Fields only operators set are unknown here.
    for body in [
        json!({"label": "x-7c1", "allowed_provider_groups": ["grp-eu-5d2"]}),
        json!({"label": "x-7c1", "allowed_models": ["*"]}),
        json!({"label": "x-7c1", "rate_limit": {"tpm": 5}}),
    ] {
        let r = mint(&cp, &owner, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
    }
    assert_eq!(
        (audit_rows(&cp).await, cp.count("gw_resources").await),
        before
    );
    assert_eq!(cp.count("tenant_keys").await, 0);

    // Within the grants and the caps.
    let (picked, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "picked-7c1", "model_ids": [m2, m1, m2], "rate_limit": {"concurrency": 5, "rpm": 60}}),
    )
    .await;
    let doc = key_doc(&cp, &picked).await;
    assert_eq!(doc["allowed_model_ids"], sorted(&[m1, m2]));
    assert_eq!(doc["rate_limit"], json!({"concurrency": 5, "rpm": 60}));
    // A key that asks for nothing is given the caps.
    let (plain, _) = mint_ok(&cp, &owner, json!({"label": "plain-7c1"})).await;
    let doc = key_doc(&cp, &plain).await;
    assert_eq!(doc["rate_limit"], json!({"concurrency": 5, "rpm": 60}));
    assert_eq!(doc["allowed_model_ids"], sorted(&[m1, m2]));

    // The same checks on an update, which changes nothing when refused.
    for (body, path) in [
        (json!({"model_ids": [m3]}), "/model_ids/0"),
        (json!({"rate_limit": {"rpm": 61}}), "/rate_limit/rpm"),
        (json!({"label": ""}), "/label"),
    ] {
        let r = on_key(&cp, &owner, "PATCH", &picked, Some(body.clone())).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{body}");
    }
    assert_eq!(key_version(&cp, &picked).await, 1);
    // Clearing a selection follows the grants again; clearing the limits
    // falls back to the caps.
    let r = on_key(
        &cp,
        &owner,
        "PATCH",
        &picked,
        Some(json!({"model_ids": null, "rate_limit": {"rpm": 30}})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["model_ids"], Value::Null);
    assert_eq!(r.body["rate_limit"], json!({"rpm": 30}));
    let doc = key_doc(&cp, &picked).await;
    assert_eq!(doc["rate_limit"], json!({"concurrency": 5, "rpm": 30}));
    // A field left out keeps what is stored.
    let r = on_key(
        &cp,
        &owner,
        "PATCH",
        &picked,
        Some(json!({"label": "kept-7c1"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["rate_limit"], json!({"rpm": 30}));
    assert_eq!(r.body["model_ids"], Value::Null);
}

#[tokio::test]
async fn a_key_cannot_be_bound_to_another_tenants_member() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant_a = cp.seed_tenant(TenantStatus::Active).await;
    let tenant_b = cp.seed_tenant(TenantStatus::Active).await;
    let owner_a = cp.member_login_seeded(tenant_a, MemberRole::Owner).await;
    let member_b = cp.seed_member(tenant_b, MemberRole::Member).await;
    let leaver = cp.member_login_seeded(tenant_a, MemberRole::Member).await;
    let r = call(
        &cp,
        "DELETE",
        &format!("/api/members/{}", leaver.id),
        Some(owner_a.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 204, "{r:?}");
    let before = (audit_rows(&cp).await, cp.count("gw_resources").await);

    let body = |member: Value| json!({"label": "bound-7c1", "member_id": member});
    let unknown = raw(
        &cp,
        "POST",
        "/api/keys",
        owner_a.cookie(),
        None,
        Some(body(json!(Uuid::new_v4()))),
    )
    .await;
    assert_eq!(unknown.0, 422);
    let answer: Value = serde_json::from_slice(&unknown.2).expect("a JSON body");
    assert_eq!(answer["details"][0]["path"], "/member_id", "{answer}");
    for member in [json!(member_b.id), json!(leaver.id)] {
        let r = raw(
            &cp,
            "POST",
            "/api/keys",
            owner_a.cookie(),
            None,
            Some(body(member.clone())),
        )
        .await;
        assert_eq!(r, unknown, "{member}");
    }
    let malformed = mint(&cp, &owner_a, body(json!("not-an-id"))).await;
    assert_eq!(malformed.body["details"][0]["path"], "/member_id");

    // B is unchanged, and nothing was written.
    assert_eq!(
        (audit_rows(&cp).await, cp.count("gw_resources").await),
        before
    );
    assert_eq!(cp.count("tenant_keys").await, 0);
    let b_members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM members WHERE tenant_id = $1 AND removed_at IS NULL",
    )
    .bind(tenant_b)
    .fetch_one(cp.db().pool())
    .await
    .expect("count B's members");
    assert_eq!(b_members, 1);
}

#[tokio::test]
async fn another_tenants_key_id_is_not_found() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant_a = cp.seed_tenant(TenantStatus::Active).await;
    let tenant_b = cp.seed_tenant(TenantStatus::Active).await;
    let owner_a = cp.member_login_seeded(tenant_a, MemberRole::Owner).await;
    let owner_b = cp.member_login_seeded(tenant_b, MemberRole::Owner).await;
    let (theirs, _) = mint_ok(&cp, &owner_b, json!({"label": "b-key-7c1"})).await;
    let doc_before = stored(&cp, "api_keys", &theirs).await;
    let before = audit_rows(&cp).await;

    let random = Uuid::new_v4().to_string();
    for (method, if_match, body) in [
        ("GET", None, None),
        ("PATCH", Some("\"1\""), Some(json!({"label": "stolen-7c1"}))),
        ("PATCH", None, Some(json!({"disabled": true}))),
        ("DELETE", Some("\"1\""), None),
        ("DELETE", None, None),
    ] {
        let unknown = raw(
            &cp,
            method,
            &format!("/api/keys/{random}"),
            owner_a.cookie(),
            if_match,
            body.clone(),
        )
        .await;
        assert_eq!(unknown.0, 404, "{method} {if_match:?}");
        let foreign = raw(
            &cp,
            method,
            &format!("/api/keys/{theirs}"),
            owner_a.cookie(),
            if_match,
            body,
        )
        .await;
        assert_eq!(foreign, unknown, "{method} {if_match:?}");
    }
    let malformed = raw(
        &cp,
        "GET",
        "/api/keys/not-an-id",
        owner_a.cookie(),
        None,
        None,
    )
    .await;
    let unknown = raw(
        &cp,
        "GET",
        &format!("/api/keys/{random}"),
        owner_a.cookie(),
        None,
        None,
    )
    .await;
    assert_eq!(malformed, unknown);
    let listed = call(&cp, "GET", "/api/keys", Some(owner_a.cookie()), None, None).await;
    assert_eq!(listed.body["items"], json!([]));

    // B's key is as it was.
    assert_eq!(audit_rows(&cp).await, before);
    assert_eq!(key_version(&cp, &theirs).await, 1);
    let doc_after = stored(&cp, "api_keys", &theirs).await;
    assert_eq!(
        (doc_after.seq, doc_after.deleted, doc_after.doc),
        (doc_before.seq, false, doc_before.doc)
    );
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM tenant_keys WHERE id = $1::uuid")
            .bind(&theirs)
            .fetch_one(cp.db().pool())
            .await
            .expect("read B's key");
    assert!(!revoked);
}

#[tokio::test]
async fn disabling_a_member_disables_their_keys_and_re_enabling_restores_them_unless_the_owner_disabled_one(
) {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let _owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let (k1, _) = mint_ok(&cp, &member, json!({"label": "k1-7c1"})).await;
    let (k2, _) = mint_ok(&cp, &member, json!({"label": "k2-7c1"})).await;
    let r = on_key(&cp, &member, "PATCH", &k2, Some(json!({"disabled": true}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &k1).await["disabled"], false);
    assert_eq!(key_doc(&cp, &k2).await["disabled"], true);

    let r = admin_patch_member(&cp, &admin, member.id, json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &k1).await["disabled"], true);
    assert_eq!(key_doc(&cp, &k2).await["disabled"], true);
    // In the member's own transaction: the key document was written at the
    // same transaction time as the audit row of the disable.
    let disabled_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT at FROM audit_log WHERE action = 'member.disable'")
            .fetch_one(cp.db().pool())
            .await
            .expect("the disable's audit row");
    assert_eq!(stored(&cp, "api_keys", &k1).await.updated_at, disabled_at);
    // The keys' own settings are unchanged.
    assert_eq!(key_version(&cp, &k1).await, 1);
    let r = call(
        &cp,
        "GET",
        &format!("/admin/keys/{k1}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(
        (
            r.body["disabled"].clone(),
            r.body["effective_disabled"].clone()
        ),
        (json!(false), json!(true))
    );
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{tenant}/keys"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    let listed = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|k| k["id"] == json!(k1))
        .expect("k1 listed")
        .clone();
    assert_eq!(listed["effective_disabled"], true, "{listed}");
    // Re-projecting the tenant's keys keeps a disabled member's closed.
    patch_tenant_ok(&cp, &admin, tenant, json!({"limits": {"rpm": 600}})).await;
    assert_eq!(key_doc(&cp, &k1).await["disabled"], true);

    let r = admin_patch_member(&cp, &admin, member.id, json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &k1).await["disabled"], false);
    assert_eq!(key_doc(&cp, &k2).await["disabled"], true);
}

#[tokio::test]
async fn reactivating_a_tenant_keeps_keys_its_owner_disabled() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (open, _) = mint_ok(&cp, &owner, json!({"label": "open-7c1"})).await;
    let (closed, _) = mint_ok(&cp, &owner, json!({"label": "closed-7c1"})).await;
    let r = on_key(
        &cp,
        &owner,
        "PATCH",
        &closed,
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");

    let r = act(&cp, &admin, tenant, "suspend").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &open).await["disabled"], true);
    assert_eq!(key_doc(&cp, &closed).await["disabled"], true);

    let r = act(&cp, &admin, tenant, "reactivate").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &open).await["disabled"], false);
    assert_eq!(key_doc(&cp, &closed).await["disabled"], true);
    let r = call(&cp, "GET", "/api/keys", Some(owner.cookie()), None, None).await;
    let states: BTreeSet<(String, bool)> = r.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|k| (text(k, "/label"), k["disabled"].as_bool().expect("a flag")))
        .collect();
    assert_eq!(
        states,
        BTreeSet::from([
            ("closed-7c1".to_string(), true),
            ("open-7c1".to_string(), false)
        ])
    );
}

/// A tenant's limit brackets its keys: a suspension closes every key before
/// the limits policy goes, and an approval or a reactivation writes the
/// policy before any key opens, so a projection that stalls between two
/// writes never leaves a key open and unlimited.
#[tokio::test]
async fn a_tenants_limit_is_written_before_its_keys_open_and_retired_after_they_close() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (k1, _) = mint_ok(&cp, &owner, json!({"label": "k1-7c1"})).await;
    let (k2, _) = mint_ok(&cp, &owner, json!({"label": "k2-7c1"})).await;
    let policy = policy_id(&cp, tenant).await;

    let r = act(&cp, &admin, tenant, "suspend").await;
    assert_eq!(r.status, 200, "{r:?}");
    let tombstone = stored(&cp, "rate_limit_policies", &policy).await;
    assert!(tombstone.deleted);
    for k in [&k1, &k2] {
        let key = stored(&cp, "api_keys", k).await;
        assert_eq!(key.doc["disabled"], true);
        assert!(
            key.seq < tombstone.seq,
            "key {k} closed at {} after the policy went at {}",
            key.seq,
            tombstone.seq
        );
    }

    let r = act(&cp, &admin, tenant, "reactivate").await;
    assert_eq!(r.status, 200, "{r:?}");
    let revived = stored(&cp, "rate_limit_policies", &policy).await;
    assert!(!revived.deleted);
    for k in [&k1, &k2] {
        let key = stored(&cp, "api_keys", k).await;
        assert_eq!(key.doc["disabled"], false);
        assert!(revived.seq < key.seq, "key {k} opened before the policy");
    }

    // A pending tenant's keys, which an operator minted, open only after the
    // approval writes the policy.
    let pending = cp.seed_tenant(TenantStatus::Pending).await;
    let r = admin_mint(&cp, &admin, pending, json!({"label": "early-7c1"})).await;
    assert_eq!(r.status, 201, "{r:?}");
    let early = text(&r.body, "/key/id");
    assert_eq!(key_doc(&cp, &early).await["disabled"], true);
    let r = act(&cp, &admin, pending, "approve").await;
    assert_eq!(r.status, 200, "{r:?}");
    let policy = stored(&cp, "rate_limit_policies", &policy_id(&cp, pending).await).await;
    let key = stored(&cp, "api_keys", &early).await;
    assert_eq!(key.doc["disabled"], false);
    assert!(policy.seq < key.seq, "the key opened before the policy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_change_racing_a_cap_change_never_exceeds_the_new_cap() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let caps = |max: u32| json!({"limits": {"concurrency": 20, "key_concurrency_max": max}});
    patch_tenant_ok(&cp, &admin, tenant, caps(10)).await;
    let (key, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "racer-7c1", "rate_limit": {"concurrency": 4}}),
    )
    .await;

    let mut orders = BTreeSet::new();
    for round in 0..12 {
        let key_version = key_version(&cp, &key).await;
        let tenant_version: i64 = sqlx::query_scalar("SELECT version FROM tenants WHERE id = $1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("read the tenant");
        let (key_path, key_tag) = (format!("/api/keys/{key}"), format!("\"{key_version}\""));
        let (tenant_path, tenant_tag) = (
            format!("/admin/tenants/{tenant}"),
            format!("\"{tenant_version}\""),
        );
        let raise = call(
            &cp,
            "PATCH",
            &key_path,
            Some(owner.cookie()),
            Some(&key_tag),
            Some(json!({"rate_limit": {"concurrency": 8}})),
        );
        let lower = call(
            &cp,
            "PATCH",
            &tenant_path,
            Some(admin.cookie()),
            Some(&tenant_tag),
            Some(caps(5)),
        );
        let (raised, lowered) = tokio::join!(raise, lower);
        assert_eq!(lowered.status, 200, "round {round}: {lowered:?}");
        assert!(
            raised.status == 200 || raised.status == 422,
            "round {round}: {raised:?}"
        );
        orders.insert(raised.status);
        let doc = key_doc(&cp, &key).await;
        let concurrency = doc["rate_limit"]["concurrency"]
            .as_u64()
            .unwrap_or_else(|| panic!("round {round}: {doc}"));
        assert!(
            concurrency <= 5,
            "round {round}: the key may run {concurrency} at once under a cap of 5"
        );
        // Back to where the round started.
        patch_tenant_ok(&cp, &admin, tenant, caps(10)).await;
        let r = on_key(
            &cp,
            &owner,
            "PATCH",
            &key,
            Some(json!({"rate_limit": {"concurrency": 4}})),
        )
        .await;
        assert_eq!(r.status, 200, "round {round}: {r:?}");
    }
    eprintln!("key change answers seen: {orders:?}");
}

#[tokio::test]
async fn changing_a_tenants_grants_reprojects_its_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let ids = models(&cp, &admin, &["grant-a-7c1", "grant-b-7c1"]).await;
    let (m1, m2) = (&ids[0], &ids[1]);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    patch_tenant_ok(&cp, &admin, tenant, json!({"allowed_model_ids": [m1]})).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (all, _) = mint_ok(&cp, &owner, json!({"label": "all-7c1"})).await;
    let (picked, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "picked-7c1", "model_ids": [m1]}),
    )
    .await;
    assert_eq!(key_doc(&cp, &all).await["allowed_model_ids"], json!([m1]));

    patch_tenant_ok(&cp, &admin, tenant, json!({"allowed_model_ids": [m1, m2]})).await;
    assert_eq!(
        key_doc(&cp, &all).await["allowed_model_ids"],
        sorted(&[m1, m2])
    );
    assert_eq!(
        key_doc(&cp, &picked).await["allowed_model_ids"],
        json!([m1])
    );

    // A key that loses its last model keeps the list, empty: an absent list
    // would fall back to names.
    patch_tenant_ok(&cp, &admin, tenant, json!({"allowed_model_ids": [m2]})).await;
    assert_eq!(key_doc(&cp, &all).await["allowed_model_ids"], json!([m2]));
    assert_eq!(key_doc(&cp, &picked).await["allowed_model_ids"], json!([]));
    patch_tenant_ok(&cp, &admin, tenant, json!({"allowed_model_ids": []})).await;
    for k in [&all, &picked] {
        let doc = key_doc(&cp, k).await;
        assert_eq!(doc.get("allowed_model_ids"), Some(&json!([])), "{doc}");
        assert!(doc.get("allowed_models").is_none(), "{doc}");
    }
    // The selection itself is kept, so a grant given back serves it again.
    patch_tenant_ok(&cp, &admin, tenant, json!({"allowed_model_ids": [m1]})).await;
    assert_eq!(
        key_doc(&cp, &picked).await["allowed_model_ids"],
        json!([m1])
    );

    // Groups and caps follow too.
    patch_tenant_ok(
        &cp,
        &admin,
        tenant,
        json!({"allowed_provider_groups": ["grp-eu-5d2"], "limits": {"key_rpm_max": 30}}),
    )
    .await;
    for k in [&all, &picked] {
        let doc = key_doc(&cp, k).await;
        assert_eq!(
            doc["allowed_provider_groups"],
            json!(["grp-eu-5d2"]),
            "{doc}"
        );
        assert_eq!(doc["rate_limit"], json!({"rpm": 30}), "{doc}");
    }
    // A rename touches no key document.
    let before = stored(&cp, "api_keys", &all).await.seq;
    patch_tenant_ok(&cp, &admin, tenant, json!({"name": "renamed-7c1"})).await;
    assert_eq!(stored(&cp, "api_keys", &all).await.seq, before);
}

#[tokio::test]
async fn deleting_a_model_withdraws_it_from_every_grant_and_key() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let ids = models(&cp, &admin, &["gone-7c1", "kept-7c1"]).await;
    let (gone, kept) = (&ids[0], &ids[1]);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    patch_tenant_ok(
        &cp,
        &admin,
        tenant,
        json!({"allowed_model_ids": [gone, kept]}),
    )
    .await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (both, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "both-7c1", "model_ids": [gone, kept]}),
    )
    .await;
    let (only, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "only-7c1", "model_ids": [gone]}),
    )
    .await;
    let (all, _) = mint_ok(&cp, &owner, json!({"label": "all-7c1"})).await;
    // A tenant whose grant was withdrawn while its key still selects the
    // model.
    let other = cp.seed_tenant(TenantStatus::Active).await;
    patch_tenant_ok(&cp, &admin, other, json!({"allowed_model_ids": [gone]})).await;
    let other_owner = cp.member_login_seeded(other, MemberRole::Owner).await;
    let (stale, _) = mint_ok(
        &cp,
        &other_owner,
        json!({"label": "stale-7c1", "model_ids": [gone]}),
    )
    .await;
    patch_tenant_ok(&cp, &admin, other, json!({"allowed_model_ids": []})).await;

    let r = call(
        &cp,
        "DELETE",
        &format!("/admin/resources/models/{gone}"),
        Some(admin.cookie()),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");

    let view = |id: &str| {
        let path = format!("/admin/keys/{id}");
        let cookie = admin.cookie().to_string();
        let cp = &cp;
        async move {
            let r = call(cp, "GET", &path, Some(&cookie), None, None).await;
            assert_eq!(r.status, 200, "{r:?}");
            r.body
        }
    };
    assert_eq!(view(&both).await["model_ids"], json!([kept]));
    assert_eq!(view(&only).await["model_ids"], json!([]));
    assert_eq!(view(&all).await["model_ids"], Value::Null);
    assert_eq!(view(&stale).await["model_ids"], json!([]));
    assert_eq!(key_version(&cp, &both).await, 2);
    assert_eq!(key_version(&cp, &only).await, 2);
    assert_eq!(key_version(&cp, &stale).await, 2);
    assert_eq!(key_version(&cp, &all).await, 1);
    assert_eq!(
        key_doc(&cp, &both).await["allowed_model_ids"],
        json!([kept])
    );
    assert_eq!(key_doc(&cp, &only).await["allowed_model_ids"], json!([]));
    assert_eq!(key_doc(&cp, &all).await["allowed_model_ids"], json!([kept]));
    assert_eq!(key_doc(&cp, &stale).await["allowed_model_ids"], json!([]));
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{tenant}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["allowed_model_ids"], json!([kept]));
    // No live document names the deleted model any more.
    let naming: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gw_resources WHERE NOT deleted AND doc::text LIKE '%' || $1 || '%'",
    )
    .bind(gone)
    .fetch_one(cp.db().pool())
    .await
    .expect("search the documents");
    assert_eq!(naming, 0);
}

#[tokio::test]
async fn removing_a_member_revokes_their_keys_before_their_user_document_goes() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let (k1, _) = mint_ok(&cp, &member, json!({"label": "leaving-1-7c1"})).await;
    let (k2, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "leaving-2-7c1", "member_id": member.id}),
    )
    .await;
    let (kept, _) = mint_ok(&cp, &owner, json!({"label": "staying-7c1"})).await;

    let r = call(
        &cp,
        "DELETE",
        &format!("/api/members/{}", member.id),
        Some(owner.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 204, "{r:?}");
    let user = stored(&cp, "users", &member.id.to_string()).await;
    assert!(user.deleted);
    for k in [&k1, &k2] {
        let key = stored(&cp, "api_keys", k).await;
        assert!(key.deleted, "{k}");
        assert!(key.seq < user.seq, "key {k} went after its user document");
        let revoked: bool = sqlx::query_scalar(
            "SELECT revoked_at IS NOT NULL FROM tenant_keys WHERE id = $1::uuid",
        )
        .bind(k)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key");
        assert!(revoked, "{k}");
    }
    assert!(!stored(&cp, "api_keys", &kept).await.deleted);
    let remaining = |r: &Resp| -> Vec<String> {
        r.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|k| text(k, "/id"))
            .collect()
    };
    let r = call(&cp, "GET", "/api/keys", Some(owner.cookie()), None, None).await;
    assert_eq!(remaining(&r), vec![kept.clone()]);
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{tenant}/keys"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(remaining(&r), vec![kept.clone()]);
    let r = call(
        &cp,
        "GET",
        &format!("/admin/keys/{k1}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 404, "{r:?}");
    let detail: Value =
        sqlx::query_scalar("SELECT detail FROM audit_log WHERE action = 'member.remove'")
            .fetch_one(cp.db().pool())
            .await
            .expect("the removal's audit row");
    assert_eq!(detail["keys_revoked"], sorted(&[&k1, &k2]), "{detail}");
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{tenant}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["key_count"], 1, "{r:?}");
}

#[tokio::test]
async fn tenant_key_views_name_no_provider_group() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    patch_tenant_ok(
        &cp,
        &admin,
        tenant,
        json!({"allowed_provider_groups": ["grp-eu-5d2"]}),
    )
    .await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let created = mint(&cp, &owner, json!({"label": "plain-7c1"})).await;
    assert_eq!(created.status, 201, "{created:?}");
    let plain = text(&created.body, "/key/id");
    let (dedicated, _) = mint_ok(&cp, &owner, json!({"label": "dedicated-7c1"})).await;
    let r = on_admin_key(
        &cp,
        &admin,
        "PATCH",
        &dedicated,
        Some(json!({"allowed_provider_groups": ["grp-dedicated-9f3"]})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        r.body["allowed_provider_groups"],
        json!(["grp-dedicated-9f3"])
    );
    assert_eq!(
        r.body["effective_provider_groups"],
        json!(["grp-dedicated-9f3"])
    );
    assert_eq!(
        key_doc(&cp, &dedicated).await["allowed_provider_groups"],
        json!(["grp-dedicated-9f3"])
    );
    let r = call(
        &cp,
        "GET",
        &format!("/admin/keys/{plain}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["allowed_provider_groups"], Value::Null, "{r:?}");
    assert_eq!(r.body["effective_provider_groups"], json!(["grp-eu-5d2"]));

    let patched = on_key(
        &cp,
        &owner,
        "PATCH",
        &dedicated,
        Some(json!({"label": "dedicated-2-7c1"})),
    )
    .await;
    assert_eq!(patched.status, 200, "{patched:?}");
    let answers = [
        created.body.clone(),
        patched.body.clone(),
        call(&cp, "GET", "/api/keys", Some(owner.cookie()), None, None)
            .await
            .body,
        call(
            &cp,
            "GET",
            &format!("/api/keys/{dedicated}"),
            Some(owner.cookie()),
            None,
            None,
        )
        .await
        .body,
    ];
    for answer in &answers {
        let body = answer.to_string();
        for token in [
            "grp-eu-5d2",
            "grp-dedicated-9f3",
            "provider_group",
            "allowed_endpoints",
            "chat_completions",
            "key_hash",
        ] {
            assert!(!body.contains(token), "{token} in {body}");
        }
    }
    let view = &answers[3];
    let fields: BTreeSet<&str> = view
        .as_object()
        .expect("a key")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        BTreeSet::from([
            "created_at",
            "disabled",
            "disabled_by_platform",
            "expires_at",
            "id",
            "label",
            "model_ids",
            "owner",
            "prefix",
            "rate_limit",
            "version",
        ])
    );
}

#[tokio::test]
async fn operators_mint_and_manage_any_tenants_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await;

    let r = admin_mint(
        &cp,
        &admin,
        tenant,
        json!({"label": "ops-7c1", "member_id": member.id, "rate_limit": {"rpm": 50}}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let id = text(&r.body, "/key/id");
    let plaintext = text(&r.body, "/plaintext");
    let fields: BTreeSet<&str> = r.body["key"]
        .as_object()
        .expect("a key")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        BTreeSet::from([
            "allowed_models",
            "allowed_provider_groups",
            "created_at",
            "disabled",
            "disabled_by_tenant",
            "effective_disabled",
            "effective_provider_groups",
            "expires_at",
            "id",
            "label",
            "member_id",
            "model_ids",
            "prefix",
            "rate_limit",
            "tenant_id",
            "version",
        ])
    );
    assert_eq!(r.body["key"]["member_id"], json!(member.id));
    assert_eq!(r.body["key"]["tenant_id"], json!(tenant));
    assert_eq!(
        r.body["key"]["effective_provider_groups"],
        json!(["shared"])
    );
    let doc = key_doc(&cp, &id).await;
    assert_eq!(doc["key_hash"], json!(ApiKey::hash_bearer(&plaintext)));
    assert_eq!(doc["user_id"], json!(member.id));
    assert_eq!(doc["team_id"], json!(tenant));
    assert_eq!(
        doc["allowed_endpoints"],
        json!([
            "chat_completions",
            "messages",
            "count_tokens",
            "responses",
            "embeddings",
            "models"
        ])
    );
    let audit: (String, String) = sqlx::query_as(
        "SELECT action, actor_kind FROM audit_log WHERE target_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_one(cp.db().pool())
    .await
    .expect("an audit row");
    assert_eq!(audit, ("key.create".to_string(), "operator".to_string()));

    // A customer key cannot take model patterns or an empty group list.
    for (body, path) in [
        (
            json!({"label": "x-7c1", "allowed_models": ["openai/*"]}),
            "/allowed_models",
        ),
        (
            json!({"label": "x-7c1", "allowed_provider_groups": []}),
            "/allowed_provider_groups",
        ),
        (
            json!({"label": "x-7c1", "allowed_provider_groups": [" grp-eu-5d2"]}),
            "/allowed_provider_groups/0",
        ),
    ] {
        let r = admin_mint(&cp, &admin, tenant, body.clone()).await;
        assert_eq!(r.status, 422, "{body}: {r:?}");
        assert_eq!(r.body["details"][0]["path"], path, "{body}");
    }

    // Optimistic concurrency on the single-key routes.
    let r = call(
        &cp,
        "GET",
        &format!("/admin/keys/{id}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.header("etag"), Some("\"1\""));
    let r = call(
        &cp,
        "PATCH",
        &format!("/admin/keys/{id}"),
        Some(admin.cookie()),
        None,
        Some(json!({"label": "x-7c1"})),
    )
    .await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (428, json!("if_match_required"))
    );
    let r = on_admin_key(&cp, &admin, "PATCH", &id, Some(json!({"disabled": true}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &id).await["disabled"], true);
    let r = call(
        &cp,
        "PATCH",
        &format!("/admin/keys/{id}"),
        Some(admin.cookie()),
        Some("\"1\""),
        Some(json!({"label": "x-7c1"})),
    )
    .await;
    assert_eq!(
        (r.status, r.body["current_version"].clone()),
        (412, json!(2))
    );
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{tenant}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["key_count"], 1);
    let r = on_admin_key(&cp, &admin, "DELETE", &id, None).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert!(stored(&cp, "api_keys", &id).await.deleted);
    let r = call(
        &cp,
        "GET",
        &format!("/admin/keys/{id}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 404);
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{tenant}/keys"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.body["items"], json!([]));
    let r = call(
        &cp,
        "GET",
        &format!("/admin/tenants/{}/keys", Uuid::new_v4()),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 404);

    // The platform's own keys may name models by pattern and call every
    // endpoint, and need no group.
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("the internal tenant");
    let r = admin_mint(
        &cp,
        &admin,
        internal,
        json!({"label": "ops-all-7c1", "allowed_models": ["openai/*"], "allowed_provider_groups": []}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let doc = key_doc(&cp, &text(&r.body, "/key/id")).await;
    assert_eq!(doc["allowed_models"], json!(["openai/*"]));
    for absent in [
        "allowed_model_ids",
        "allowed_endpoints",
        "allowed_provider_groups",
        "user_id",
    ] {
        assert!(doc.get(absent).is_none(), "{absent} in {doc}");
    }
    assert_eq!(doc["disabled"], false);
}

/// A tenant's rollout covers its keys' documents as well as its own.
#[tokio::test]
async fn a_tenants_rollout_waits_for_its_keys() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (key, _) = mint_ok(&cp, &owner, json!({"label": "rollout-7c1"})).await;
    // Everything the tenant derives is projected, except the key.
    sqlx::query(
        "UPDATE gw_resources SET projected_version = version \
         WHERE managed_by IN ($1, $2)",
    )
    .bind(format!("tenant:{tenant}"))
    .bind(format!("member:{}", owner.id))
    .execute(cp.db().pool())
    .await
    .expect("mark projected");
    let path = format!("/admin/tenants/{tenant}");
    let r = call(&cp, "GET", &path, Some(admin.cookie()), None, None).await;
    assert_eq!(r.body["rollout"]["state"], "pending", "{r:?}");
    sqlx::query(
        "UPDATE gw_resources SET projected_version = version \
         WHERE kind = 'api_keys' AND id = $1::uuid",
    )
    .bind(&key)
    .execute(cp.db().pool())
    .await
    .expect("mark projected");
    let r = call(&cp, "GET", &path, Some(admin.cookie()), None, None).await;
    assert_eq!(r.body["rollout"]["state"], "no_gateways", "{r:?}");
}

#[tokio::test]
async fn a_member_mints_at_most_ten_keys_a_minute() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let other = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let mut first = None;
    for i in 0..10 {
        let (id, _) = mint_ok(&cp, &member, json!({"label": format!("burst-{i}-7c1")})).await;
        first.get_or_insert(id);
    }
    // Revoking one gives no slot back.
    let first = first.expect("a key");
    let r = on_key(&cp, &member, "DELETE", &first, None).await;
    assert_eq!(r.status, 204, "{r:?}");
    let before = (audit_rows(&cp).await, cp.count("tenant_keys").await);
    let r = mint(&cp, &member, json!({"label": "one-too-many-7c1"})).await;
    assert_eq!(r.status, 429, "{r:?}");
    assert_eq!(r.body["code"], "busy");
    assert_eq!(
        (audit_rows(&cp).await, cp.count("tenant_keys").await),
        before
    );
    // The limit is the member's own.
    mint_ok(&cp, &other, json!({"label": "someone-else-7c1"})).await;
    // A minute later the member mints again.
    sqlx::query(
        "UPDATE audit_log SET at = at - interval '61 seconds' \
         WHERE action = 'key.create' AND actor_id = $1",
    )
    .bind(member.id)
    .execute(cp.db().pool())
    .await
    .expect("age the mints");
    mint_ok(&cp, &member, json!({"label": "next-minute-7c1"})).await;
}

/// The release this control plane reports, as a stamped build would.
const CP_VERSION: &str = "1.9.0+sha-cp7c1";

/// A gateway credential issued by `admin`, whose instance `instance`
/// reports `version`.
async fn register_gateway(cp: &TestCp, admin: &OperatorSession) -> (String, DpClient) {
    let bundle = cp.issue_bundle(admin, "gw-keys-7c1").await;
    let client = cp.dp_client(&bundle);
    (bundle.dp_id, client)
}

async fn beat(gw: &(String, DpClient), instance: &str, version: &str) {
    let r =
        gw.1.heartbeat(json!({
            "dp_id": gw.0,
            "instance_id": instance,
            "hostname": format!("host-{instance}"),
            "uptime_seconds": 1,
            "version": version,
            "supported_guardrail_kinds": [],
            "applied_revision": 1,
            "exporter_health": [],
            "partially_compatible_resources": [],
        }))
        .await;
    assert_eq!(r.status, 200, "beat {instance} at {version}: {r:?}");
}

#[tokio::test]
async fn creating_a_tenant_key_while_an_older_gateway_is_registered_is_503_without_details() {
    let Some(cp) = TestCp::start_stamped(CP_VERSION).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = register_gateway(&cp, &admin).await;
    beat(&gw, "old-7c1", "1.8.2+sha-old7c1").await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let before = (audit_rows(&cp).await, cp.count("gw_resources").await);

    let r = mint(&cp, &owner, json!({"label": "too-early-7c1"})).await;
    assert_eq!(r.status, 503, "{r:?}");
    let fields: BTreeSet<&str> = r
        .body
        .as_object()
        .expect("an error")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fields, BTreeSet::from(["code", "error"]), "{r:?}");
    assert_eq!(r.body["code"], "unavailable");
    let body = r.body.to_string();
    for token in ["1.8.2", "1.9.0", "allowed_endpoints", "gateway_too_old"] {
        assert!(!body.contains(token), "{token} in {body}");
    }
    assert_eq!(
        (audit_rows(&cp).await, cp.count("gw_resources").await),
        before
    );
    assert_eq!(cp.count("tenant_keys").await, 0);
    // Operators see why.
    let r = admin_mint(&cp, &admin, tenant, json!({"label": "too-early-7c1"})).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (422, json!("gateway_too_old")),
        "{r:?}"
    );

    beat(&gw, "old-7c1", "1.9.0+sha-new7c1").await;
    mint_ok(&cp, &owner, json!({"label": "in-time-7c1"})).await;
}

/// While an older gateway is registered, whatever narrows what a key may do
/// still saves (disabling a key, a member or a tenant, withdrawing a model,
/// revoking), and whatever opens it again waits.
#[tokio::test]
async fn closing_a_key_works_while_an_older_gateway_is_registered() {
    let Some(cp) = TestCp::start_stamped(CP_VERSION).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = register_gateway(&cp, &admin).await;
    beat(&gw, "current-7c1", "1.9.0+sha-new7c1").await;
    let ids = models(&cp, &admin, &["narrow-a-7c1", "narrow-b-7c1"]).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    patch_tenant_ok(&cp, &admin, tenant, json!({"allowed_model_ids": ids})).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let (open, _) = mint_ok(&cp, &owner, json!({"label": "open-7c1"})).await;
    let (picked, _) = mint_ok(
        &cp,
        &owner,
        json!({"label": "picked-7c1", "model_ids": [ids[0]]}),
    )
    .await;
    let (members, _) = mint_ok(&cp, &member, json!({"label": "members-7c1"})).await;
    let (leaving, _) = mint_ok(&cp, &owner, json!({"label": "leaving-7c1"})).await;
    beat(&gw, "old-7c1", "1.8.2+sha-old7c1").await;

    let r = on_key(&cp, &owner, "PATCH", &open, Some(json!({"disabled": true}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &open).await["disabled"], true);
    let r = on_key(
        &cp,
        &owner,
        "PATCH",
        &open,
        Some(json!({"disabled": false})),
    )
    .await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (503, json!("unavailable")),
        "{r:?}"
    );
    assert_eq!(key_doc(&cp, &open).await["disabled"], true);

    // The last model withdrawn writes an empty list, which narrows.
    let r = call(
        &cp,
        "DELETE",
        &format!("/admin/resources/models/{}", ids[0]),
        Some(admin.cookie()),
        Some("\"1\""),
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &picked).await["allowed_model_ids"], json!([]));

    let r = on_key(&cp, &owner, "DELETE", &leaving, None).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert!(stored(&cp, "api_keys", &leaving).await.deleted);

    let r = admin_patch_member(&cp, &admin, member.id, json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &members).await["disabled"], true);
    let r = admin_patch_member(&cp, &admin, member.id, json!({"disabled": false})).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (422, json!("gateway_too_old")),
        "{r:?}"
    );
    assert_eq!(key_doc(&cp, &members).await["disabled"], true);

    let r = call(
        &cp,
        "DELETE",
        &format!("/api/members/{}", member.id),
        Some(owner.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(r.status, 204, "{r:?}");
    assert!(stored(&cp, "api_keys", &members).await.deleted);

    let r = act(&cp, &admin, tenant, "suspend").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &picked).await["disabled"], true);
    let r = act(&cp, &admin, tenant, "reactivate").await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (422, json!("gateway_too_old")),
        "{r:?}"
    );
    assert_eq!(key_doc(&cp, &picked).await["disabled"], true);
}

/// `PATCH /api/members/{id}` with `{role}` as `by`.
async fn set_role(cp: &TestCp, by: &MemberSession, member: Uuid, role: &str) -> Resp {
    call(
        cp,
        "PATCH",
        &format!("/api/members/{member}"),
        Some(by.cookie()),
        None,
        Some(json!({ "role": role })),
    )
    .await
}

/// A billing member sees bills only: their keys stop working when they are
/// moved to billing and work again when they are moved back, no key can be
/// bound to them, and while an older gateway is registered, moving them
/// back waits like any other reopening.
#[tokio::test]
async fn a_member_moved_to_billing_holds_no_working_key() {
    let Some(cp) = TestCp::start_stamped(CP_VERSION).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = register_gateway(&cp, &admin).await;
    beat(&gw, "current-7c1", "1.9.0+sha-new7c1").await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let (key, _) = mint_ok(&cp, &member, json!({"label": "theirs-7c1"})).await;
    let billing = cp.seed_member(tenant, MemberRole::Billing).await;

    // No key is bound to a billing member, on either API.
    let r = mint(
        &cp,
        &owner,
        json!({"label": "for-billing-7c1", "member_id": billing.id}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/member_id");
    let r = admin_mint(
        &cp,
        &admin,
        tenant,
        json!({"label": "for-billing-7c1", "member_id": billing.id}),
    )
    .await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/member_id");

    let r = set_role(&cp, &owner, member.id, "billing").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);
    let r = call(
        &cp,
        "GET",
        &format!("/admin/keys/{key}"),
        Some(admin.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(
        (
            r.body["disabled"].clone(),
            r.body["effective_disabled"].clone()
        ),
        (json!(false), json!(true))
    );
    // A re-projection of the tenant keeps it closed.
    patch_tenant_ok(&cp, &admin, tenant, json!({"limits": {"rpm": 600}})).await;
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);

    let r = set_role(&cp, &owner, member.id, "member").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &key).await["disabled"], false);

    // With an older gateway registered, moving to billing still closes the
    // key, and moving back waits.
    beat(&gw, "old-7c1", "1.8.2+sha-old7c1").await;
    let r = set_role(&cp, &owner, member.id, "billing").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);
    let r = set_role(&cp, &owner, member.id, "member").await;
    assert_eq!(r.status, 503, "{r:?}");
    let fields: BTreeSet<&str> = r
        .body
        .as_object()
        .expect("an error")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fields, BTreeSet::from(["code", "error"]), "{r:?}");
    assert_eq!(r.body["code"], "unavailable");
    let role: String = sqlx::query_scalar("SELECT role FROM members WHERE id = $1")
        .bind(member.id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the member");
    assert_eq!(role, "billing");
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);

    beat(&gw, "old-7c1", "1.9.0+sha-new7c1").await;
    let r = set_role(&cp, &owner, member.id, "member").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(key_doc(&cp, &key).await["disabled"], false);
}

/// The platform's switch and the tenant's are separate: what an operator
/// switched off the tenant cannot switch on, and an operator switching a
/// key on again does not undo the tenant's own switch.
#[tokio::test]
async fn an_operators_disable_sticks() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (key, _) = mint_ok(&cp, &owner, json!({"label": "watched-7c1"})).await;

    let r = on_admin_key(&cp, &admin, "PATCH", &key, Some(json!({"disabled": true}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        (
            r.body["disabled"].clone(),
            r.body["disabled_by_tenant"].clone(),
            r.body["effective_disabled"].clone()
        ),
        (json!(true), json!(false), json!(true))
    );
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE action = 'key.update' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("the update's audit row");
    assert_eq!(detail, json!({"fields": ["disabled"], "disabled": true}));

    let r = call(
        &cp,
        "GET",
        &format!("/api/keys/{key}"),
        Some(owner.cookie()),
        None,
        None,
    )
    .await;
    assert_eq!(
        (
            r.body["disabled"].clone(),
            r.body["disabled_by_platform"].clone()
        ),
        (json!(false), json!(true))
    );
    let version = key_version(&cp, &key).await;
    let r = on_key(&cp, &owner, "PATCH", &key, Some(json!({"disabled": false}))).await;
    assert_eq!(
        (r.status, r.body["code"].clone()),
        (403, json!("forbidden")),
        "{r:?}"
    );
    assert!(
        r.body["error"].as_str().is_some_and(|m| m.contains("平台")),
        "{r:?}"
    );
    assert_eq!(key_version(&cp, &key).await, version);
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);
    // The tenant still renames it, and sets its own switch.
    let r = on_key(
        &cp,
        &owner,
        "PATCH",
        &key,
        Some(json!({"label": "renamed-7c1"})),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = on_key(&cp, &owner, "PATCH", &key, Some(json!({"disabled": true}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    // A suspension and a reactivation leave it off.
    assert_eq!(act(&cp, &admin, tenant, "suspend").await.status, 200);
    assert_eq!(act(&cp, &admin, tenant, "reactivate").await.status, 200);
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);

    // The operator switches it on again; the tenant's switch is still off.
    let r = on_admin_key(&cp, &admin, "PATCH", &key, Some(json!({"disabled": false}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        (
            r.body["disabled"].clone(),
            r.body["disabled_by_tenant"].clone(),
            r.body["effective_disabled"].clone()
        ),
        (json!(false), json!(true), json!(true))
    );
    assert_eq!(key_doc(&cp, &key).await["disabled"], true);
    let r = on_key(&cp, &owner, "PATCH", &key, Some(json!({"disabled": false}))).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["disabled_by_platform"], false);
    assert_eq!(key_doc(&cp, &key).await["disabled"], false);
}

/// Sets `key_doc_version` as a release older than this one left it.
async fn set_key_doc_version(cp: &TestCp, version: u32) {
    sqlx::query(
        "INSERT INTO cp_settings (key, value) VALUES ('key_doc_version', $1) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind(json!(version))
    .execute(cp.db().pool())
    .await
    .expect("set the key document version");
}

async fn key_doc_version(cp: &TestCp) -> Value {
    sqlx::query_scalar("SELECT value FROM cp_settings WHERE key = 'key_doc_version'")
        .fetch_one(cp.db().pool())
        .await
        .expect("read the key document version")
}

/// Switches the stored documents of `keys` off behind the control plane's
/// back, so that writing them as they should be opens them again: a write
/// the gateway-version gate refuses while an older gateway is registered.
async fn close_behind_its_back(cp: &TestCp, keys: &[&str]) {
    for key in keys {
        sqlx::query(
            "UPDATE gw_resources SET doc = jsonb_set(doc, '{disabled}', 'true') \
             WHERE kind = 'api_keys' AND id = $1::uuid",
        )
        .bind(key)
        .execute(cp.db().pool())
        .await
        .expect("tamper with a key document");
    }
}

/// Every upgrade boots the control plane while older gateways are still
/// registered: a boot whose rewrites the gateway-version gate refuses
/// leaves them for a later boot instead of failing, and makes them once
/// the gateways are upgraded.
#[tokio::test]
async fn a_boot_leaves_a_rewrite_an_older_gateway_refuses_for_later() {
    let Some(cp) = TestCp::start_stamped(CP_VERSION).await else {
        return;
    };
    let admin = cp.admin_login_seeded(OperatorRole::Admin).await;
    let gw = register_gateway(&cp, &admin).await;
    beat(&gw, "gw-7c1", "1.9.0+sha-new7c1").await;
    let internal: Uuid = sqlx::query_scalar("SELECT id FROM tenants WHERE is_internal")
        .fetch_one(cp.db().pool())
        .await
        .expect("the internal tenant");
    let r = admin_mint(
        &cp,
        &admin,
        internal,
        json!({"label": "ops-7c1", "allowed_provider_groups": ["grp-ops-7c1"]}),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let ops = text(&r.body, "/key/id");
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    // Rewritten first, and without the gate's objection, but in the same
    // tenant as a rewrite it refuses: a tenant's keys move together.
    let (first, _) = mint_ok(&cp, &owner, json!({"label": "first-7c1"})).await;
    let (customer, _) = mint_ok(&cp, &owner, json!({"label": "customer-7c1"})).await;
    assert_eq!(key_doc_version(&cp).await, json!(KEY_DOC_VERSION));

    close_behind_its_back(&cp, &[&ops, &customer]).await;
    sqlx::query(
        "UPDATE gw_resources SET doc = jsonb_set(doc, '{display_name}', '\"stale-7c1\"') \
         WHERE kind = 'api_keys' AND id = $1::uuid",
    )
    .bind(&first)
    .execute(cp.db().pool())
    .await
    .expect("tamper with a key document");
    set_key_doc_version(&cp, 0).await;
    beat(&gw, "gw-7c1", "1.8.2+sha-old7c1").await;
    let upgraded = cp.restart_stamped(CP_VERSION).await;
    assert_eq!(key_doc(&upgraded, &ops).await["disabled"], true);
    assert_eq!(key_doc(&upgraded, &customer).await["disabled"], true);
    assert_eq!(
        key_doc(&upgraded, &first).await["display_name"],
        "stale-7c1"
    );
    assert_eq!(key_doc_version(&upgraded).await, json!(0));
    drop(upgraded);

    beat(&gw, "gw-7c1", "1.9.0+sha-new7c1").await;
    let again = cp.restart_stamped(CP_VERSION).await;
    assert_eq!(key_doc(&again, &ops).await["disabled"], false);
    assert_eq!(key_doc(&again, &customer).await["disabled"], false);
    assert_eq!(key_doc(&again, &first).await["display_name"], "first-7c1");
    assert_eq!(key_doc_version(&again).await, json!(KEY_DOC_VERSION));
}

/// A release that changes what key documents carry rewrites every live key
/// once, at its first boot, and not again; a revoked key stays deleted.
#[tokio::test]
async fn a_new_key_document_version_rewrites_every_key_once() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let a = cp.seed_tenant(TenantStatus::Active).await;
    let b = cp.seed_tenant(TenantStatus::Active).await;
    let owner_a = cp.member_login_seeded(a, MemberRole::Owner).await;
    let owner_b = cp.member_login_seeded(b, MemberRole::Owner).await;
    let (ka, _) = mint_ok(&cp, &owner_a, json!({"label": "a-7c1"})).await;
    let (kb, _) = mint_ok(&cp, &owner_b, json!({"label": "b-7c1"})).await;
    let (gone, _) = mint_ok(&cp, &owner_b, json!({"label": "gone-7c1"})).await;
    let r = on_key(&cp, &owner_b, "DELETE", &gone, None).await;
    assert_eq!(r.status, 204, "{r:?}");
    let stale = |cp: &TestCp, key: String| {
        let pool = cp.db().pool().clone();
        async move {
            sqlx::query(
                "UPDATE gw_resources SET doc = jsonb_set(doc, '{display_name}', '\"stale-7c1\"') \
                 WHERE kind = 'api_keys' AND id = $1::uuid",
            )
            .bind(key)
            .execute(&pool)
            .await
            .expect("tamper with a key document");
        }
    };
    stale(&cp, ka.clone()).await;
    stale(&cp, kb.clone()).await;
    let ctx = WriteCtx::of(cp.state());

    // The stored version is this release's: nothing is rewritten.
    let deferred = keys::bring_documents_up_to(cp.db(), &ctx, KEY_DOC_VERSION)
        .await
        .expect("bring the documents up to date");
    assert_eq!(deferred, 0);
    assert_eq!(key_doc(&cp, &ka).await["display_name"], "stale-7c1");

    // A newer shape rewrites every key's document, once.
    let deferred = keys::bring_documents_up_to(cp.db(), &ctx, KEY_DOC_VERSION + 1)
        .await
        .expect("bring the documents up to date");
    assert_eq!(deferred, 0);
    assert_eq!(key_doc(&cp, &ka).await["display_name"], "a-7c1");
    assert_eq!(key_doc(&cp, &kb).await["display_name"], "b-7c1");
    assert!(stored(&cp, "api_keys", &gone).await.deleted);
    assert_eq!(key_doc_version(&cp).await, json!(KEY_DOC_VERSION + 1));
    stale(&cp, ka.clone()).await;
    keys::bring_documents_up_to(cp.db(), &ctx, KEY_DOC_VERSION + 1)
        .await
        .expect("bring the documents up to date");
    assert_eq!(key_doc(&cp, &ka).await["display_name"], "stale-7c1");

    // So does a boot that finds an older version stored.
    set_key_doc_version(&cp, 0).await;
    let restarted = cp.restart().await;
    assert_eq!(key_doc(&restarted, &ka).await["display_name"], "a-7c1");
    assert_eq!(key_doc_version(&restarted).await, json!(KEY_DOC_VERSION));
}
