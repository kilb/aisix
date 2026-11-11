//! The tenant side's people: self-service signup, member sessions, invites,
//! roles and the last-owner rule, and what a member can and cannot see.
//!
//! Distinctive tokens (`-7c1`, `grp-eu-5d2`) keep "not present" assertions
//! from passing by accident.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use aisix_control_plane::auth::{MemberRole, OperatorRole};
use aisix_control_plane::http::{MinRole, ROUTE_ROLES};
use aisix_control_plane::members;
use aisix_control_plane::resources::validate::{validate_lenient, validate_strict};
use aisix_control_plane::resources::write::RESOURCES_LOCK_KEY;
use aisix_control_plane::resources::Kind;
use aisix_control_plane::tenants::TenantStatus;
use serde_json::{json, Value};
use sqlx::{Postgres, Transaction};
use support::members::{cookie_of, tenant_fixture, MemberSession};
use support::{OperatorSession, Resp, TestCp, TEST_PASSWORD};
use uuid::Uuid;

fn fresh_email(tag: &str) -> String {
    format!("{tag}-{}@tenant.test", Uuid::new_v4().simple())
}

async fn signup(cp: &TestCp, org: &str, email: &str, display_name: Option<&str>) -> Resp {
    let mut body = json!({"org_name": org, "email": email, "password": TEST_PASSWORD});
    if let Some(name) = display_name {
        body["display_name"] = json!(name);
    }
    cp.send("POST", "/api/signup", None, Some(body)).await
}

async fn accept(cp: &TestCp, token: &str, password: &str, display_name: Option<&str>) -> Resp {
    let mut body = json!({"token": token, "password": password});
    if let Some(name) = display_name {
        body["display_name"] = json!(name);
    }
    cp.send("POST", "/api/invites/accept", None, Some(body))
        .await
}

/// `POST /api/members/invites` as `by`.
async fn invite(cp: &TestCp, by: &MemberSession, email: &str, role: &str) -> Resp {
    cp.send(
        "POST",
        "/api/members/invites",
        Some(by.cookie()),
        Some(json!({"email": email, "role": role})),
    )
    .await
}

/// Invites `email` as `role` and accepts the invitation with `password`;
/// the new member's session.
async fn join(cp: &TestCp, by: &MemberSession, email: &str, role: &str, password: &str) -> Resp {
    let r = invite(cp, by, email, role).await;
    assert_eq!(r.status, 201, "invite {email}: {r:?}");
    let token = r.body["invite_token"]
        .as_str()
        .expect("a token")
        .to_string();
    let a = accept(cp, &token, password, None).await;
    assert_eq!(a.status, 201, "accept for {email}: {a:?}");
    a
}

async fn patch_member(cp: &TestCp, by: &MemberSession, id: &str, role: &str) -> Resp {
    cp.send(
        "PATCH",
        &format!("/api/members/{id}"),
        Some(by.cookie()),
        Some(json!({ "role": role })),
    )
    .await
}

async fn remove_member(cp: &TestCp, by: &MemberSession, id: &str) -> Resp {
    cp.send(
        "DELETE",
        &format!("/api/members/{id}"),
        Some(by.cookie()),
        None,
    )
    .await
}

async fn admin_patch_member(cp: &TestCp, by: &OperatorSession, id: &str, body: Value) -> Resp {
    cp.send(
        "PATCH",
        &format!("/admin/members/{id}"),
        Some(by.cookie()),
        Some(body),
    )
    .await
}

/// An answer's status, `Content-Type` and body bytes, for comparing two
/// answers byte for byte.
async fn raw(
    cp: &TestCp,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (u16, String, Vec<u8>) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    // Labelled JSON with or without a body, as the Dashboard sends it.
    let mut request = client
        .request(method, cp.url(path))
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        request = request.header(reqwest::header::COOKIE, cookie);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("send the request");
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.bytes().await.expect("read the body").to_vec();
    (status, content_type, bytes)
}

/// `PATCH`/`PUT`/`DELETE` with `If-Match`, for the operator's tenant routes.
async fn send_if_match(
    cp: &TestCp,
    cookie: &str,
    method: &str,
    path: &str,
    version: i64,
    body: Value,
) -> u16 {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("an HTTP client");
    let method = reqwest::Method::from_bytes(method.as_bytes()).expect("an HTTP method");
    client
        .request(method, cp.url(path))
        .header(reqwest::header::COOKIE, cookie)
        .header(reqwest::header::IF_MATCH, format!("\"{version}\""))
        .json(&body)
        .send()
        .await
        .expect("send the request")
        .status()
        .as_u16()
}

fn text(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{pointer} in {v}"))
        .to_string()
}

/// `(role, disabled, removed)` of member `id`.
async fn member_row(cp: &TestCp, id: &str) -> (String, bool, bool) {
    sqlx::query_as("SELECT role, disabled, removed_at IS NOT NULL FROM members WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the member")
}

async fn member_sessions(cp: &TestCp, id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM sessions \
         WHERE principal_kind = 'member' AND principal_id = $1::uuid AND expires_at > now()",
    )
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .expect("count sessions")
}

/// Enabled, live owners of `tenant`.
async fn owners(cp: &TestCp, tenant: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM members \
         WHERE tenant_id = $1 AND role = 'owner' AND NOT disabled AND removed_at IS NULL",
    )
    .bind(tenant)
    .fetch_one(cp.db().pool())
    .await
    .expect("count owners")
}

async fn audit_rows(cp: &TestCp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log")
        .fetch_one(cp.db().pool())
        .await
        .expect("count audit rows")
}

/// `(action, actor_kind, actor_id, target_kind, target_id)` of every audit
/// row about `tenant`, in order.
async fn tenant_audit(
    cp: &TestCp,
    tenant: &str,
) -> Vec<(String, String, Option<Uuid>, String, Option<String>)> {
    sqlx::query_as(
        "SELECT action, actor_kind, actor_id, target_kind, target_id FROM audit_log \
         WHERE tenant_id = $1::uuid ORDER BY id",
    )
    .bind(tenant)
    .fetch_all(cp.db().pool())
    .await
    .expect("read the audit log")
}

/// The text of every row of `tables`, to show a value is stored nowhere.
async fn dump(cp: &TestCp, tables: &[&str]) -> String {
    let mut all = String::new();
    for table in tables {
        let rows: Vec<String> = sqlx::query_scalar(&format!("SELECT t::text FROM {table} t"))
            .fetch_all(cp.db().pool())
            .await
            .unwrap_or_else(|e| panic!("dump {table}: {e}"));
        all.push_str(&rows.join("\n"));
    }
    all
}

/// The stored document and owner of `kind/id`, and whether it is deleted.
async fn resource(cp: &TestCp, kind: &str, id: &str) -> Option<(Value, Option<String>, bool)> {
    sqlx::query_as(
        "SELECT doc, managed_by, deleted FROM gw_resources WHERE kind = $1 AND id = $2::uuid",
    )
    .bind(kind)
    .bind(id)
    .fetch_optional(cp.db().pool())
    .await
    .expect("read a resource")
}

/// The `members` items `cookie`'s member lists.
async fn listed_members(cp: &TestCp, cookie: &str) -> Vec<Value> {
    let r = cp.get_with_cookie("/api/members", cookie).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

fn ids(items: &[Value]) -> Vec<String> {
    items.iter().map(|m| text(m, "/id")).collect()
}

async fn act_on_tenant(cp: &TestCp, op: &OperatorSession, tenant: &str, action: &str) {
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/{action}"),
            Some(op.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 200, "{action} {tenant}: {r:?}");
}

#[tokio::test]
async fn signing_up_makes_a_pending_tenant_with_its_owner_logged_in() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let email = fresh_email("Founder");
    let r = signup(
        &cp,
        "  Acme 7c1  ",
        &format!("  {email} "),
        Some(" Ada 7c1 "),
    )
    .await;
    assert_eq!(r.status, 201, "{r:?}");
    let normalized = email.to_ascii_lowercase();
    let member_id = text(&r.body, "/member/id");
    let tenant_id = text(&r.body, "/tenant/id");
    assert_eq!(
        r.body,
        json!({
            "authed": true,
            "api_contract": 1,
            "member": {"id": member_id, "email": normalized, "display_name": "Ada 7c1", "role": "owner"},
            "tenant": {"id": tenant_id, "name": "Acme 7c1", "status": "pending"},
        })
    );

    // The owner is logged in, with the portal's cookie.
    let set_cookie = r.set_cookie.clone().expect("a session cookie");
    let token = set_cookie
        .strip_prefix("aisix_cp_portal=")
        .and_then(|rest| rest.split(';').next())
        .expect("the portal cookie")
        .to_string();
    assert_eq!(
        set_cookie,
        format!(
            "aisix_cp_portal={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=43200"
        )
    );
    let cookie = cookie_of(&r).expect("a cookie");
    let session = cp.get_with_cookie("/api/session", &cookie).await;
    assert_eq!(session.status, 200, "{session:?}");
    assert_eq!(session.body, r.body);
    // Stored as the token's SHA-256 only.
    let (stored, expected): (Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT token_sha256, sha256($2::bytea) FROM sessions \
         WHERE principal_kind = 'member' AND principal_id = $1::uuid",
    )
    .bind(&member_id)
    .bind(token.as_bytes())
    .fetch_one(cp.db().pool())
    .await
    .expect("read the session");
    assert_eq!(stored, expected);

    // A pending tenant, with the defaults copied, and its owner as a member.
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let t = cp
        .get_with_cookie(&format!("/admin/tenants/{tenant_id}"), viewer.cookie())
        .await;
    assert_eq!(t.status, 200, "{t:?}");
    assert_eq!(t.body["status"], "pending");
    assert_eq!(t.body["is_internal"], false);
    assert_eq!(t.body["approved_at"], Value::Null);
    assert_eq!(t.body["allowed_provider_groups"], json!(["shared"]));
    assert_eq!(
        t.body["limits"],
        json!({"concurrency": 20, "rpm": 1200, "key_concurrency_max": 10})
    );
    assert_eq!(t.body["member_count"], 1);
    let listed = cp
        .get_with_cookie(
            &format!("/admin/tenants/{tenant_id}/members"),
            viewer.cookie(),
        )
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    let items = listed.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    let created_at = items[0]["created_at"].clone();
    assert!(created_at.is_string(), "{created_at}");
    assert_eq!(
        items[0],
        json!({
            "id": member_id, "email": normalized, "display_name": "Ada 7c1",
            "role": "owner", "disabled": false, "created_at": created_at,
        })
    );
    let pending = cp
        .get_with_cookie("/admin/tenants?status=pending", viewer.cookie())
        .await;
    assert!(
        pending.body["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|t| t["id"] == tenant_id.as_str()),
        "{pending:?}"
    );

    // A pending tenant gives the gateways nothing: no team and no user
    // document until an operator approves it. The signup is audited as the
    // owner.
    assert_eq!(resource(&cp, "teams", &tenant_id).await, None);
    assert_eq!(resource(&cp, "users", &member_id).await, None);
    assert_eq!(
        tenant_audit(&cp, &tenant_id).await,
        vec![(
            "tenant.signup".to_string(),
            "member".to_string(),
            Some(Uuid::parse_str(&member_id).expect("a uuid")),
            "tenant".to_string(),
            Some(tenant_id.clone()),
        )]
    );
    // The password is stored only as its argon2 hash.
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM members WHERE id = $1::uuid")
        .bind(&member_id)
        .fetch_one(cp.db().pool())
        .await
        .expect("read the hash");
    assert!(hash.starts_with("$argon2"), "{hash}");
    let stored = dump(&cp, &["members", "sessions", "audit_log", "gw_resources"]).await;
    assert!(!stored.contains(TEST_PASSWORD));
    assert!(!stored.contains(&token));
}

#[tokio::test]
async fn signing_up_with_a_taken_email_is_409_not_500() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let email = fresh_email("taken");
    let first = signup(&cp, "first-7c1", &email, None).await;
    assert_eq!(first.status, 201, "{first:?}");
    let count = |sql: &'static str| {
        let pool = cp.db().pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .expect("count")
        }
    };
    let tenants = count("SELECT count(*) FROM tenants").await;
    let teams = count("SELECT count(*) FROM gw_resources WHERE kind = 'teams'").await;
    let audit = audit_rows(&cp).await;

    let again = signup(&cp, "second-7c1", &email.to_ascii_uppercase(), None).await;
    assert_eq!(again.status, 409, "{again:?}");
    assert_eq!(again.body["code"], "email_taken");
    assert!(again.set_cookie.is_none(), "{again:?}");
    // Nothing of the second tenant is left behind.
    assert_eq!(count("SELECT count(*) FROM tenants").await, tenants);
    assert_eq!(
        count("SELECT count(*) FROM gw_resources WHERE kind = 'teams'").await,
        teams
    );
    assert_eq!(audit_rows(&cp).await, audit);
    assert!(!dump(&cp, &["tenants", "gw_resources"])
        .await
        .contains("second-7c1"));

    // An email an invitation brought in is just as taken.
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = text(&first.body, "/tenant/id");
    let invited = fresh_email("invited");
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/invites"),
            Some(op.cookie()),
            Some(json!({"email": invited, "role": "member"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let a = accept(
        &cp,
        &text(&r.body, "/invite_token"),
        "invited-password-1",
        None,
    )
    .await;
    assert_eq!(a.status, 201, "{a:?}");
    let r = signup(&cp, "third-7c1", &invited, None).await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "email_taken");
}

#[tokio::test]
async fn signups_are_limited_site_wide_and_a_refused_one_gives_its_slot_back() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let taken = fresh_email("slots");
    let r = signup(&cp, "slots-7c1", &taken, None).await;
    assert_eq!(r.status, 201, "{r:?}");
    // 59 of the minute's 60 signups are used up.
    for i in 0..58 {
        assert!(
            cp.state().portal.signups.try_take().is_some(),
            "slot {i} was refused"
        );
    }
    // Signups refused for a taken email hand their slot back, so retrying
    // one cannot shut signup for everyone.
    for _ in 0..3 {
        let r = signup(&cp, "slots-7c1", &taken, None).await;
        assert_eq!(r.status, 409, "{r:?}");
    }
    // A request refused before it reaches the database takes no slot.
    let r = cp
        .send(
            "POST",
            "/api/signup",
            None,
            Some(json!({"org_name": "x", "email": fresh_email("short"), "password": "short"})),
        )
        .await;
    assert_eq!(r.status, 422, "{r:?}");
    // The sixtieth signup of the minute goes through, the sixty-first waits.
    let r = signup(&cp, "slots-7c1", &fresh_email("sixtieth"), None).await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = signup(&cp, "slots-7c1", &fresh_email("sixty-first"), None).await;
    assert_eq!(r.status, 429, "{r:?}");
    assert_eq!(r.body["code"], "busy");
    assert!(r.set_cookie.is_none());
}

