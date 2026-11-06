//! Tenants and their members for the tests: tenants created the way the
//! control plane creates them (`tenants::create`), members the way it adds
//! them (`members::create`), each with [`TEST_PASSWORD`], and logged in
//! through `POST /api/login`.

use aisix_control_plane::audit::Actor;
use aisix_control_plane::auth::password;
use aisix_control_plane::auth::MemberRole;
use aisix_control_plane::members;
use aisix_control_plane::resources::write::WriteCtx;
use aisix_control_plane::tenants::{self, BillingMode, TenantStatus};
use futures::FutureExt;
use serde_json::{json, Value};
use uuid::Uuid;

use super::{Resp, TestCp, TEST_PASSWORD, TEST_PASSWORD_HASH};

/// A member the harness added.
#[derive(Debug, Clone)]
pub struct SeededMember {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub email: String,
    pub role: MemberRole,
}

/// A logged-in member.
#[derive(Debug, Clone)]
pub struct MemberSession {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub email: String,
    pub role: MemberRole,
    cookie: String,
}

impl MemberSession {
    /// `aisix_cp_portal=<token>`, for a `Cookie` header.
    pub fn cookie(&self) -> &str {
        &self.cookie
    }
}

/// The request the tenant route `method path` is tested with: the concrete
/// path and the body. Ids name nothing and bodies are refused once the
/// identity checks have passed, except `POST /api/members/invites`, which
/// creates an invitation. Called once per attempt.
pub fn tenant_fixture(method: &str, path: &str) -> (String, Option<Value>) {
    match (method, path) {
        // Rejected as too short, after the identity checks.
        ("POST", "/api/password") => (
            path.to_string(),
            Some(json!({"current_password": TEST_PASSWORD, "new_password": "short"})),
        ),
        ("GET", "/api/org" | "/api/models" | "/api/members" | "/api/members/invites") => {
            (path.to_string(), None)
        }
        ("POST", "/api/members/invites") => (
            path.to_string(),
            Some(json!({
                "email": format!("fixture-{}@tenant.test", Uuid::new_v4().simple()),
                "role": "member",
            })),
        ),
        ("DELETE", "/api/members/invites/{id}") => {
            (format!("/api/members/invites/{}", Uuid::new_v4()), None)
        }
        ("PATCH", "/api/members/{id}") => (
            format!("/api/members/{}", Uuid::new_v4()),
            Some(json!({"role": "member"})),
        ),
        ("DELETE", "/api/members/{id}") => (format!("/api/members/{}", Uuid::new_v4()), None),
        _ => panic!("no fixture for {method} {path}: add one to tests/support/members.rs"),
    }
}

/// The session cookie a login, signup or invite acceptance set, as a
/// `Cookie` header value.
pub fn cookie_of(r: &Resp) -> Option<String> {
    r.set_cookie
        .as_deref()
        .and_then(|c| c.split(';').next())
        .map(str::to_string)
}

impl TestCp {
    /// A new ordinary tenant in `status`, with the tenant defaults.
    pub async fn seed_tenant(&self, status: TenantStatus) -> Uuid {
        let ctx = WriteCtx::of(self.state());
        let name = format!("tenant-{}", Uuid::new_v4().simple());
        self.db()
            .tx(move |conn| {
                async move {
                    tenants::create(
                        conn,
                        &ctx,
                        &name,
                        status,
                        BillingMode::Prepaid,
                        Actor::System,
                    )
                    .await
                }
                .boxed()
            })
            .await
            .expect("create a tenant")
            .id
    }

    /// A member of `tenant_id` with a fresh email and [`TEST_PASSWORD`].
    pub async fn seed_member(&self, tenant_id: Uuid, role: MemberRole) -> SeededMember {
        let hash = TEST_PASSWORD_HASH
            .get_or_init(|| async {
                tokio::task::spawn_blocking(|| password::hash(TEST_PASSWORD))
                    .await
                    .expect("join the hashing task")
                    .expect("hash the test password")
            })
            .await
            .clone();
        let email = format!("{}-{}@tenant.test", role.as_str(), Uuid::new_v4().simple());
        let ctx = WriteCtx::of(self.state());
        let stored = email.clone();
        let row = self
            .db()
            .tx(move |conn| {
                async move {
                    // As `members::create` asks of a caller adding to an
                    // existing tenant.
                    tenants::lock(&mut *conn, tenant_id).await?;
                    members::create(conn, &ctx, tenant_id, &stored, &hash, None, role).await
                }
                .boxed()
            })
            .await
            .expect("add a member");
        SeededMember {
            id: row.id,
            tenant_id,
            email,
            role,
        }
    }

    /// `POST /api/login`.
    pub async fn member_login(&self, email: &str, password: &str) -> Resp {
        self.send(
            "POST",
            "/api/login",
            None,
            Some(serde_json::json!({ "email": email, "password": password })),
        )
        .await
    }

    /// Logs in as `m`, which must succeed.
    pub async fn member_login_as(&self, m: &SeededMember) -> MemberSession {
        let r = self.member_login(&m.email, TEST_PASSWORD).await;
        assert_eq!(r.status, 200, "log in as {}: {r:?}", m.email);
        MemberSession {
            id: m.id,
            tenant_id: m.tenant_id,
            email: m.email.clone(),
            role: m.role,
            cookie: cookie_of(&r).expect("a session cookie"),
        }
    }

    /// Adds a member with `role` to `tenant_id` and logs in as it.
    pub async fn member_login_seeded(&self, tenant_id: Uuid, role: MemberRole) -> MemberSession {
        let m = self.seed_member(tenant_id, role).await;
        self.member_login_as(&m).await
    }
}