/// `(version, seq, deleted)` of the stored document `kind/id`.
async fn stamp(cp: &TestCp, kind: &str, id: &str) -> (i64, i64, bool) {
    sqlx::query_as(
        "SELECT version, seq, deleted FROM gw_resources WHERE kind = $1 AND id = $2::uuid",
    )
    .bind(kind)
    .bind(id)
    .fetch_one(cp.db().pool())
    .await
    .unwrap_or_else(|e| panic!("read {kind}/{id}: {e}"))
}

/// Mints a key as `cookie`'s member; its id.
async fn mint(cp: &TestCp, cookie: &str, label: &str) -> String {
    let r = cp
        .send(
            "POST",
            "/api/keys",
            Some(cookie),
            Some(json!({"label": label})),
        )
        .await;
    assert_eq!(r.status, 201, "mint {label}: {r:?}");
    text(&r.body, "/key/id")
}

/// A tenant waiting for approval gives the gateways nothing. Its approval
/// writes the team and the user of every live member, in that transaction
/// and ahead of the limits policy and the keys; the keys minted afterwards
/// work; and a suspension and a reactivation leave the team and the users
/// as they are.
#[tokio::test]
async fn approving_a_tenant_writes_its_team_and_every_members_user_and_its_keys_then_work() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = signup(&cp, "approve-7c1", &fresh_email("founder"), Some("Ada 7c1")).await;
    assert_eq!(r.status, 201, "{r:?}");
    let tenant = text(&r.body, "/tenant/id");
    let owner_id = text(&r.body, "/member/id");
    let owner_cookie = cookie_of(&r).expect("a cookie");
    // An operator's invitation, accepted while the tenant waits.
    let invited = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/invites"),
            Some(op.cookie()),
            Some(json!({"email": fresh_email("early"), "role": "member"})),
        )
        .await;
    assert_eq!(invited.status, 201, "{invited:?}");
    let a = accept(
        &cp,
        &text(&invited.body, "/invite_token"),
        "early-password-7c1",
        None,
    )
    .await;
    assert_eq!(a.status, 201, "{a:?}");
    let member_id = text(&a.body, "/member/id");
    let member_cookie = cookie_of(&a).expect("a cookie");
    let owners = [
        format!("tenant:{tenant}"),
        format!("member:{owner_id}"),
        format!("member:{member_id}"),
    ];
    let owned: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gw_resources WHERE managed_by = ANY($1)")
            .bind(&owners[..])
            .fetch_one(cp.db().pool())
            .await
            .expect("count the documents");
    assert_eq!(owned, 0, "a pending tenant wrote documents");

    act_on_tenant(&cp, &op, &tenant, "approve").await;
    let short = member_id.replace('-', "");
    for (kind, id, doc, owner) in [
        ("teams", &tenant, json!({"name": "approve-7c1"}), &owners[0]),
        ("users", &owner_id, json!({"name": "Ada 7c1"}), &owners[1]),
        (
            "users",
            &member_id,
            json!({ "name": format!("member-{}", &short[..8]) }),
            &owners[2],
        ),
    ] {
        let (stored, managed_by, deleted) = resource(&cp, kind, id)
            .await
            .unwrap_or_else(|| panic!("{kind}/{id} after the approval"));
        assert_eq!(
            (stored, managed_by.as_ref(), deleted),
            (doc, Some(owner), false)
        );
    }
    let policy: Uuid =
        sqlx::query_scalar("SELECT limits_policy_id FROM tenants WHERE id = $1::uuid")
            .bind(&tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("a limits policy");
    let policy_seq = stamp(&cp, "rate_limit_policies", &policy.to_string())
        .await
        .1;
    for (kind, id) in [
        ("teams", &tenant),
        ("users", &owner_id),
        ("users", &member_id),
    ] {
        assert!(
            stamp(&cp, kind, id).await.1 < policy_seq,
            "{kind}/{id} was written after the limits policy"
        );
    }

    // Keys minted now name the team and their member, and work.
    let owner_key = mint(&cp, &owner_cookie, "owner-key-7c1").await;
    let member_key = mint(&cp, &member_cookie, "member-key-7c1").await;
    for (key, member) in [(&owner_key, &owner_id), (&member_key, &member_id)] {
        let (doc, _, deleted) = resource(&cp, "api_keys", key)
            .await
            .expect("a key document");
        assert!(!deleted);
        assert_eq!(doc["team_id"], tenant.as_str(), "{doc}");
        assert_eq!(doc["user_id"], member.as_str(), "{doc}");
        assert_eq!(doc["disabled"], false, "{doc}");
    }

    // Suspending closes the keys and keeps the team and the users; so does
    // reactivating, which opens the keys again.
    let kept = [
        ("teams", &tenant),
        ("users", &owner_id),
        ("users", &member_id),
    ];
    let mut before = Vec::new();
    for (kind, id) in kept {
        before.push(stamp(&cp, kind, id).await);
    }
    for (action, disabled) in [("suspend", true), ("reactivate", false)] {
        act_on_tenant(&cp, &op, &tenant, action).await;
        for ((kind, id), was) in kept.iter().zip(&before) {
            let now = stamp(&cp, kind, id).await;
            assert_eq!((now.0, now.2), (was.0, false), "{action}: {kind}/{id}");
        }
        for key in [&owner_key, &member_key] {
            let (doc, _, _) = resource(&cp, "api_keys", key)
                .await
                .expect("a key document");
            assert_eq!(doc["disabled"], disabled, "{action}: {doc}");
        }
    }
}

/// Signup is anonymous, so the tenants waiting for approval are capped at
/// [`members::MAX_PENDING_TENANTS`]; past the cap a signup is refused with
/// the very answer a full signup window gives, telling the caller nothing
/// new. A tenant that stops waiting frees its place.
#[tokio::test]
async fn pending_tenants_are_capped_and_past_the_cap_signup_answers_as_a_full_window_does() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    // Every place but one taken.
    sqlx::query(
        "INSERT INTO tenants (id, name, status) \
         SELECT gen_random_uuid(), 'waiting-7c1-' || n, 'pending' FROM generate_series(1, $1) n",
    )
    .bind(members::MAX_PENDING_TENANTS - 1)
    .execute(cp.db().pool())
    .await
    .expect("seed pending tenants");
    let r = signup(&cp, "last-place-7c1", &fresh_email("last"), None).await;
    assert_eq!(r.status, 201, "{r:?}");
    let last = text(&r.body, "/tenant/id");

    let counts = || async {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT (SELECT count(*) FROM tenants), (SELECT count(*) FROM audit_log)",
        )
        .fetch_one(cp.db().pool())
        .await
        .expect("count")
    };
    let before = counts().await;
    let over = signup(&cp, "over-cap-7c1", &fresh_email("over"), None).await;
    assert_eq!(over.status, 429, "{over:?}");
    assert!(over.set_cookie.is_none(), "{over:?}");
    assert_eq!(counts().await, before, "a refused signup changed something");

    // Approved, the last tenant no longer waits, and its place is free.
    act_on_tenant(&cp, &op, &last, "approve").await;
    let r = signup(&cp, "freed-place-7c1", &fresh_email("freed"), None).await;
    assert_eq!(r.status, 201, "{r:?}");

    // A full window answers exactly as the cap did.
    while cp.state().portal.signups.try_take().is_some() {}
    let full = signup(&cp, "full-window-7c1", &fresh_email("full"), None).await;
    assert_eq!(
        (over.status, &over.body),
        (full.status, &full.body),
        "the cap's refusal differs from the window's"
    );
}

#[tokio::test]
async fn a_removed_member_can_be_invited_again() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let email = fresh_email("returning");

    let first = join(&cp, &owner, &email, "member", "first-password-7c1").await;
    let first_id = text(&first.body, "/member/id");
    let r = remove_member(&cp, &owner, &first_id).await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = cp.member_login(&email, "first-password-7c1").await;
    assert_eq!(r.status, 401, "{r:?}");

    let second = join(&cp, &owner, &email, "billing", "second-password-7c1").await;
    let second_id = text(&second.body, "/member/id");
    assert_ne!(second_id, first_id);
    assert_eq!(second.body["member"]["role"], "billing");
    assert_eq!(second.body["member"]["email"], email.as_str());

    // Only the live member logs in, with its own password.
    let r = cp.member_login(&email, "second-password-7c1").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["member"]["id"], second_id.as_str());
    let r = cp.member_login(&email, "first-password-7c1").await;
    assert_eq!(r.status, 401, "{r:?}");
    let listed = ids(&listed_members(&cp, owner.cookie()).await);
    assert!(listed.contains(&second_id), "{listed:?}");
    assert!(!listed.contains(&first_id), "{listed:?}");
    // The removed member's row stays, marked removed.
    assert_eq!(
        member_row(&cp, &first_id).await,
        ("member".to_string(), false, true)
    );
}

/// The tenant routes a member of a tenant that is not active may still
/// call: to see the tenant's state, keep their own account, and close what
/// should not outlive the suspension (a leaked key, a member who left).
/// Every other one answers `403 tenant_not_active`.
const OPEN_WHILE_NOT_ACTIVE: [(&str, &str); 6] = [
    ("GET", "/api/org"),
    ("POST", "/api/password"),
    ("GET", "/api/keys"),
    ("DELETE", "/api/keys/{id}"),
    ("GET", "/api/members"),
    ("DELETE", "/api/members/{id}"),
];

/// Every member route outside [`OPEN_WHILE_NOT_ACTIVE`] answers `cookie`
/// with `403 tenant_not_active` and changes nothing.
async fn every_other_route_is_closed(cp: &TestCp, cookie: &str) {
    let mut closed = 0;
    for (method, path, min) in ROUTE_ROLES {
        if !matches!(min, MinRole::Member(_)) || OPEN_WHILE_NOT_ACTIVE.contains(&(*method, *path)) {
            continue;
        }
        let before = audit_rows(cp).await;
        let (uri, body) = tenant_fixture(method, path);
        let r = cp.send(method, &uri, Some(cookie), body).await;
        assert_eq!(r.status, 403, "{method} {path}: {r:?}");
        assert_eq!(r.body["code"], "tenant_not_active", "{method} {path}");
        assert_eq!(
            audit_rows(cp).await,
            before,
            "{method} {path} changed something"
        );
        closed += 1;
    }
    assert!(closed >= 7, "only {closed} routes were checked");
}

#[tokio::test]
async fn a_pending_tenant_reaches_only_the_routes_open_while_not_active() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let email = fresh_email("pending");
    let r = signup(&cp, "pending-7c1", &email, None).await;
    assert_eq!(r.status, 201, "{r:?}");
    let cookie = cookie_of(&r).expect("a cookie");
    let tenant = text(&r.body, "/tenant/id");

    let s = cp.get_with_cookie("/api/session", &cookie).await;
    assert_eq!(s.body["authed"], true, "{s:?}");
    assert_eq!(s.body["tenant"]["status"], "pending");
    let org = cp.get_with_cookie("/api/org", &cookie).await;
    assert_eq!(org.status, 200, "{org:?}");
    // The tenant's own key caps, and nothing about groups or upstreams.
    assert_eq!(
        org.body,
        json!({"id": tenant, "name": "pending-7c1", "status": "pending",
               "limits": {"key_concurrency_max": 10}})
    );
    // A password change is open too: refused here for its length, not for
    // the tenant's state.
    let (uri, body) = tenant_fixture("POST", "/api/password");
    let r = cp.send("POST", &uri, Some(&cookie), body).await;
    assert_eq!(r.status, 422, "{r:?}");
    every_other_route_is_closed(&cp, &cookie).await;

    // Approved, the tenant's routes open.
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    act_on_tenant(&cp, &op, &tenant, "approve").await;
    assert_eq!(listed_members(&cp, &cookie).await.len(), 1);

    // Suspended, they close again; its members can still sign in and see
    // why.
    act_on_tenant(&cp, &op, &tenant, "suspend").await;
    every_other_route_is_closed(&cp, &cookie).await;
    let again = cp.member_login(&email, TEST_PASSWORD).await;
    assert_eq!(again.status, 200, "{again:?}");
    assert_eq!(again.body["tenant"]["status"], "suspended");
    let org = cp
        .get_with_cookie("/api/org", &cookie_of(&again).expect("a cookie"))
        .await;
    assert_eq!(org.body["status"], "suspended", "{org:?}");
}

/// The ids and versions a revoke or a removal needs come from the lists, so
/// a suspended tenant's lists stay open, read-only and as each role sees
/// them: the owner lists the keys and the members and revokes a key at the
/// version listed; a member lists only their own keys and no members; a
/// billing member lists no keys. Everything else that reads or writes stays
/// closed.
#[tokio::test]
async fn a_suspended_tenants_owner_lists_its_keys_and_members_and_revokes_from_the_list() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let member = join(
        &cp,
        &owner,
        &fresh_email("lister"),
        "member",
        "lister-password-7c1",
    )
    .await;
    let member_cookie = cookie_of(&member).expect("a cookie");
    let billing = join(
        &cp,
        &owner,
        &fresh_email("payer"),
        "billing",
        "payer-password-7c1",
    )
    .await;
    let billing_cookie = cookie_of(&billing).expect("a cookie");
    let owners_key = mint(&cp, owner.cookie(), "owners-7c1").await;
    let members_key = mint(&cp, &member_cookie, "members-7c1").await;
    act_on_tenant(&cp, &op, &tenant.to_string(), "suspend").await;

    let keys = cp.get_with_cookie("/api/keys", owner.cookie()).await;
    assert_eq!(keys.status, 200, "{keys:?}");
    let items = keys.body["items"].as_array().expect("items").clone();
    let mut listed = ids(&items);
    listed.sort();
    let mut minted = vec![owners_key.clone(), members_key.clone()];
    minted.sort();
    assert_eq!(listed, minted);
    let members = listed_members(&cp, owner.cookie()).await;
    assert_eq!(members.len(), 3, "{members:?}");

    // A member sees their own key and no member list; billing, no keys.
    let theirs = cp.get_with_cookie("/api/keys", &member_cookie).await;
    assert_eq!(theirs.status, 200, "{theirs:?}");
    assert_eq!(
        ids(theirs.body["items"].as_array().expect("items")),
        std::slice::from_ref(&members_key)
    );
    for (cookie, path) in [
        (&member_cookie, "/api/members"),
        (&billing_cookie, "/api/keys"),
    ] {
        let r = cp.get_with_cookie(path, cookie).await;
        assert_eq!(r.status, 403, "{path}: {r:?}");
        assert_eq!(r.body["code"], "forbidden", "{path}: {r:?}");
    }

    // The owner revokes the member's key at the version the list showed.
    let listed_key = items
        .iter()
        .find(|k| k["id"] == members_key.as_str())
        .expect("the member's key is listed");
    let version = listed_key["version"].as_i64().expect("a version");
    let status = send_if_match(
        &cp,
        owner.cookie(),
        "DELETE",
        &format!("/api/keys/{members_key}"),
        version,
        json!({}),
    )
    .await;
    assert_eq!(status, 204);
    let keys = cp.get_with_cookie("/api/keys", owner.cookie()).await;
    assert_eq!(
        ids(keys.body["items"].as_array().expect("items")),
        std::slice::from_ref(&owners_key)
    );

    // Nothing else opens: a single key, a mint, an invitation.
    for (method, path, body) in [
        ("GET", format!("/api/keys/{owners_key}"), None),
        (
            "POST",
            "/api/keys".to_string(),
            Some(json!({"label": "late-7c1"})),
        ),
        (
            "POST",
            "/api/members/invites".to_string(),
            Some(json!({"email": fresh_email("late"), "role": "member"})),
        ),
    ] {
        let r = cp.send(method, &path, Some(owner.cookie()), body).await;
        assert_eq!(r.status, 403, "{method} {path}: {r:?}");
        assert_eq!(r.body["code"], "tenant_not_active", "{method} {path}");
    }
}

/// The key and member lists are reads: they answer while a change holds
/// the tenant's row, rather than queueing behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_key_and_member_lists_do_not_wait_for_the_tenants_lock() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
        .bind(tenant)
        .execute(&mut *holder)
        .await
        .expect("hold the tenant");
    for path in ["/api/keys", "/api/members"] {
        let r = tokio::time::timeout(
            Duration::from_secs(3),
            cp.get_with_cookie(path, owner.cookie()),
        )
        .await
        .unwrap_or_else(|_| panic!("{path} waited for the tenant's lock"));
        assert_eq!(r.status, 200, "{path}: {r:?}");
    }
    holder.rollback().await.expect("release the tenant");
}

/// A suspended or pending tenant can still close what must not outlive the
/// suspension: it revokes a leaked key and removes a member who left. A key
/// revoked meanwhile stays revoked through the reactivation, which opens
/// only the keys still live; and a member removed before the approval gets
/// no user document from it.
#[tokio::test]
async fn a_tenant_that_is_not_active_can_still_revoke_its_keys_and_remove_a_member() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let joined = join(
        &cp,
        &owner,
        &fresh_email("leaver"),
        "member",
        "leaver-password-7c1",
    )
    .await;
    let leaver = text(&joined.body, "/member/id");
    let leaked = mint(&cp, owner.cookie(), "leaked-7c1").await;
    let kept = mint(&cp, owner.cookie(), "kept-7c1").await;
    let leavers = mint(&cp, &cookie_of(&joined).expect("a cookie"), "leaver-7c1").await;
    let tenant = tenant.to_string();
    act_on_tenant(&cp, &op, &tenant, "suspend").await;

    let status = send_if_match(
        &cp,
        owner.cookie(),
        "DELETE",
        &format!("/api/keys/{leaked}"),
        1,
        json!({}),
    )
    .await;
    assert_eq!(status, 204);
    let r = remove_member(&cp, &owner, &leaver).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert!(member_row(&cp, &leaver).await.2, "not removed");
    for (kind, id) in [
        ("api_keys", &leaked),
        ("api_keys", &leavers),
        ("users", &leaver),
    ] {
        let (_, _, deleted) = resource(&cp, kind, id).await.expect("a document");
        assert!(deleted, "{kind}/{id} outlived its revocation");
    }

    act_on_tenant(&cp, &op, &tenant, "reactivate").await;
    let (doc, _, _) = resource(&cp, "api_keys", &kept)
        .await
        .expect("a key document");
    assert_eq!(doc["disabled"], false, "{doc}");
    for key in [&leaked, &leavers] {
        let (_, _, deleted) = resource(&cp, "api_keys", key).await.expect("a document");
        assert!(deleted, "the reactivation brought back {key}");
    }
    let listed = cp.get_with_cookie("/api/keys", owner.cookie()).await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(ids(listed.body["items"].as_array().expect("items")), [kept]);

    // While the tenant still waits for approval, too.
    let r = signup(&cp, "waiting-7c1", &fresh_email("waiting"), None).await;
    assert_eq!(r.status, 201, "{r:?}");
    let waiting = text(&r.body, "/tenant/id");
    let founder = text(&r.body, "/member/id");
    let founder_cookie = cookie_of(&r).expect("a cookie");
    let invited = cp
        .send(
            "POST",
            &format!("/admin/tenants/{waiting}/invites"),
            Some(op.cookie()),
            Some(json!({"email": fresh_email("early"), "role": "member"})),
        )
        .await;
    assert_eq!(invited.status, 201, "{invited:?}");
    let a = accept(
        &cp,
        &text(&invited.body, "/invite_token"),
        "early-password-7c1",
        None,
    )
    .await;
    assert_eq!(a.status, 201, "{a:?}");
    let early = text(&a.body, "/member/id");
    let r = cp
        .send(
            "DELETE",
            &format!("/api/members/{early}"),
            Some(&founder_cookie),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    act_on_tenant(&cp, &op, &waiting, "approve").await;
    assert_eq!(resource(&cp, "users", &early).await, None);
    assert!(resource(&cp, "users", &founder).await.is_some());
}

#[tokio::test]
async fn a_member_session_never_opens_the_admin_api() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let token = member
        .cookie()
        .strip_prefix("aisix_cp_portal=")
        .expect("the portal cookie")
        .to_string();
    // The cookie as the browser sends it, the token under the operators'
    // cookie name, and both at once.
    let disguised = format!("aisix_cp_admin={token}");
    let both = format!("{}; {disguised}", member.cookie());
    for cookie in [member.cookie(), disguised.as_str(), both.as_str()] {
        let s = cp.get_with_cookie("/admin/session", cookie).await;
        assert_eq!(s.status, 200, "{s:?}");
        assert_eq!(
            s.body,
            json!({"authed": false, "api_contract": 1}),
            "{cookie}"
        );
        for path in [
            "/admin/tenants",
            "/admin/audit",
            "/admin/operators",
            &format!("/admin/tenants/{tenant}/members"),
        ] {
            let r = cp.get_with_cookie(path, cookie).await;
            assert_eq!(r.status, 401, "{path} with {cookie}: {r:?}");
            assert_eq!(r.body["code"], "unauthenticated");
        }
        let r = cp
            .send(
                "PATCH",
                &format!("/admin/members/{}", member.id),
                Some(cookie),
                Some(json!({"disabled": true})),
            )
            .await;
        assert_eq!(r.status, 401, "{r:?}");
    }
    assert!(!member_row(&cp, &member.id.to_string()).await.1);
    // The same cookie does open the tenant API, which reads only that one.
    assert_eq!(listed_members(&cp, member.cookie()).await.len(), 1);
    let r = cp.get_with_cookie("/api/org", &disguised).await;
    assert_eq!(r.status, 401, "{r:?}");
}

#[tokio::test]
async fn an_operator_session_never_opens_the_tenant_api() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Admin).await;
    let token = op
        .cookie()
        .strip_prefix("aisix_cp_admin=")
        .expect("the admin cookie")
        .to_string();
    let disguised = format!("aisix_cp_portal={token}");
    let both = format!("{}; {disguised}", op.cookie());
    for cookie in [op.cookie(), disguised.as_str(), both.as_str()] {
        let s = cp.get_with_cookie("/api/session", cookie).await;
        assert_eq!(s.status, 200, "{s:?}");
        assert_eq!(
            s.body,
            json!({"authed": false, "api_contract": 1}),
            "{cookie}"
        );
        for path in [
            "/api/org",
            "/api/models",
            "/api/members",
            "/api/members/invites",
        ] {
            let r = cp.get_with_cookie(path, cookie).await;
            assert_eq!(r.status, 401, "{path} with {cookie}: {r:?}");
            assert_eq!(r.body["code"], "unauthenticated");
        }
        let (uri, body) = tenant_fixture("POST", "/api/password");
        let r = cp.send("POST", &uri, Some(cookie), body).await;
        assert_eq!(r.status, 401, "{r:?}");
        // Logging out of the tenant API leaves the operator's session alone.
        let r = cp.send("POST", "/api/logout", Some(cookie), None).await;
        assert_eq!(r.status, 204, "{r:?}");
    }
    // So does a member logging in with it in the portal cookie, which ends
    // the session that cookie carried only when it is a member's.
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let m = cp.seed_member(tenant, MemberRole::Member).await;
    let r = cp
        .send(
            "POST",
            "/api/login",
            Some(&disguised),
            Some(json!({"email": m.email, "password": TEST_PASSWORD})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    // The operator is still logged in.
    let s = cp.get_with_cookie("/admin/session", op.cookie()).await;
    assert_eq!(s.body["authed"], true, "{s:?}");
}

#[tokio::test]
async fn an_invite_is_single_use_and_expires() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;

    let email = fresh_email("Invitee");
    let r = invite(&cp, &owner, &email, "member").await;
    assert_eq!(r.status, 201, "{r:?}");
    let token = text(&r.body, "/invite_token");
    let invite_id = text(&r.body, "/id");
    assert!(
        token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
        "{token}"
    );
    // No portal URL is configured, so there is no link.
    let mut keys: Vec<&str> = r
        .body
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["expires_at", "id", "invite_token"]);
    // Stored as its SHA-256 only, for seven days.
    let (hashed, seven_days): (bool, bool) = sqlx::query_as(
        "SELECT token_sha256 = sha256($2::bytea), expires_at - created_at = interval '7 days' \
         FROM member_invites WHERE id = $1::uuid",
    )
    .bind(&invite_id)
    .bind(token.as_bytes())
    .fetch_one(cp.db().pool())
    .await
    .expect("read the invite");
    assert!(hashed && seven_days);
    assert!(!dump(&cp, &["member_invites", "audit_log"])
        .await
        .contains(&token));

    let joined = accept(&cp, &token, "invitee-password-1", Some("Bea 7c1")).await;
    assert_eq!(joined.status, 201, "{joined:?}");
    assert_eq!(joined.body["member"]["email"], email.to_ascii_lowercase());
    assert_eq!(joined.body["member"]["role"], "member");
    assert_eq!(joined.body["member"]["display_name"], "Bea 7c1");
    assert_eq!(joined.body["tenant"]["id"], tenant.to_string());
    let s = cp
        .get_with_cookie("/api/session", &cookie_of(&joined).expect("a cookie"))
        .await;
    assert_eq!(s.body, joined.body);

    // Used once, it is gone; so is an expired one, a revoked one, one never
    // issued and one of another shape, all with the same answer.
    let used = raw(
        &cp,
        "POST",
        "/api/invites/accept",
        None,
        Some(json!({"token": token, "password": "another-password-1"})),
    )
    .await;
    assert_eq!(used.0, 404, "{:?}", String::from_utf8_lossy(&used.2));
    let expired = invite(&cp, &owner, &fresh_email("expired"), "member").await;
    sqlx::query(
        "UPDATE member_invites SET expires_at = now() - interval '1 second' WHERE id = $1::uuid",
    )
    .bind(text(&expired.body, "/id"))
    .execute(cp.db().pool())
    .await
    .expect("expire the invite");
    let revoked = invite(&cp, &owner, &fresh_email("revoked"), "member").await;
    let r = cp
        .send(
            "DELETE",
            &format!("/api/members/invites/{}", text(&revoked.body, "/id")),
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = cp
        .send(
            "DELETE",
            &format!("/api/members/invites/{}", text(&revoked.body, "/id")),
            Some(owner.cookie()),
            None,
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    for refused in [
        text(&expired.body, "/invite_token"),
        text(&revoked.body, "/invite_token"),
        "ab".repeat(32),
        "not-a-token".to_string(),
    ] {
        let r = raw(
            &cp,
            "POST",
            "/api/invites/accept",
            None,
            Some(json!({"token": refused, "password": "another-password-1"})),
        )
        .await;
        assert_eq!(r, used, "{refused}");
    }

    // Two acceptances of one invitation at once: one joins.
    for round in 0..3 {
        let r = invite(&cp, &owner, &fresh_email("twice"), "member").await;
        let token = text(&r.body, "/invite_token");
        let (a, b) = tokio::join!(
            accept(&cp, &token, "twice-password-1", None),
            accept(&cp, &token, "twice-password-2", None),
        );
        let mut statuses = [a.status, b.status];
        statuses.sort_unstable();
        assert_eq!(statuses, [201, 404], "round {round}: {a:?} {b:?}");
    }

    // Only invitations still open are listed.
    let open = invite(&cp, &owner, &fresh_email("open"), "billing").await;
    let listed = cp
        .get_with_cookie("/api/members/invites", owner.cookie())
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    let items = listed.body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        items[0],
        json!({
            "id": open.body["id"], "email": items[0]["email"], "role": "billing",
            "expires_at": open.body["expires_at"], "created_at": items[0]["created_at"],
            "invited_by": {"id": owner.id, "display_name": null},
        })
    );
    assert!(text(&items[0], "/email").starts_with("open-"));
    let actions: Vec<String> = tenant_audit(&cp, &tenant.to_string())
        .await
        .into_iter()
        .map(|row| row.0)
        .collect();
    assert_eq!(
        actions[..2],
        ["member.invite", "member.join"],
        "{actions:?}"
    );
    assert!(
        actions.contains(&"member.invite_revoke".to_string()),
        "{actions:?}"
    );
}

#[tokio::test]
async fn an_admin_cannot_invite_or_change_an_owner() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let admin = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    let member = cp.seed_member(tenant, MemberRole::Member).await;
    let owners_invite = invite(&cp, &owner, &fresh_email("next-owner"), "owner").await;
    assert_eq!(owners_invite.status, 201, "{owners_invite:?}");
    let before = audit_rows(&cp).await;

    let refusals = [
        invite(&cp, &admin, &fresh_email("owner"), "owner").await,
        patch_member(&cp, &admin, &owner.id.to_string(), "admin").await,
        patch_member(&cp, &admin, &member.id.to_string(), "owner").await,
        patch_member(&cp, &admin, &admin.id.to_string(), "owner").await,
        remove_member(&cp, &admin, &owner.id.to_string()).await,
        cp.send(
            "DELETE",
            &format!("/api/members/invites/{}", text(&owners_invite.body, "/id")),
            Some(admin.cookie()),
            None,
        )
        .await,
    ];
    for r in &refusals {
        assert_eq!(r.status, 403, "{r:?}");
        assert_eq!(r.body["code"], "forbidden");
    }
    assert_eq!(audit_rows(&cp).await, before);
    assert_eq!(
        member_row(&cp, &owner.id.to_string()).await,
        ("owner".to_string(), false, false)
    );
    assert_eq!(member_row(&cp, &member.id.to_string()).await.0, "member");
    assert_eq!(member_row(&cp, &admin.id.to_string()).await.0, "admin");

    // What an admin may do.
    let r = invite(&cp, &admin, &fresh_email("helper"), "admin").await;
    assert_eq!(r.status, 201, "{r:?}");
    let r = patch_member(&cp, &admin, &member.id.to_string(), "billing").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["role"], "billing");
    // And an owner may make another owner.
    let r = patch_member(&cp, &owner, &admin.id.to_string(), "owner").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(owners(&cp, tenant).await, 2);
}

#[tokio::test]
async fn the_last_owner_cannot_be_demoted_or_removed() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let id = owner.id.to_string();

    let check_last = |r: Resp| {
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(r.body["code"], "last_owner");
    };
    check_last(patch_member(&cp, &owner, &id, "admin").await);
    check_last(remove_member(&cp, &owner, &id).await);
    check_last(admin_patch_member(&cp, &op, &id, json!({"disabled": true})).await);
    assert_eq!(
        member_row(&cp, &id).await,
        ("owner".to_string(), false, false)
    );

    // A disabled owner is not one that remains.
    let second = cp.seed_member(tenant, MemberRole::Owner).await;
    let r = admin_patch_member(&cp, &op, &second.id.to_string(), json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    check_last(patch_member(&cp, &owner, &id, "member").await);
    check_last(remove_member(&cp, &owner, &id).await);

    // Once the second owner is enabled again, the first can step down, and
    // then the second is the last.
    let r = admin_patch_member(&cp, &op, &second.id.to_string(), json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = patch_member(&cp, &owner, &id, "admin").await;
    assert_eq!(r.status, 200, "{r:?}");
    let second = cp.member_login_as(&second).await;
    check_last(patch_member(&cp, &second, &second.id.to_string(), "admin").await);
    check_last(remove_member(&cp, &second, &second.id.to_string()).await);
    assert_eq!(owners(&cp, tenant).await, 1);
}

/// Lock waits here last as long as the test needs, however slow argon2 is.
fn patient(cfg: &mut aisix_control_plane::config::CpConfig) {
    cfg.db_lock_timeout_ms = 120_000;
}

async fn lock_waiters(cp: &TestCp) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(cp.db().pool())
    .await
    .expect("read pg_stat_activity")
}

/// Returns once `n` backends wait for a lock. It polls a condition the test
/// itself sets up, so it does not depend on timing.
async fn until_lock_waiters(cp: &TestCp, n: i64) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while lock_waiters(cp).await < n {
        assert!(Instant::now() < deadline, "{n} lock waiters never appeared");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A transaction that holds back every insert into `audit_log` until it
/// ends: a change blocks on its audit row after doing everything else.
async fn hold_audit_inserts(cp: &TestCp) -> Transaction<'static, Postgres> {
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("LOCK TABLE audit_log IN SHARE MODE")
        .execute(&mut *holder)
        .await
        .expect("hold audit_log");
    holder
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_owners_demoting_each_other_at_once_leave_one_owner() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    // Each round, two owners demote each other, then (on a fresh pair) each
    // one demotes themselves. The first change holds the tenant and waits
    // on its audit row; the second has to wait for it before it counts the
    // owners, so it sees the first change.
    for round in 0..6 {
        let tenant = cp.seed_tenant(TenantStatus::Active).await;
        let a = cp.member_login_seeded(tenant, MemberRole::Owner).await;
        let b = cp.member_login_seeded(tenant, MemberRole::Owner).await;
        let crosswise = round % 2 == 0;
        let (a_target, b_target) = if crosswise {
            (b.id, a.id)
        } else {
            (a.id, b.id)
        };
        let holder = hold_audit_inserts(&cp).await;
        let first = tokio::spawn({
            let (cp, a) = (cp.clone(), a.clone());
            async move { patch_member(&cp, &a, &a_target.to_string(), "member").await }
        });
        until_lock_waiters(&cp, 1).await;
        let second = tokio::spawn({
            let (cp, b) = (cp.clone(), b.clone());
            async move { patch_member(&cp, &b, &b_target.to_string(), "member").await }
        });
        until_lock_waiters(&cp, 2).await;
        holder.rollback().await.expect("release audit_log");
        let first = first.await.expect("join");
        let second = second.await.expect("join");
        assert_eq!(first.status, 200, "round {round}: {first:?}");
        // Crosswise, the second caller is no longer an owner; on their own,
        // they are the last one.
        let refusal = if crosswise { 403 } else { 409 };
        assert_eq!(second.status, refusal, "round {round}: {second:?}");
        assert_eq!(owners(&cp, tenant).await, 1, "round {round}");
    }
}

#[tokio::test]
async fn another_tenants_member_id_is_not_found() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant_a = cp.seed_tenant(TenantStatus::Active).await;
    let tenant_b = cp.seed_tenant(TenantStatus::Active).await;
    let owner_a = cp.member_login_seeded(tenant_a, MemberRole::Owner).await;
    let owner_b = cp.member_login_seeded(tenant_b, MemberRole::Owner).await;
    let member_b = cp.member_login_seeded(tenant_b, MemberRole::Member).await;
    let invite_b = invite(&cp, &owner_b, &fresh_email("b-invitee"), "member").await;
    assert_eq!(invite_b.status, 201, "{invite_b:?}");
    let invite_b = text(&invite_b.body, "/id");
    let before = audit_rows(&cp).await;

    let random = Uuid::new_v4().to_string();
    let theirs = member_b.id.to_string();
    for (method, path, body) in [
        ("PATCH", "/api/members/{}", Some(json!({"role": "admin"}))),
        ("PATCH", "/api/members/{}", Some(json!({"role": "owner"}))),
        ("DELETE", "/api/members/{}", None),
    ] {
        let unknown = raw(
            &cp,
            method,
            &path.replace("{}", &random),
            Some(owner_a.cookie()),
            body.clone(),
        )
        .await;
        assert_eq!(unknown.0, 404, "{method} {path}");
        let foreign = raw(
            &cp,
            method,
            &path.replace("{}", &theirs),
            Some(owner_a.cookie()),
            body,
        )
        .await;
        assert_eq!(foreign, unknown, "{method} {path}");
    }
    let unknown = raw(
        &cp,
        "DELETE",
        &format!("/api/members/invites/{random}"),
        Some(owner_a.cookie()),
        None,
    )
    .await;
    assert_eq!(unknown.0, 404);
    let foreign = raw(
        &cp,
        "DELETE",
        &format!("/api/members/invites/{invite_b}"),
        Some(owner_a.cookie()),
        None,
    )
    .await;
    assert_eq!(foreign, unknown);
    // A malformed id answers the same.
    let malformed = raw(
        &cp,
        "DELETE",
        "/api/members/not-an-id",
        Some(owner_a.cookie()),
        None,
    )
    .await;
    assert_eq!(
        malformed,
        raw(
            &cp,
            "DELETE",
            &format!("/api/members/{random}"),
            Some(owner_a.cookie()),
            None
        )
        .await
    );

    // B's member and invitation are untouched, and nothing was recorded.
    assert_eq!(audit_rows(&cp).await, before);
    assert_eq!(
        member_row(&cp, &theirs).await,
        ("member".to_string(), false, false)
    );
    assert_eq!(member_sessions(&cp, &theirs).await, 1);
    let open = cp
        .get_with_cookie("/api/members/invites", owner_b.cookie())
        .await;
    assert_eq!(
        ids(open.body["items"].as_array().expect("items")),
        [invite_b]
    );
    // A's own lists name none of B's.
    let listed = ids(&listed_members(&cp, owner_a.cookie()).await);
    assert_eq!(listed, [owner_a.id.to_string()]);
}

#[tokio::test]
async fn the_users_document_names_no_email() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let named_email = fresh_email("named-7c1");
    let named = signup(&cp, "users-7c1", &named_email, Some("Cyd 7c1")).await;
    let unnamed_email = fresh_email("unnamed-7c1");
    let unnamed = signup(&cp, "users-7c1", &unnamed_email, None).await;
    // A signup's user is written when its tenant is approved.
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    for r in [&named, &unnamed] {
        act_on_tenant(&cp, &op, &text(&r.body, "/tenant/id"), "approve").await;
    }
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let joined_email = fresh_email("joined-7c1");
    let r = invite(&cp, &owner, &joined_email, "member").await;
    let joined = accept(
        &cp,
        &text(&r.body, "/invite_token"),
        "joined-password-1",
        Some("Dee 7c1"),
    )
    .await;

    for (r, email, name) in [
        (&named, &named_email, None),
        (&unnamed, &unnamed_email, Some(())),
        (&joined, &joined_email, None),
    ] {
        assert_eq!(r.status, 201, "{r:?}");
        let id = text(&r.body, "/member/id");
        let (doc, owner, deleted) = resource(&cp, "users", &id)
            .await
            .unwrap_or_else(|| panic!("no users document for {id}"));
        let expected = match name {
            Some(()) => format!("member-{}", &id[..8]),
            None => text(&r.body, "/member/display_name"),
        };
        assert_eq!(doc, json!({ "name": expected }));
        assert_eq!(owner, Some(format!("member:{id}")));
        assert!(!deleted);
        let local = email.split('@').next().expect("a local part");
        assert!(!doc.to_string().contains(local), "{doc}");
    }
}

#[test]
fn every_member_derived_document_loads_on_the_lenient_schema() {
    let id = Uuid::new_v4();
    for doc in [
        members::user_doc(id, None),
        members::user_doc(id, Some("Ada 7c1")),
    ] {
        validate_strict(Kind::Users, &doc).unwrap_or_else(|e| panic!("strict: {e:?} {doc}"));
        validate_lenient(Kind::Users, &doc).unwrap_or_else(|e| panic!("lenient: {e} {doc}"));
    }
    assert_eq!(
        members::user_doc(id, None),
        json!({"name": format!("member-{}", &id.to_string()[..8])})
    );
}

#[tokio::test]
async fn tenant_models_list_names_and_ids_only() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let create = |kind: &'static str, doc: Value| {
        let (cp, cookie) = (&cp, op.cookie().to_string());
        async move {
            let r = cp
                .send(
                    "POST",
                    &format!("/admin/resources/{kind}"),
                    Some(&cookie),
                    Some(json!({ "doc": doc })),
                )
                .await;
            assert_eq!(r.status, 201, "create {kind}: {r:?}");
            text(&r.body, "/id")
        }
    };
    // A provider key names only live groups.
    support::seed_group(&cp, &op, "grp-eu-5d2").await;
    let account = create(
        "provider_keys",
        json!({"display_name": "acct-e2e-1", "provider": "openai",
               "api_key": "sk-models-7c1", "groups": ["grp-eu-5d2"]}),
    )
    .await;
    let model = |name: &str| {
        json!({"display_name": name, "provider": "openai", "model_name": "gpt-4o-mini",
               "provider_key_id": account})
    };
    let granted = create("models", model("chat-e2e")).await;
    let _not_granted = create("models", model("other-e2e-7c1")).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let t = cp
        .get_with_cookie(&format!("/admin/tenants/{tenant}"), op.cookie())
        .await;
    let version = t.body["version"].as_i64().expect("a version");
    let status = send_if_match(
        &cp,
        op.cookie(),
        "PATCH",
        &format!("/admin/tenants/{tenant}"),
        version,
        json!({"allowed_model_ids": [granted], "allowed_provider_groups": ["grp-eu-5d2"]}),
    )
    .await;
    assert_eq!(status, 200);

    let billing = cp.member_login_seeded(tenant, MemberRole::Billing).await;
    let r = cp.get_with_cookie("/api/models", billing.cookie()).await;
    assert_eq!(r.status, 200, "{r:?}");
    let items = r.body["items"].as_array().expect("items");
    assert_eq!(items, &[json!({"id": granted, "name": "chat-e2e"})]);
    for item in items {
        let mut keys: Vec<&String> = item.as_object().expect("an object").keys().collect();
        keys.sort();
        assert_eq!(keys, ["id", "name"]);
    }
    // Nothing a member reads names an upstream.
    let org = cp.get_with_cookie("/api/org", billing.cookie()).await;
    let session = cp.get_with_cookie("/api/session", billing.cookie()).await;
    for body in [&r.body, &org.body, &session.body] {
        let text = body.to_string();
        for leak in [
            "gpt-4o-mini",
            "openai",
            "acct-e2e-1",
            "grp-eu-5d2",
            "other-e2e-7c1",
            &account,
        ] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
    }
}

#[tokio::test]
async fn removing_a_member_ends_their_sessions() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let leaving = cp.seed_member(tenant, MemberRole::Admin).await;
    let first = cp.member_login_as(&leaving).await;
    let second = cp.member_login_as(&leaving).await;
    let id = leaving.id.to_string();
    assert_eq!(member_sessions(&cp, &id).await, 2);

    let r = remove_member(&cp, &owner, &id).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(r.body, Value::Null);
    for session in [&first, &second] {
        let r = cp.get_with_cookie("/api/org", session.cookie()).await;
        assert_eq!(r.status, 401, "{r:?}");
        let s = cp.get_with_cookie("/api/session", session.cookie()).await;
        assert_eq!(s.body["authed"], false, "{s:?}");
    }
    assert_eq!(member_sessions(&cp, &id).await, 0);
    let r = cp.member_login(&leaving.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 401, "{r:?}");
    // Their users document is deleted; the owner is unaffected.
    let (_, _, deleted) = resource(&cp, "users", &id).await.expect("the users row");
    assert!(deleted);
    assert_eq!(
        ids(&listed_members(&cp, owner.cookie()).await),
        [owner.id.to_string()]
    );
    let audit = tenant_audit(&cp, &tenant.to_string()).await;
    assert_eq!(
        audit.last().cloned(),
        Some((
            "member.remove".to_string(),
            "member".to_string(),
            Some(owner.id),
            "member".to_string(),
            Some(id.clone()),
        ))
    );
    // Gone means gone.
    let r = remove_member(&cp, &owner, &id).await;
    assert_eq!(r.status, 404, "{r:?}");
}

#[tokio::test]
async fn a_disabled_or_removed_members_session_is_refused_even_when_it_was_not_ended() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    for column in ["disabled = true", "removed_at = now()"] {
        let m = cp.member_login_seeded(tenant, MemberRole::Owner).await;
        assert_eq!(cp.get_with_cookie("/api/org", m.cookie()).await.status, 200);
        // Changed behind the API's back, so the session row stays.
        sqlx::query(&format!("UPDATE members SET {column} WHERE id = $1"))
            .bind(m.id)
            .execute(cp.db().pool())
            .await
            .expect("change the member");
        assert_eq!(member_sessions(&cp, &m.id.to_string()).await, 1);
        let r = cp.get_with_cookie("/api/org", m.cookie()).await;
        assert_eq!(r.status, 401, "{column}: {r:?}");
        let s = cp.get_with_cookie("/api/session", m.cookie()).await;
        assert_eq!(s.body["authed"], false, "{column}: {s:?}");
    }
}

#[tokio::test]
async fn wrong_password_unknown_email_and_disabled_or_removed_members_answer_alike() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let live = cp.seed_member(tenant, MemberRole::Member).await;
    let disabled = cp.seed_member(tenant, MemberRole::Member).await;
    let removed = cp.seed_member(tenant, MemberRole::Member).await;
    sqlx::query("UPDATE members SET disabled = true WHERE id = $1")
        .bind(disabled.id)
        .execute(cp.db().pool())
        .await
        .expect("disable");
    sqlx::query("UPDATE members SET removed_at = now() WHERE id = $1")
        .bind(removed.id)
        .execute(cp.db().pool())
        .await
        .expect("remove");

    let mut answers = Vec::new();
    for (email, password) in [
        (live.email.as_str(), "wrong-password-7c1"),
        ("nobody-7f3a@tenant.test", TEST_PASSWORD),
        (disabled.email.as_str(), TEST_PASSWORD),
        (removed.email.as_str(), TEST_PASSWORD),
    ] {
        let verified = cp.verification_count();
        let r = raw(
            &cp,
            "POST",
            "/api/login",
            None,
            Some(json!({"email": email, "password": password})),
        )
        .await;
        assert_eq!(r.0, 401, "{email}");
        // The same work each time: one verification.
        assert_eq!(cp.verification_count(), verified + 1, "{email}");
        answers.push(r);
    }
    assert!(answers.windows(2).all(|w| w[0] == w[1]), "{answers:?}");
    let body: Value = serde_json::from_slice(&answers[0].2).expect("JSON");
    assert_eq!(body["code"], "invalid_credentials");
    // The positive control: the live member's right password works.
    let r = cp.member_login(&live.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 200, "{r:?}");
}

#[tokio::test]
async fn logging_in_replaces_the_session_the_request_carried() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let m = cp.seed_member(tenant, MemberRole::Member).await;
    let old = cp.member_login_as(&m).await;
    let r = cp
        .send(
            "POST",
            "/api/login",
            Some(old.cookie()),
            Some(json!({"email": m.email, "password": TEST_PASSWORD})),
        )
        .await;
    assert_eq!(r.status, 200, "{r:?}");
    let new = cookie_of(&r).expect("a new cookie");
    assert_ne!(new, old.cookie());
    let s = cp.get_with_cookie("/api/session", old.cookie()).await;
    assert_eq!(
        s.body["authed"], false,
        "the carried session survived: {s:?}"
    );
    let s = cp.get_with_cookie("/api/session", &new).await;
    assert_eq!(s.body["authed"], true, "{s:?}");
    assert_eq!(member_sessions(&cp, &m.id.to_string()).await, 1);

    // Logging out ends it and clears the cookie.
    let r = cp.send("POST", "/api/logout", Some(&new), None).await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(
        r.set_cookie.as_deref(),
        Some("aisix_cp_portal=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
    );
    let s = cp.get_with_cookie("/api/session", &new).await;
    assert_eq!(s.body["authed"], false, "{s:?}");
    assert_eq!(member_sessions(&cp, &m.id.to_string()).await, 0);
}

#[tokio::test]
async fn five_failed_logins_cool_a_member_email_down() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let m = cp.seed_member(tenant, MemberRole::Member).await;
    let other = cp.seed_member(tenant, MemberRole::Member).await;
    for _ in 0..5 {
        let r = cp.member_login(&m.email, "wrong-password-7c1").await;
        assert_eq!(r.status, 401, "{r:?}");
    }
    let verified = cp.verification_count();
    let r = cp
        .member_login(
            &format!(" {} ", m.email.to_ascii_uppercase()),
            TEST_PASSWORD,
        )
        .await;
    assert_eq!(r.status, 429, "{r:?}");
    assert_eq!(r.body["code"], "busy");
    assert_eq!(
        r.headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("60")
    );
    assert_eq!(
        cp.verification_count(),
        verified,
        "a cooling email was verified"
    );
    let r = cp.member_login(&other.email, TEST_PASSWORD).await;
    assert_eq!(r.status, 200, "{r:?}");
}

/// Runs `change` against `victim`'s account while a login with the old
/// password is in flight, holding both back until each is inside its
/// transaction, and returns the change's and the login's answers.
async fn race_login_against<F, Fut>(cp: &Arc<TestCp>, email: &str, change: F) -> (Resp, Resp)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Resp> + Send + 'static,
{
    let holder = hold_audit_inserts(cp).await;
    let change = tokio::spawn(change());
    until_lock_waiters(cp, 1).await;
    let login = tokio::spawn({
        let (cp, email) = (cp.clone(), email.to_string());
        async move { cp.member_login(&email, TEST_PASSWORD).await }
    });
    until_lock_waiters(cp, 2).await;
    holder.rollback().await.expect("release audit_log");
    (change.await.expect("join"), login.await.expect("join"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acceptance_racing_a_signup_for_its_email_is_409_not_500() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let email = fresh_email("contested");
    let r = invite(&cp, &owner, &email, "member").await;
    assert_eq!(r.status, 201, "{r:?}");
    let token = text(&r.body, "/invite_token");
    let invite_id = text(&r.body, "/id");

    // A signup for the same email, as far as the database sees one: it
    // holds the resources lock (it has written its own new tenant's team)
    // when it adds its member to that tenant.
    let signup_tenant = cp.seed_tenant(TenantStatus::Pending).await;
    let mut signup = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *signup)
        .await
        .expect("take the resources lock");
    let accepting = tokio::spawn({
        let (cp, token) = (cp.clone(), token.clone());
        async move { accept(&cp, &token, "contested-password-1", None).await }
    });
    until_lock_waiters(&cp, 1).await;
    sqlx::query(
        "INSERT INTO members (id, tenant_id, email, password_hash, role) \
         VALUES ($1, $2, $3, 'not-a-hash', 'member')",
    )
    .bind(Uuid::new_v4())
    .bind(signup_tenant)
    .bind(&email)
    .execute(&mut *signup)
    .await
    .expect("the signup adds its member");
    signup.commit().await.expect("commit the signup");

    // The acceptance finds the email taken, rather than deadlocking.
    let accepted = accepting.await.expect("join");
    assert_eq!(accepted.status, 409, "{accepted:?}");
    assert_eq!(accepted.body["code"], "email_taken");
    // And the invitation is still unused.
    let used: bool = sqlx::query_scalar(
        "SELECT accepted_at IS NOT NULL FROM member_invites WHERE id = $1::uuid",
    )
    .bind(&invite_id)
    .fetch_one(cp.db().pool())
    .await
    .expect("read the invite");
    assert!(!used);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acceptance_racing_a_removal_in_its_tenant_waits_for_it() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let leaving = cp.seed_member(tenant, MemberRole::Member).await;
    let r = invite(&cp, &owner, &fresh_email("arriving"), "member").await;
    let token = text(&r.body, "/invite_token");

    // The removal holds the tenant, then waits for the leaving member's
    // row; the acceptance comes in meanwhile.
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM members WHERE id = $1 FOR UPDATE")
        .bind(leaving.id)
        .execute(&mut *holder)
        .await
        .expect("hold the leaving member");
    let removal = tokio::spawn({
        let (cp, owner, id) = (cp.clone(), owner.clone(), leaving.id.to_string());
        async move { remove_member(&cp, &owner, &id).await }
    });
    until_lock_waiters(&cp, 1).await;
    let acceptance = tokio::spawn({
        let (cp, token) = (cp.clone(), token.clone());
        async move { accept(&cp, &token, "arriving-password-1", None).await }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release the member");
    let removal = removal.await.expect("join");
    let acceptance = acceptance.await.expect("join");
    assert_eq!(
        (removal.status, acceptance.status),
        (204, 201),
        "{removal:?} {acceptance:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acceptance_racing_a_revocation_of_its_invite_is_never_a_500() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let r = invite(&cp, &owner, &fresh_email("revoked-or-not"), "member").await;
    let (token, id) = (text(&r.body, "/invite_token"), text(&r.body, "/id"));

    // The acceptance gets as far as the gateway documents and waits there;
    // the revocation of the same invitation comes in meanwhile.
    let mut holder = cp.db().pool().begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .expect("take the resources lock");
    let acceptance = tokio::spawn({
        let (cp, token) = (cp.clone(), token.clone());
        async move { accept(&cp, &token, "revoked-password-1", None).await }
    });
    until_lock_waiters(&cp, 1).await;
    let revocation = tokio::spawn({
        let (cp, owner) = (cp.clone(), owner.clone());
        async move {
            cp.send(
                "DELETE",
                &format!("/api/members/invites/{id}"),
                Some(owner.cookie()),
                None,
            )
            .await
        }
    });
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release the resources lock");
    let acceptance = acceptance.await.expect("join");
    let revocation = revocation.await.expect("join");
    let outcome = (acceptance.status, revocation.status);
    assert!(
        outcome == (201, 404) || outcome == (404, 204),
        "{acceptance:?} {revocation:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acceptance_racing_an_operator_change_to_its_tenant_is_never_a_500() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    // An approval, then a rename: both write the tenant's documents.
    for approve in [true, false] {
        let status = if approve {
            TenantStatus::Pending
        } else {
            TenantStatus::Active
        };
        let tenant = cp.seed_tenant(status).await;
        let r = cp
            .send(
                "POST",
                &format!("/admin/tenants/{tenant}/invites"),
                Some(op.cookie()),
                Some(json!({"email": fresh_email("changing"), "role": "owner"})),
            )
            .await;
        assert_eq!(r.status, 201, "{r:?}");
        let token = text(&r.body, "/invite_token");
        let version = cp
            .get_with_cookie(&format!("/admin/tenants/{tenant}"), op.cookie())
            .await
            .body["version"]
            .as_i64()
            .expect("a version");

        // The operator's change waits for the tenant; the acceptance
        // queues behind it.
        let mut holder = cp.db().pool().begin().await.expect("begin");
        sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR UPDATE")
            .bind(tenant)
            .execute(&mut *holder)
            .await
            .expect("hold the tenant");
        let change = tokio::spawn({
            let (cp, cookie) = (cp.clone(), op.cookie().to_string());
            async move {
                if approve {
                    cp.send(
                        "POST",
                        &format!("/admin/tenants/{tenant}/approve"),
                        Some(&cookie),
                        None,
                    )
                    .await
                    .status
                } else {
                    send_if_match(
                        &cp,
                        &cookie,
                        "PATCH",
                        &format!("/admin/tenants/{tenant}"),
                        version,
                        json!({"name": "renamed-7c1"}),
                    )
                    .await
                }
            }
        });
        until_lock_waiters(&cp, 1).await;
        let acceptance = tokio::spawn({
            let (cp, token) = (cp.clone(), token.clone());
            async move { accept(&cp, &token, "changing-password-1", None).await }
        });
        until_lock_waiters(&cp, 2).await;
        holder.rollback().await.expect("release the tenant");
        let change = change.await.expect("join");
        let acceptance = acceptance.await.expect("join");
        assert_eq!(change, 200, "approve={approve}");
        assert_eq!(acceptance.status, 201, "approve={approve}: {acceptance:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_racing_a_disable_gets_no_session_back_on_re_enable() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let victim = cp.seed_member(tenant, MemberRole::Member).await;
    let id = victim.id.to_string();
    let (disable, login) = race_login_against(&cp, &victim.email, || {
        let (cp, op, id) = (cp.clone(), op.clone(), id.clone());
        async move { admin_patch_member(&cp, &op, &id, json!({"disabled": true})).await }
    })
    .await;
    assert_eq!(disable.status, 200, "{disable:?}");
    assert_eq!(member_sessions(&cp, &id).await, 0);
    // A session that slipped past the disable would come back with the
    // member.
    let r = admin_patch_member(&cp, &op, &id, json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    if let Some(cookie) = cookie_of(&login) {
        let s = cp.get_with_cookie("/api/session", &cookie).await;
        assert_eq!(
            s.body["authed"], false,
            "the raced login kept a session: {s:?}"
        );
    }
    assert_eq!(login.status, 401, "{login:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_racing_a_password_change_gets_no_lasting_session() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let victim = cp.seed_member(tenant, MemberRole::Member).await;
    let changer = cp.member_login_as(&victim).await;
    let (change, login) = race_login_against(&cp, &victim.email, || {
        let (cp, cookie) = (cp.clone(), changer.cookie().to_string());
        async move {
            cp.send(
                "POST",
                "/api/password",
                Some(&cookie),
                Some(json!({"current_password": TEST_PASSWORD, "new_password": "raced-new-password-1"})),
            )
            .await
        }
    })
    .await;
    assert_eq!(change.status, 204, "{change:?}");
    if let Some(cookie) = cookie_of(&login) {
        let s = cp.get_with_cookie("/api/session", &cookie).await;
        assert_eq!(
            s.body["authed"], false,
            "the raced login kept a session: {s:?}"
        );
    }
    assert_eq!(login.status, 401, "{login:?}");
    // Only the changer's own session is left.
    assert_eq!(member_sessions(&cp, &victim.id.to_string()).await, 1);
}

#[tokio::test]
async fn changing_ones_password_ends_ones_other_sessions() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let m = cp.seed_member(tenant, MemberRole::Billing).await;
    let here = cp.member_login_as(&m).await;
    let elsewhere = cp.member_login_as(&m).await;
    let change = |current: &str, new: &str| {
        let body = json!({"current_password": current, "new_password": new});
        let cookie = here.cookie().to_string();
        let cp = &cp;
        async move {
            cp.send("POST", "/api/password", Some(&cookie), Some(body))
                .await
        }
    };
    let r = change("wrong-password-7c1", "changed-password-7c1").await;
    assert_eq!(r.status, 401, "{r:?}");
    assert_eq!(r.body["code"], "invalid_credentials");
    let r = change(TEST_PASSWORD, "short").await;
    assert_eq!(r.status, 422, "{r:?}");
    assert_eq!(r.body["details"][0]["path"], "/new_password");
    assert_eq!(member_sessions(&cp, &m.id.to_string()).await, 2);

    let r = change(TEST_PASSWORD, "changed-password-7c1").await;
    assert_eq!(r.status, 204, "{r:?}");
    assert_eq!(
        cp.get_with_cookie("/api/org", here.cookie()).await.status,
        200
    );
    assert_eq!(
        cp.get_with_cookie("/api/org", elsewhere.cookie())
            .await
            .status,
        401
    );
    assert_eq!(cp.member_login(&m.email, TEST_PASSWORD).await.status, 401);
    assert_eq!(
        cp.member_login(&m.email, "changed-password-7c1")
            .await
            .status,
        200
    );
    let actions: Vec<String> = tenant_audit(&cp, &tenant.to_string())
        .await
        .into_iter()
        .map(|row| row.0)
        .collect();
    assert_eq!(actions, ["member.password"]);
    assert!(!dump(&cp, &["audit_log", "members"])
        .await
        .contains("changed-password-7c1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn of_two_password_changes_from_one_password_the_second_is_refused() {
    // Both changes verify the same current password before either writes.
    // The first commits; the second's current password is no longer the
    // member's, so it must neither replace the new password nor end the
    // session the first change kept.
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let m = cp.seed_member(tenant, MemberRole::Member).await;
    let first_session = cp.member_login_as(&m).await;
    let second_session = cp.member_login_as(&m).await;
    let holder = hold_audit_inserts(&cp).await;

    let change = |session: &MemberSession, new: &'static str| {
        let (cp, cookie) = (cp.clone(), session.cookie().to_string());
        tokio::spawn(async move {
            cp.send(
                "POST",
                "/api/password",
                Some(&cookie),
                Some(json!({"current_password": TEST_PASSWORD, "new_password": new})),
            )
            .await
        })
    };
    let first = change(&first_session, "first-new-password-7c1");
    until_lock_waiters(&cp, 1).await;
    let second = change(&second_session, "second-new-password-7c1");
    until_lock_waiters(&cp, 2).await;
    holder.rollback().await.expect("release audit_log");

    let first = first.await.expect("join");
    assert_eq!(first.status, 204, "{first:?}");
    let second = second.await.expect("join");
    assert_eq!(second.status, 401, "{second:?}");
    assert_eq!(second.body["code"], "invalid_credentials", "{second:?}");
    assert_eq!(
        cp.member_login(&m.email, "second-new-password-7c1")
            .await
            .status,
        401
    );
    let s = cp
        .get_with_cookie("/api/session", first_session.cookie())
        .await;
    assert_eq!(s.body["authed"], true, "{s:?}");
    let s = cp
        .get_with_cookie("/api/session", second_session.cookie())
        .await;
    assert_eq!(s.body["authed"], false, "{s:?}");
    assert_eq!(
        cp.member_login(&m.email, "first-new-password-7c1")
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn an_operator_invites_into_a_tenant_and_disables_its_members() {
    let Some(cp) = TestCp::start_with(|cfg| {
        cfg.portal_url = Some("https://portal.example.test/".to_string());
    })
    .await
    else {
        return;
    };
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let viewer = cp.admin_login_seeded(OperatorRole::Viewer).await;
    let t = cp
        .send(
            "POST",
            "/admin/tenants",
            Some(op.cookie()),
            Some(json!({"name": "ops-7c1"})),
        )
        .await;
    assert_eq!(t.status, 201, "{t:?}");
    let tenant = text(&t.body, "/id");

    // An operator's invitation, with its link.
    let owner_email = fresh_email("ops-owner");
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/invites"),
            Some(op.cookie()),
            Some(json!({"email": owner_email, "role": "owner"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let token = text(&r.body, "/invite_token");
    assert_eq!(
        r.body["invite_url"],
        format!("https://portal.example.test/invite#token={token}")
    );
    let owner = accept(&cp, &token, "ops-owner-password", None).await;
    assert_eq!(owner.status, 201, "{owner:?}");
    assert_eq!(owner.body["member"]["role"], "owner");
    let owner_id = text(&owner.body, "/member/id");
    let owner_cookie = cookie_of(&owner).expect("a cookie");

    // A tenant's own invitation carries the link too.
    let r = cp
        .send(
            "POST",
            "/api/members/invites",
            Some(&owner_cookie),
            Some(json!({"email": fresh_email("ops-member"), "role": "member"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(
        r.body["invite_url"],
        format!(
            "https://portal.example.test/invite#token={}",
            text(&r.body, "/invite_token")
        )
    );
    let joined = accept(
        &cp,
        &text(&r.body, "/invite_token"),
        "ops-member-password",
        None,
    )
    .await;
    let member_id = text(&joined.body, "/member/id");
    let member_cookie = cookie_of(&joined).expect("a cookie");

    let listed = cp
        .get_with_cookie(&format!("/admin/tenants/{tenant}/members"), viewer.cookie())
        .await;
    assert_eq!(listed.status, 200, "{listed:?}");
    assert_eq!(
        ids(listed.body["items"].as_array().expect("items")),
        [owner_id.clone(), member_id.clone()]
    );
    let t = cp
        .get_with_cookie(&format!("/admin/tenants/{tenant}"), viewer.cookie())
        .await;
    assert_eq!(t.body["member_count"], 2);

    // Disabling a member ends their sessions; enabling them lets them log
    // in again, but brings no old session back.
    let r = admin_patch_member(&cp, &op, &member_id, json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["disabled"], true);
    assert_eq!(r.body["id"], member_id.as_str());
    assert_eq!(
        cp.get_with_cookie("/api/org", &member_cookie).await.status,
        401
    );
    let email = text(&joined.body, "/member/email");
    assert_eq!(
        cp.member_login(&email, "ops-member-password").await.status,
        401
    );
    // Unchanged twice is no change.
    let r = admin_patch_member(&cp, &op, &member_id, json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let r = admin_patch_member(&cp, &op, &member_id, json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.body["disabled"], false);
    assert_eq!(
        cp.get_with_cookie("/api/org", &member_cookie).await.status,
        401
    );
    assert_eq!(
        cp.member_login(&email, "ops-member-password").await.status,
        200
    );
    let audit: Vec<(String, String)> = tenant_audit(&cp, &tenant)
        .await
        .into_iter()
        .filter(|row| row.0.starts_with("member."))
        .map(|row| (row.0, row.1))
        .collect();
    let expected: Vec<(String, String)> = [
        ("member.invite", "operator"),
        ("member.join", "member"),
        ("member.invite", "member"),
        ("member.join", "member"),
        ("member.disable", "operator"),
        ("member.enable", "operator"),
    ]
    .iter()
    .map(|(a, k)| (a.to_string(), k.to_string()))
    .collect();
    assert_eq!(audit, expected);

    // Nothing to invite into or change: the platform's own tenant, a tenant
    // that does not exist, a member that does not exist or was removed.
    let tenants = cp.get_with_cookie("/admin/tenants", viewer.cookie()).await;
    let internal = tenants.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|t| t["is_internal"] == true)
        .map(|t| text(t, "/id"))
        .expect("the internal tenant");
    let body = json!({"email": fresh_email("internal"), "role": "owner"});
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{internal}/invites"),
            Some(op.cookie()),
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "tenant_state");
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{}/invites", Uuid::new_v4()),
            Some(op.cookie()),
            Some(body),
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    let r = cp
        .get_with_cookie(
            &format!("/admin/tenants/{}/members", Uuid::new_v4()),
            viewer.cookie(),
        )
        .await;
    assert_eq!(r.status, 404, "{r:?}");
    let r = admin_patch_member(
        &cp,
        &op,
        &Uuid::new_v4().to_string(),
        json!({"disabled": true}),
    )
    .await;
    assert_eq!(r.status, 404, "{r:?}");
    let r = cp
        .send(
            "DELETE",
            &format!("/api/members/{member_id}"),
            Some(&owner_cookie),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{r:?}");
    let r = admin_patch_member(&cp, &op, &member_id, json!({"disabled": true})).await;
    assert_eq!(r.status, 404, "{r:?}");
}

/// The invitations `cookie`'s member lists as open.
async fn open_invites(cp: &TestCp, cookie: &str) -> Vec<Value> {
    let r = cp.get_with_cookie("/api/members/invites", cookie).await;
    assert_eq!(r.status, 200, "{r:?}");
    r.body["items"].as_array().expect("items").clone()
}

#[tokio::test]
async fn an_invite_dies_with_its_issuers_authority() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let issue = |by: &MemberSession, role: &'static str| {
        let (cp, by) = (&cp, by.clone());
        async move {
            let r = invite(cp, &by, &fresh_email("issued"), role).await;
            assert_eq!(r.status, 201, "{r:?}");
            (text(&r.body, "/id"), text(&r.body, "/invite_token"))
        }
    };
    // Invitations nobody below loses authority over, which must outlive
    // every revocation: an operator's, and an owner invitation from the
    // owner who stays.
    let r = cp
        .send(
            "POST",
            &format!("/admin/tenants/{tenant}/invites"),
            Some(op.cookie()),
            Some(json!({"email": fresh_email("from-ops"), "role": "member"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let ops_invite = text(&r.body, "/id");
    let (own_invite, _) = issue(&owner, "owner").await;
    let kept = [ops_invite.clone(), own_invite.clone()];
    let with = |id: &str| [kept[0].clone(), kept[1].clone(), id.to_string()];

    // An owner stepping down to admin loses only their owner invitations.
    let second = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let (_, owner_token) = issue(&second, "owner").await;
    let (member_invite, member_token) = issue(&second, "member").await;
    let r = patch_member(&cp, &owner, &second.id.to_string(), "admin").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        ids(&open_invites(&cp, owner.cookie()).await),
        with(&member_invite)
    );
    // Down to member, they lose the rest.
    let r = patch_member(&cp, &owner, &second.id.to_string(), "member").await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(ids(&open_invites(&cp, owner.cookie()).await), kept);

    // A disabled admin's invitations go, and so do a removed one's.
    let disabled = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    let (_, disabled_token) = issue(&disabled, "member").await;
    let r = admin_patch_member(
        &cp,
        &op,
        &disabled.id.to_string(),
        json!({"disabled": true}),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(ids(&open_invites(&cp, owner.cookie()).await), kept);
    let r = invite(&cp, &owner, &fresh_email("eve"), "admin").await;
    let eve = accept(
        &cp,
        &text(&r.body, "/invite_token"),
        "eve-password-7c1",
        Some("Eve 7c1"),
    )
    .await;
    assert_eq!(eve.status, 201, "{eve:?}");
    let eve_id = text(&eve.body, "/member/id");
    let eve_cookie = cookie_of(&eve).expect("a cookie");
    let r = cp
        .send(
            "POST",
            "/api/members/invites",
            Some(&eve_cookie),
            Some(json!({"email": fresh_email("from-eve"), "role": "billing"})),
        )
        .await;
    assert_eq!(r.status, 201, "{r:?}");
    let (eve_invite, eve_token) = (text(&r.body, "/id"), text(&r.body, "/invite_token"));
    // The list names who issued each invitation; an operator's names no
    // member.
    let listed = open_invites(&cp, owner.cookie()).await;
    assert_eq!(ids(&listed), with(&eve_invite));
    assert_eq!(listed[0]["invited_by"], Value::Null);
    assert_eq!(
        listed[1]["invited_by"],
        json!({"id": owner.id, "display_name": null})
    );
    assert_eq!(
        listed[2]["invited_by"],
        json!({"id": eve_id, "display_name": "Eve 7c1"})
    );
    assert_eq!(remove_member(&cp, &owner, &eve_id).await.status, 204);
    assert_eq!(ids(&open_invites(&cp, owner.cookie()).await), kept);

    // None of theirs can be accepted any more.
    for token in [owner_token, member_token, disabled_token, eve_token] {
        let r = accept(&cp, &token, "too-late-password-1", None).await;
        assert_eq!(r.status, 404, "{r:?}");
    }
}

#[tokio::test]
async fn signups_are_counted_across_instances() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    // Sixty signups in the last minute, through other instances.
    sqlx::query(
        "INSERT INTO audit_log (actor_kind, action, target_kind) \
         SELECT 'member', 'tenant.signup', 'tenant' FROM generate_series(1, 60)",
    )
    .execute(cp.db().pool())
    .await
    .expect("record signups");
    let tenants = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tenants")
            .fetch_one(cp.db().pool())
            .await
            .expect("count tenants")
    };
    let before = tenants().await;
    let r = signup(&cp, "counted-7c1", &fresh_email("counted"), None).await;
    assert_eq!(r.status, 429, "{r:?}");
    assert_eq!(r.body["code"], "busy");
    assert!(r.set_cookie.is_none());
    assert_eq!(tenants().await, before);
    // A minute later they no longer count.
    sqlx::query(
        "UPDATE audit_log SET at = now() - interval '61 seconds' WHERE action = 'tenant.signup'",
    )
    .execute(cp.db().pool())
    .await
    .expect("age the signups");
    let r = signup(&cp, "counted-7c1", &fresh_email("later"), None).await;
    assert_eq!(r.status, 201, "{r:?}");
}

// 直接创建只增加当前组织的账号，不替换管理员的会话。
async fn create_account(cp: &TestCp, by: &MemberSession, body: Value) -> Resp {
    cp.send("POST", "/api/members", Some(by.cookie()), Some(body))
        .await
}

#[tokio::test]
async fn direct_account_owner_and_admin_create_login_ready_members_without_changing_session() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let admin = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    for (by, role) in [
        (&owner, "owner"),
        (&admin, "admin"),
        (&admin, "member"),
        (&admin, "billing"),
    ] {
        let email = fresh_email("direct");
        let r = create_account(
            &cp,
            by,
            json!({
                "email": format!("  {}  ", email.to_ascii_uppercase()),
                "password": "direct-created-password-7c1", "role": role,
                "display_name": "Direct 7c1",
            }),
        )
        .await;
        assert_eq!(r.status, 201, "{r:?}");
        assert!(r.set_cookie.is_none(), "管理员会话不得被替换");
        let id = text(&r.body, "/id");
        let created_at = text(&r.body, "/created_at");
        assert_eq!(
            r.body,
            json!({"id": id, "email": email, "display_name": "Direct 7c1",
            "role": role, "disabled": false, "created_at": created_at})
        );
        let login = cp.member_login(&email, "direct-created-password-7c1").await;
        assert_eq!(login.status, 200, "{login:?}");
        assert_eq!(login.body["member"]["id"], id);
        assert_eq!(login.body["member"]["role"], role);
        assert_eq!(login.body["tenant"]["id"], tenant.to_string());
        let session = cp.get_with_cookie("/api/session", by.cookie()).await;
        assert_eq!(session.body["member"]["id"], by.id.to_string());
        assert!(listed_members(&cp, owner.cookie())
            .await
            .iter()
            .any(|m| m["id"] == id));
        let user = resource(&cp, "users", &id).await.expect("已投影用户");
        assert!(!user.2);
        assert_eq!(user.1, Some(format!("member:{id}")));
        assert!(!user.0.to_string().contains(&email));
        let hash: String =
            sqlx::query_scalar("SELECT password_hash FROM members WHERE id = $1::uuid")
                .bind(&id)
                .fetch_one(cp.db().pool())
                .await
                .expect("密码哈希");
        assert!(hash.starts_with("$argon2"));
        assert!(!r.body.to_string().contains(&hash));
        let audit = tenant_audit(&cp, &tenant.to_string()).await;
        assert!(audit.iter().any(|a| a.0 == "member.create"
            && a.2 == Some(by.id)
            && a.3 == "member"
            && a.4.as_deref() == Some(id.as_str())));
        let stored = dump(&cp, &["members", "audit_log", "gw_resources"]).await;
        assert!(!stored.contains("direct-created-password-7c1"));
        assert!(!dump(&cp, &["audit_log", "gw_resources"])
            .await
            .contains(&hash));
    }
}

#[tokio::test]
async fn direct_account_requires_manager_and_only_owner_grants_owner() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let before_members = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM members WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(cp.db().pool())
            .await
            .expect("成员数")
    };
    let admin = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    let member = cp.member_login_seeded(tenant, MemberRole::Member).await;
    let billing = cp.member_login_seeded(tenant, MemberRole::Billing).await;
    let before = before_members().await;
    let audit = audit_rows(&cp).await;
    for (by, role) in [(&admin, "owner"), (&member, "member"), (&billing, "member")] {
        let r = create_account(
            &cp,
            by,
            json!({"email": fresh_email("refused-direct"),
            "password": "refused-direct-password-7c1", "role": role}),
        )
        .await;
        assert_eq!(r.status, 403, "{r:?}");
        assert_eq!(r.body["code"], "forbidden");
    }
    assert_eq!(before_members().await, before);
    assert_eq!(audit_rows(&cp).await, audit);
}

#[tokio::test]
async fn direct_account_refuses_pending_and_suspended_tenants() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    for status in [TenantStatus::Pending, TenantStatus::Suspended] {
        let tenant = cp.seed_tenant(status).await;
        let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
        let before = audit_rows(&cp).await;
        let email = fresh_email("inactive-direct");
        let r = create_account(
            &cp,
            &owner,
            json!({"email": email,
            "password": "inactive-direct-password-7c1", "role": "member"}),
        )
        .await;
        assert_eq!(r.status, 403, "{r:?}");
        assert_eq!(r.body["code"], "tenant_not_active");
        assert_eq!(audit_rows(&cp).await, before);
        assert_eq!(
            cp.member_login(&email, "inactive-direct-password-7c1")
                .await
                .status,
            401
        );
    }
}

#[tokio::test]
async fn direct_account_duplicate_email_never_changes_existing_credentials_or_tenant() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let other = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    for victim_tenant in [tenant, other] {
        let victim = cp.seed_member(victim_tenant, MemberRole::Billing).await;
        let original = cp.member_login_as(&victim).await;
        let before = dump(&cp, &["members", "gw_resources", "audit_log"]).await;
        let r = create_account(&cp, &owner, json!({
            "email": format!(" {} ", victim.email.to_ascii_uppercase()),
            "password": "replacement-direct-password-7c1", "role": "owner", "display_name": "Overwrite",
        })).await;
        assert_eq!(r.status, 409, "{r:?}");
        assert_eq!(r.body["code"], "email_taken");
        assert_eq!(
            dump(&cp, &["members", "gw_resources", "audit_log"]).await,
            before
        );
        assert_eq!(
            cp.member_login(&victim.email, TEST_PASSWORD).await.status,
            200
        );
        assert_eq!(
            cp.member_login(&victim.email, "replacement-direct-password-7c1")
                .await
                .status,
            401
        );
        let s = cp.get_with_cookie("/api/session", original.cookie()).await;
        assert_eq!(s.body["tenant"]["id"], victim_tenant.to_string());
        assert_eq!(s.body["member"]["role"], "billing");
    }
}

#[tokio::test]
async fn direct_account_validates_fields_and_rejects_cross_tenant_override() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let other = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let base = json!({"email": fresh_email("invalid-direct"), "password": "valid-direct-password-7c1", "role": "member"});
    let before = dump(&cp, &["members", "gw_resources", "audit_log"]).await;
    for (field, value) in [
        ("email", json!("invalid")),
        ("password", json!("short")),
        ("role", json!("operator")),
        ("tenant_id", json!(other)),
        ("display_name", json!("x".repeat(129))),
    ] {
        let mut body = base.clone();
        body[field] = value;
        let r = create_account(&cp, &owner, body).await;
        assert_eq!(r.status, 422, "field={field}: {r:?}");
        assert!(!r.body.to_string().contains("valid-direct-password-7c1"));
        assert!(!r.body.to_string().contains("$argon2"));
    }
    assert_eq!(
        dump(&cp, &["members", "gw_resources", "audit_log"]).await,
        before
    );
    // 不带显示名也能创建，且只能进入调用者当前组织。
    let r = create_account(&cp, &owner, base).await;
    assert_eq!(r.status, 201, "{r:?}");
    assert_eq!(r.body["display_name"], Value::Null);
    let login = cp
        .member_login(&text(&r.body, "/email"), "valid-direct-password-7c1")
        .await;
    assert_eq!(login.status, 200, "{login:?}");
    assert_eq!(login.body["tenant"]["id"], tenant.to_string());
}

#[tokio::test]
async fn direct_account_disabled_email_stays_reserved_without_password_reset() {
    let Some(cp) = TestCp::start().await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let victim = cp.seed_member(tenant, MemberRole::Member).await;
    let op = cp.admin_login_seeded(OperatorRole::Operator).await;
    let r = admin_patch_member(&cp, &op, &victim.id.to_string(), json!({"disabled": true})).await;
    assert_eq!(r.status, 200, "{r:?}");
    let before = dump(&cp, &["members", "gw_resources", "audit_log"]).await;
    let r = create_account(
        &cp,
        &owner,
        json!({"email": victim.email,
        "password": "disabled-reset-password-7c1", "role": "admin"}),
    )
    .await;
    assert_eq!(r.status, 409, "{r:?}");
    assert_eq!(r.body["code"], "email_taken");
    assert_eq!(
        dump(&cp, &["members", "gw_resources", "audit_log"]).await,
        before
    );
    let r = admin_patch_member(&cp, &op, &victim.id.to_string(), json!({"disabled": false})).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(
        cp.member_login(&victim.email, TEST_PASSWORD).await.status,
        200
    );
    assert_eq!(
        cp.member_login(&victim.email, "disabled-reset-password-7c1")
            .await
            .status,
        401
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_account_concurrent_duplicate_creates_exactly_one_account() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let email = fresh_email("concurrent-direct");
    let body =
        json!({"email": email, "password": "concurrent-direct-password-7c1", "role": "member"});
    let (a, b) = tokio::join!(
        create_account(&cp, &owner, body.clone()),
        create_account(&cp, &owner, body)
    );
    let mut statuses = [a.status, b.status];
    statuses.sort();
    assert_eq!(statuses, [201, 409], "{a:?}; {b:?}");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM members WHERE email = $1")
        .bind(&email)
        .fetch_one(cp.db().pool())
        .await
        .expect("唯一账号");
    assert_eq!(count, 1);
    assert_eq!(
        cp.member_login(&email, "concurrent-direct-password-7c1")
            .await
            .status,
        200
    );
    assert_eq!(
        tenant_audit(&cp, &tenant.to_string())
            .await
            .iter()
            .filter(|a| a.0 == "member.create")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_account_rechecks_authority_after_waiting_for_tenant_lock() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let admin = cp.member_login_seeded(tenant, MemberRole::Admin).await;
    let holder = hold_audit_inserts(&cp).await;
    let demotion = tokio::spawn({
        let (cp, owner, id) = (cp.clone(), owner.clone(), admin.id.to_string());
        async move { patch_member(&cp, &owner, &id, "member").await }
    });
    until_lock_waiters(&cp, 1).await;
    let email = fresh_email("raced-direct");
    let creation = tokio::spawn({
        let (cp, email) = (cp.clone(), email.clone());
        async move {
            create_account(
                &cp,
                &admin,
                json!({"email": email,
            "password": "raced-direct-password-7c1", "role": "member"}),
            )
            .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !creation.is_finished() && lock_waiters(&cp).await < 2 {
        assert!(Instant::now() < deadline, "创建未到达组织锁");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    holder.rollback().await.expect("释放审计锁");
    let demotion = demotion.await.expect("降级任务");
    assert_eq!(demotion.status, 200, "{demotion:?}");
    let r = creation.await.expect("创建任务");
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.body["code"], "forbidden");
    assert_eq!(
        cp.member_login(&email, "raced-direct-password-7c1")
            .await
            .status,
        401
    );
    assert!(!tenant_audit(&cp, &tenant.to_string())
        .await
        .iter()
        .any(|a| a.0 == "member.create"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_account_rechecks_owner_grant_after_owner_becomes_admin() {
    let Some(cp) = TestCp::start_with(patient).await else {
        return;
    };
    let cp = Arc::new(cp);
    let tenant = cp.seed_tenant(TenantStatus::Active).await;
    let owner = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let second = cp.member_login_seeded(tenant, MemberRole::Owner).await;
    let holder = hold_audit_inserts(&cp).await;
    let demotion = tokio::spawn({
        let (cp, owner, id) = (cp.clone(), owner.clone(), second.id.to_string());
        async move { patch_member(&cp, &owner, &id, "admin").await }
    });
    until_lock_waiters(&cp, 1).await;
    let email = fresh_email("owner-raced-direct");
    let creation = tokio::spawn({
        let (cp, email) = (cp.clone(), email.clone());
        async move {
            create_account(
                &cp,
                &second,
                json!({"email": email,
                "password": "owner-raced-direct-password-7c1", "role": "owner"}),
            )
            .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !creation.is_finished() && lock_waiters(&cp).await < 2 {
        assert!(Instant::now() < deadline, "创建未到达组织锁");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    holder.rollback().await.expect("释放审计锁");
    let demotion = demotion.await.expect("降级任务");
    assert_eq!(demotion.status, 200, "{demotion:?}");
    let r = creation.await.expect("创建任务");
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.body["code"], "forbidden");
    assert_eq!(owners(&cp, tenant).await, 1);
    assert_eq!(
        cp.member_login(&email, "owner-raced-direct-password-7c1")
            .await
            .status,
        401
    );
    assert!(!tenant_audit(&cp, &tenant.to_string())
        .await
        .iter()
        .any(|a| a.0 == "member.create"));
}
