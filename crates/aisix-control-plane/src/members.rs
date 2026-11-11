//! Tenant members: self-service signup, member sessions (login, logout, the
//! session, password changes), invitations, roles and removal, what a
//! member reads about their tenant (`/api/org`, `/api/models`), and the
//! operators' member routes.
//!
//! Rules every handler here follows:
//! - An id a member names, in the path or the body, is looked up within the
//!   member's own tenant first, so another tenant's id answers exactly as an
//!   id that names nothing does (`404 not_found`).
//! - A change to members (an invitation, a role, a disable, a removal) takes
//!   the tenant's lock ([`tenants::lock`]) and only then reads what it
//!   decides on, the caller included ([`MemberAuth::fetch`]): a caller
//!   demoted or removed meanwhile is refused, and two changes cannot each
//!   count the other's target as the owner who remains.
//! - Each member of an approved tenant has a `users` document,
//!   `users/<member id>`, named after the display name and never the email,
//!   and owned by `member:<id>`. A pending tenant's members have none until
//!   the approval writes them.
//! - Login, signup and invitation acceptance end the session the request
//!   carried and start a new one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, AuditEntry};
use crate::auth::password::{self, normalize_email, LoginThrottle, MIN_PASSWORD_LEN};
use crate::auth::session::{self, Principal, PrincipalKind, MEMBER_SESSION_TTL, PORTAL_COOKIE};
use crate::auth::{MemberAuth, MemberRole, OperatorAuth, OperatorRole, KEY_HOLDERS};
use crate::db::DbError;
use crate::error::{ApiError, Detail};
use crate::http::{ApiJson, API_CONTRACT};
use crate::keys;
use crate::operators::cooling_down;
use crate::resources::write::{
    write_resource, ResourceWrite, WriteCtx, WriteOp, RESOURCES_LOCK_KEY,
};
use crate::resources::Kind;
use crate::tenants::{self, BillingMode, TenantLimits, TenantStatus};
use crate::AppState;

/// How long an invitation can be accepted.
pub const INVITE_TTL: Duration = Duration::from_secs(7 * 86_400);

/// Signups accepted per minute across the site. Signup is unauthenticated
/// and verifies no email, and each one creates a tenant and a member, so it
/// is bounded as a whole: counted in the database
/// (the `tenant.signup` audit rows of the last minute), whichever instance
/// took them, and first in this process, which turns a flood away before
/// any argon2 work.
pub const SIGNUPS_PER_MINUTE: usize = 60;

/// Serialises the signups' count of the last minute against their own
/// rows, so two instances cannot both admit the sixtieth. A signup takes it
/// first in its transaction, before any other lock, and nothing else takes
/// it.
pub const SIGNUP_LOCK_KEY: i64 = 0x6169_7369_7863_7073;

const SIGNUP_WINDOW: Duration = Duration::from_secs(60);

/// Tenants waiting for approval, at most. Signup is anonymous and nothing
/// removes a tenant, so without a cap anyone could grow the tenants table
/// and the operators' tenant list without bound. Past the cap a signup is
/// refused as a full signup window refuses it, which tells the caller
/// nothing new. Counted under [`SIGNUP_LOCK_KEY`].
pub const MAX_PENDING_TENANTS: i64 = 1_000;

/// The longest display name accepted.
const MAX_DISPLAY_NAME_CHARS: usize = 128;

/// Hex characters in an invitation token (32 random bytes).
const TOKEN_LEN: usize = 64;

const TARGET_KIND: &str = "member";
const INVITE_TARGET_KIND: &str = "member_invite";

/// The signup window: when each signup of the last minute took its slot.
/// Kept in this process, like the login cooldown. Cheap to clone.
#[derive(Clone, Default)]
pub struct SignupThrottle {
    taken: Arc<Mutex<VecDeque<Instant>>>,
}

impl SignupThrottle {
    /// A slot for one signup, or `None` when the minute's are taken.
    pub fn try_take(&self) -> Option<Instant> {
        self.try_take_at(Instant::now())
    }

    /// Hands back the slot `taken` gave, for a signup that did not happen:
    /// otherwise resubmitting a taken email would shut signup for everyone
    /// for a minute.
    pub fn give_back(&self, taken: Instant) {
        let mut slots = self.lock();
        if let Some(at) = slots.iter().rposition(|t| *t == taken) {
            slots.remove(at);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Instant>> {
        // The queue stays consistent whatever panicked while holding it.
        self.taken.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn try_take_at(&self, now: Instant) -> Option<Instant> {
        let mut slots = self.lock();
        while slots
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= SIGNUP_WINDOW)
        {
            slots.pop_front();
        }
        if slots.len() >= SIGNUPS_PER_MINUTE {
            return None;
        }
        slots.push_back(now);
        Some(now)
    }
}

/// What bounds the unauthenticated tenant routes: the login cooldown for
/// member emails (apart from the operators'), and the signup window.
#[derive(Clone, Default)]
pub struct PortalLimits {
    pub logins: LoginThrottle,
    pub signups: SignupThrottle,
}

/// One live row of `members`, without the password hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub role: MemberRole,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
}

type RawMember = (
    Uuid,
    Uuid,
    String,
    Option<String>,
    String,
    bool,
    DateTime<Utc>,
);

const COLUMNS: &str = "id, tenant_id, email, display_name, role, disabled, created_at";

impl MemberRow {
    fn from_raw(raw: RawMember) -> Result<MemberRow, ApiError> {
        let (id, tenant_id, email, display_name, role, disabled, created_at) = raw;
        Ok(MemberRow {
            id,
            tenant_id,
            email,
            display_name,
            role: role.parse().map_err(ApiError::internal)?,
            disabled,
            created_at,
        })
    }

    fn view(&self) -> MemberView {
        MemberView {
            id: self.id,
            email: self.email.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
            disabled: self.disabled,
            created_at: self.created_at,
        }
    }
}

/// `Member` in the API.
#[derive(Debug, Serialize)]
pub struct MemberView {
    id: Uuid,
    email: String,
    display_name: Option<String>,
    role: MemberRole,
    disabled: bool,
    created_at: DateTime<Utc>,
}

fn detail(path: &str, message: impl Into<String>) -> Detail {
    Detail {
        path: path.to_string(),
        message: message.into(),
    }
}

/// Who owns the documents derived from member `id`.
pub fn owner(id: Uuid) -> String {
    format!("member:{id}")
}

/// Member `id`'s `users` document: named after the display name, or
/// `member-<the id's first 8 characters>` without one, and never after the
/// email, which the gateways would put into telemetry.
pub fn user_doc(id: Uuid, display_name: Option<&str>) -> Value {
    let name = match display_name {
        Some(name) => name.to_string(),
        None => format!(
            "member-{}",
            id.simple().to_string().chars().take(8).collect::<String>()
        ),
    };
    json!({ "name": name })
}

fn email_taken() -> ApiError {
    ApiError::conflict("email_taken", "该邮箱已被使用")
}

/// Adds a member to `tenant_id` with an `email` already normalised and a
/// `password_hash`, and writes their `users` document unless the tenant is
/// pending, whose approval writes it ([`put_missing_users`]). A live member
/// with the same email, in any tenant, is `409 email_taken`.
///
/// The tenant's lock ([`tenants::lock`]) comes first, as the lock order
/// asks: the member row takes a key-share lock on the tenant row, and taken
/// after the resources lock below, a change holding the tenant while it
/// waits for the resources lock would deadlock with this. Every caller
/// holds it already (a signup adds to the tenant it has just created), so
/// here it is a no-op that reads the tenant's status.
///
/// The resources lock is taken before the member row, not only by the
/// document write after it. A transaction already holding that lock
/// (another member being added, which takes it here first) may be adding a
/// member with the same email, and it waits for any row this one inserted
/// first: taken after the insert, the lock would close a deadlock.
pub async fn create(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
    email: &str,
    password_hash: &str,
    display_name: Option<&str>,
    role: MemberRole,
) -> Result<MemberRow, ApiError> {
    let tenant = tenants::lock(&mut *conn, tenant_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RESOURCES_LOCK_KEY)
        .execute(&mut *conn)
        .await
        .map_err(DbError::from)?;
    let id = Uuid::new_v4();
    let raw: RawMember = sqlx::query_as(&format!(
        "INSERT INTO members (id, tenant_id, email, password_hash, display_name, role) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(tenant_id)
    .bind(email)
    .bind(password_hash)
    .bind(display_name)
    .bind(role.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| match DbError::from(e) {
        DbError::UniqueViolation { constraint } if constraint == "members_email_live" => {
            email_taken()
        }
        e => e.into(),
    })?;
    if tenant.status != TenantStatus::Pending {
        write_resource(
            conn,
            ctx,
            ResourceWrite {
                kind: Kind::Users,
                id,
                op: WriteOp::Create(&user_doc(id, display_name)),
                expected_version: None,
                managed_by: Some(&owner(id)),
            },
        )
        .await?;
    }
    MemberRow::from_raw(raw)
}

/// Writes the `users` document of every live member of `tenant_id` that has
/// none live, oldest member first: all of them when the tenant is approved,
/// since a pending tenant's members have none. Run by `tenants::project`,
/// at its users step, which holds the tenant's lock; one query when nothing
/// is missing.
pub(crate) async fn put_missing_users(
    conn: &mut PgConnection,
    ctx: &WriteCtx,
    tenant_id: Uuid,
) -> Result<(), ApiError> {
    let missing: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT m.id, m.display_name FROM members m \
         WHERE m.tenant_id = $1 AND m.removed_at IS NULL AND NOT EXISTS ( \
           SELECT 1 FROM gw_resources r WHERE r.kind = $2 AND r.id = m.id AND NOT r.deleted) \
         ORDER BY m.created_at, m.id",
    )
    .bind(tenant_id)
    .bind(Kind::Users.segment())
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    for (id, display_name) in missing {
        let doc = user_doc(id, display_name.as_deref());
        tenants::put(&mut *conn, ctx, Kind::Users, id, &doc, &owner(id)).await?;
    }
    Ok(())
}

/// The live member `id` of `tenant_id`, or `404 not_found`: a member of
/// another tenant is not found either.
async fn live_member(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<MemberRow, ApiError> {
    let raw: Option<RawMember> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM members WHERE id = $1 AND tenant_id = $2 AND removed_at IS NULL"
    ))
    .bind(id)
    .bind(tenant_id)
    .fetch_optional(conn)
    .await
    .map_err(DbError::from)?;
    MemberRow::from_raw(raw.ok_or_else(ApiError::not_found)?)
}

/// The live members of `tenant_id`, oldest first.
async fn members_of(conn: &mut PgConnection, tenant_id: Uuid) -> Result<Value, ApiError> {
    let raw: Vec<RawMember> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM members WHERE tenant_id = $1 AND removed_at IS NULL \
         ORDER BY created_at, id"
    ))
    .bind(tenant_id)
    .fetch_all(conn)
    .await
    .map_err(DbError::from)?;
    let items = raw
        .into_iter()
        .map(|r| MemberRow::from_raw(r).map(|row| row.view()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "items": items }))
}

/// The caller as they stand now, under their tenant's lock, checked again
/// for an active tenant and one of the `allowed` roles: the request was
/// authorised from a read made before the lock.
pub(crate) async fn current(
    conn: &mut PgConnection,
    auth: &MemberAuth,
    allowed: &[MemberRole],
) -> Result<MemberAuth, ApiError> {
    let now = locked_caller(conn, auth).await?;
    now.require_active()?;
    now.require(allowed)?;
    Ok(now)
}

/// [`current`] for a route a member of a tenant that is not active may
/// still call (`http::INACTIVE_TENANT_ROUTES`): the tenant's state is not
/// checked, the role is.
pub(crate) async fn current_in_any_state(
    conn: &mut PgConnection,
    auth: &MemberAuth,
    allowed: &[MemberRole],
) -> Result<MemberAuth, ApiError> {
    let now = locked_caller(conn, auth).await?;
    now.require(allowed)?;
    Ok(now)
}

/// The caller as they stand now, read under their tenant's lock; `401`
/// when they are gone.
async fn locked_caller(conn: &mut PgConnection, auth: &MemberAuth) -> Result<MemberAuth, ApiError> {
    tenants::lock(&mut *conn, auth.tenant_id).await?;
    MemberAuth::fetch(&mut *conn, auth.id)
        .await?
        .filter(|m| m.tenant_id == auth.tenant_id)
        .ok_or_else(ApiError::unauthenticated)
}

/// Revokes the open invitations member `issuer` of `tenant_id` issued:
/// every one when `all`, otherwise only those for an owner. An invitation
/// hands on its issuer's authority, so it goes when that authority does:
/// all of them when the issuer is removed, disabled or demoted to member or
/// billing, the owner ones when an owner becomes an admin. The caller holds
/// the tenant's lock, as an acceptance does, so none is accepted meanwhile.
async fn revoke_issued_invites(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    issuer: Uuid,
    all: bool,
) -> Result<u64, ApiError> {
    Ok(sqlx::query(
        "UPDATE member_invites SET revoked_at = now() \
         WHERE tenant_id = $1 AND created_by_kind = 'member' AND created_by = $2 \
           AND accepted_at IS NULL AND revoked_at IS NULL AND ($3 OR role = 'owner')",
    )
    .bind(tenant_id)
    .bind(issuer)
    .bind(all)
    .execute(conn)
    .await
    .map_err(DbError::from)?
    .rows_affected())
}

/// `409 last_owner` unless `tenant_id` keeps an enabled owner other than
/// `leaving`. The caller holds the tenant's lock, so no other change can be
/// counting `leaving` as the owner who remains.
async fn refuse_last_owner(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    leaving: Uuid,
) -> Result<(), ApiError> {
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM members \
         WHERE tenant_id = $1 AND role = 'owner' AND NOT disabled AND removed_at IS NULL \
           AND id <> $2",
    )
    .bind(tenant_id)
    .bind(leaving)
    .fetch_one(conn)
    .await
    .map_err(DbError::from)?;
    if others == 0 {
        return Err(ApiError::conflict(
            "last_owner",
            "至少要保留一个可用的所有者",
        ));
    }
    Ok(())
}

/// A malformed id names no member or invitation.
fn parse_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw).map_err(|_| ApiError::not_found())
}

/// The display name to store: trimmed, `None` when empty, or a detail at
/// `/display_name` when too long or holding control characters.
fn display_name(raw: Option<&str>) -> Result<Option<String>, Detail> {
    let Some(name) = raw.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_DISPLAY_NAME_CHARS {
        return Err(detail(
            "/display_name",
            format!("显示名称不能超过 {MAX_DISPLAY_NAME_CHARS} 个字符"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(detail("/display_name", "显示名称不能包含控制字符"));
    }
    Ok(Some(name.to_string()))
}

fn password_too_short(path: &str) -> Detail {
    detail(path, format!("密码至少需要 {MIN_PASSWORD_LEN} 个字符"))
}

fn digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Whether `token` has the shape of one [`create_invite`] issues. Anything
/// else is refused without a query.
fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `Session` in the API, for member `id`, who is logged in.
async fn session_view(conn: &mut PgConnection, id: Uuid) -> Result<Value, ApiError> {
    type Raw = (String, Option<String>, String, Uuid, String, String);
    let (email, display_name, role, tenant_id, tenant_name, status): Raw = sqlx::query_as(
        "SELECT m.email, m.display_name, m.role, t.id, t.name, t.status \
         FROM members m JOIN tenants t ON t.id = m.tenant_id WHERE m.id = $1",
    )
    .bind(id)
    .fetch_one(conn)
    .await
    .map_err(DbError::from)?;
    Ok(json!({
        "authed": true,
        "api_contract": API_CONTRACT,
        "member": {"id": id, "email": email, "display_name": display_name, "role": role},
        "tenant": {"id": tenant_id, "name": tenant_name, "status": status},
    }))
}

/// Starts member `id`'s session in place of the one the request carried,
/// if any: a session planted in the browser beforehand does not outlive a
/// login.
async fn start_session(
    conn: &mut PgConnection,
    id: Uuid,
    carried: Option<&str>,
) -> Result<String, ApiError> {
    if let Some(old) = carried {
        session::revoke(&mut *conn, old, PrincipalKind::Member).await?;
    }
    Ok(session::create(conn, Principal::Member(id), MEMBER_SESSION_TTL).await?)
}

/// The answer that sets the session cookie for `token`.
fn with_cookie(status: StatusCode, token: &str, body: Value) -> Response {
    let cookie = session::cookie(PORTAL_COOKIE, token, MEMBER_SESSION_TTL);
    (status, [(header::SET_COOKIE, cookie)], Json(body)).into_response()
}

fn carried_token(headers: &HeaderMap) -> Option<String> {
    session::token_from_headers(headers, PORTAL_COOKIE).map(str::to_string)
}

fn member_actor(auth: &MemberAuth) -> Actor<'_> {
    Actor::Member {
        id: auth.id,
        email: &auth.email,
    }
}

fn member_entry<'a>(
    actor: Actor<'a>,
    action: &'static str,
    tenant_id: Uuid,
    member_id: Uuid,
    detail: Value,
) -> AuditEntry<'a> {
    AuditEntry {
        actor,
        action,
        target_kind: TARGET_KIND,
        target_id: Some(member_id.to_string()),
        tenant_id: Some(tenant_id),
        detail,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignupRequest {
    org_name: String,
    email: String,
    password: String,
    display_name: Option<String>,
}

/// `POST /api/signup` → `201 Session` and the session cookie: a pending
/// tenant named `org_name`, with the caller as its owner, logged in. `409
/// email_taken` when a live member has the email; at most
/// [`SIGNUPS_PER_MINUTE`] a minute across the site, and at most
/// [`MAX_PENDING_TENANTS`] tenants waiting for approval (`429 busy` alike),
/// a refused signup handing its slot back.
pub async fn signup(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<SignupRequest>,
) -> Result<Response, ApiError> {
    let mut problems = Vec::new();
    let org = tenants::tenant_name(&req.org_name).unwrap_or_else(|d| {
        problems.push(Detail {
            path: "/org_name".to_string(),
            ..d
        });
        String::new()
    });
    let email = normalize_email(&req.email);
    if email.is_none() {
        problems.push(detail("/email", "邮箱格式不正确"));
    }
    if !password::long_enough(&req.password) {
        problems.push(password_too_short("/password"));
    }
    let name = display_name(req.display_name.as_deref()).unwrap_or_else(|d| {
        problems.push(d);
        None
    });
    let (Some(email), true) = (email, problems.is_empty()) else {
        return Err(ApiError::validation(problems));
    };
    let Some(slot) = state.portal.signups.try_take() else {
        return Err(signups_busy());
    };
    let signed_up = sign_up(&state, &headers, org, email, &req.password, name).await;
    if signed_up.is_err() {
        state.portal.signups.give_back(slot);
    }
    signed_up
}

fn signups_busy() -> ApiError {
    ApiError {
        message: "注册过于频繁，请稍后再试".to_string(),
        ..ApiError::busy()
    }
}

async fn sign_up(
    state: &AppState,
    headers: &HeaderMap,
    org: String,
    email: String,
    password: &str,
    name: Option<String>,
) -> Result<Response, ApiError> {
    let hash = state.passwords.hash(password).await?;
    let ctx = WriteCtx::of(state);
    let carried = carried_token(headers);
    let (token, view) = state
        .db
        .tx(move |conn| {
            async move {
                sqlx::query("SELECT pg_advisory_xact_lock($1)")
                    .bind(SIGNUP_LOCK_KEY)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let recent: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM audit_log \
                     WHERE action = 'tenant.signup' AND at > now() - make_interval(secs => $1)",
                )
                .bind(SIGNUP_WINDOW.as_secs_f64())
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if recent >= SIGNUPS_PER_MINUTE as i64 {
                    return Err(signups_busy());
                }
                // Served by the partial index `tenants_pending`.
                let pending: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM tenants WHERE status = 'pending'")
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                if pending >= MAX_PENDING_TENANTS {
                    tracing::warn!(
                        pending,
                        "refused a signup: the tenants waiting for approval are at their cap"
                    );
                    return Err(signups_busy());
                }
                let tenant = tenants::create(
                    &mut *conn,
                    &ctx,
                    &org,
                    TenantStatus::Pending,
                    BillingMode::Prepaid,
                    Actor::Unidentified,
                )
                .await?;
                let owner = create(
                    &mut *conn,
                    &ctx,
                    tenant.id,
                    &email,
                    &hash,
                    name.as_deref(),
                    MemberRole::Owner,
                )
                .await?;
                let token = start_session(&mut *conn, owner.id, carried.as_deref()).await?;
                audit::record(
                    &mut *conn,
                    AuditEntry {
                        actor: Actor::Member {
                            id: owner.id,
                            email: &owner.email,
                        },
                        action: "tenant.signup",
                        target_kind: "tenant",
                        target_id: Some(tenant.id.to_string()),
                        tenant_id: Some(tenant.id),
                        detail: json!({}),
                    },
                )
                .await?;
                let view = session_view(conn, owner.id).await?;
                Ok::<_, ApiError>((token, view))
            }
            .boxed()
        })
        .await?;
    Ok(with_cookie(StatusCode::CREATED, &token, view))
}

#[derive(Deserialize)]
pub struct LoginRequest {
    email: String,
    password: String,
}

/// `POST /api/login` → `200 Session` and the session cookie. A wrong
/// password, an unknown email, a disabled member and a removed one all
/// answer the same `401 invalid_credentials` after the same work (one
/// argon2 verification); a body that cannot be read answers it too. The
/// tenant's status does not matter here: a member of a pending or suspended
/// tenant signs in and sees why the tenant's routes are closed.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<ApiJson<LoginRequest>, ApiError>,
) -> Result<Response, ApiError> {
    let Ok(ApiJson(req)) = body else {
        return Err(ApiError::invalid_credentials());
    };
    let Some(email) = normalize_email(&req.email) else {
        return Err(ApiError::invalid_credentials());
    };
    // Before any argon2 work.
    if state.portal.logins.in_cooldown(&email) {
        return Ok(cooling_down());
    }
    let found: Option<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT id, password_hash, disabled FROM members WHERE email = $1 AND removed_at IS NULL",
    )
    .bind(&email)
    .fetch_optional(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let matched = state
        .passwords
        .verify(found.as_ref().map(|f| f.1.as_str()), &req.password)
        .await?;
    let Some((id, verified_hash, _)) = found.filter(|f| matched && !f.2) else {
        state.portal.logins.note_failure(&email);
        return Err(ApiError::invalid_credentials());
    };
    state.portal.logins.clear(&email);
    let carried = carried_token(&headers);
    let (token, view) = state
        .db
        .tx(move |conn| {
            async move {
                // The member was read and verified outside this transaction,
                // which can take seconds. A password change, a disable or a
                // removal that committed meanwhile has already ended the
                // member's sessions, before this one existed. So the row is
                // read again, with the hash just verified, enabled and not
                // removed, under a share lock: those changes update the row
                // before ending its sessions, so either this waits for one
                // to commit and then finds no row, or the change waits for
                // this session to commit and then ends it too.
                let unchanged: Option<bool> = sqlx::query_scalar(
                    "SELECT true FROM members WHERE id = $1 AND password_hash = $2 \
                     AND NOT disabled AND removed_at IS NULL FOR SHARE",
                )
                .bind(id)
                .bind(&verified_hash)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                if unchanged.is_none() {
                    return Err(ApiError::invalid_credentials());
                }
                let token = start_session(&mut *conn, id, carried.as_deref()).await?;
                let view = session_view(conn, id).await?;
                Ok::<_, ApiError>((token, view))
            }
            .boxed()
        })
        .await?;
    Ok(with_cookie(StatusCode::OK, &token, view))
}

/// `POST /api/logout`: ends the request's member session, if any, and
/// clears the cookie. It reads the portal cookie only, so an operator's
/// session is never ended here.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = session::token_from_headers(&headers, PORTAL_COOKIE) {
        session::revoke(state.db.pool(), token, PrincipalKind::Member).await?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, session::cleared_cookie(PORTAL_COOKIE))],
    )
        .into_response())
}

/// `GET /api/session`: `{authed, api_contract, member?, tenant?}`.
pub async fn session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let Some(auth) = MemberAuth::from_session(&headers, &state).await? else {
        return Ok(Json(
            json!({ "authed": false, "api_contract": API_CONTRACT }),
        ));
    };
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    Ok(Json(session_view(&mut conn, auth.id).await?))
}

/// `401 invalid_credentials`, "当前密码错误": the current password given is
/// not the member's, or no longer is.
fn wrong_current_password() -> ApiError {
    ApiError {
        message: "当前密码错误".to_string(),
        ..ApiError::invalid_credentials()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}

/// `POST /api/password`: the caller's own password, whatever the tenant's
/// status. Every other session of the caller ends; the one making the
/// change stays. Of two changes from the same current password, the one
/// that commits second is refused as a wrong current password, so it cannot
/// replace the first one's.
pub async fn change_password(
    auth: MemberAuth,
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ChangePassword>,
) -> Result<Response, ApiError> {
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    if !password::long_enough(&req.new_password) {
        return Err(ApiError::validation(vec![password_too_short(
            "/new_password",
        )]));
    }
    // A stolen session must not make guessing the password any easier than
    // the login does: the same cooldown applies.
    if state.portal.logins.in_cooldown(&auth.email) {
        return Ok(cooling_down());
    }
    let stored: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM members WHERE id = $1")
            .bind(auth.id)
            .fetch_optional(state.db.pool())
            .await
            .map_err(DbError::from)?;
    let matched = state
        .passwords
        .verify(stored.as_deref(), &req.current_password)
        .await?;
    let (true, Some(verified)) = (matched, stored) else {
        state.portal.logins.note_failure(&auth.email);
        return Err(wrong_current_password());
    };
    state.portal.logins.clear(&auth.email);
    let hash = state.passwords.hash(&req.new_password).await?;
    let current = carried_token(&headers);
    state
        .db
        .tx(move |conn| {
            async move {
                // Only over the hash just verified: a change that committed
                // since replaced the password this one was checked against,
                // and its own session would otherwise be ended by a
                // password that is no longer the member's.
                let updated = sqlx::query(
                    "UPDATE members SET password_hash = $2 \
                     WHERE id = $1 AND password_hash = $3 AND NOT disabled AND removed_at IS NULL",
                )
                .bind(auth.id)
                .bind(&hash)
                .bind(&verified)
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?
                .rows_affected();
                if updated == 0 {
                    let live: Option<bool> = sqlx::query_scalar(
                        "SELECT true FROM members \
                         WHERE id = $1 AND NOT disabled AND removed_at IS NULL",
                    )
                    .bind(auth.id)
                    .fetch_optional(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                    return Err(match live {
                        Some(_) => wrong_current_password(),
                        None => ApiError::unauthenticated(),
                    });
                }
                session::revoke_all(&mut *conn, Principal::Member(auth.id), current.as_deref())
                    .await?;
                audit::record(
                    conn,
                    member_entry(
                        member_actor(&auth),
                        "member.password",
                        auth.tenant_id,
                        auth.id,
                        json!({}),
                    ),
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Who issued an invitation.
#[derive(Clone, Copy)]
enum Inviter<'a> {
    Operator(&'a OperatorAuth),
    Member(&'a MemberAuth),
}

/// Issues an invitation to `tenant_id` for `email` as `role`, and records
/// it. The token is returned here once; only its SHA-256 is stored.
async fn create_invite(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    email: &str,
    role: MemberRole,
    by: Inviter<'_>,
) -> Result<(Uuid, String, DateTime<Utc>), ApiError> {
    let mut bytes = [0u8; TOKEN_LEN / 2];
    rand_core::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(ApiError::internal)?;
    let token = hex::encode(bytes);
    let id = Uuid::new_v4();
    let (kind, created_by, actor) = match by {
        Inviter::Operator(op) => ("operator", op.id, Actor::Operator(op)),
        Inviter::Member(m) => ("member", m.id, member_actor(m)),
    };
    let expires_at: DateTime<Utc> = sqlx::query_scalar(
        "INSERT INTO member_invites \
           (id, tenant_id, email, role, token_sha256, expires_at, created_by_kind, created_by) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6), $7, $8) \
         RETURNING expires_at",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(email)
    .bind(role.as_str())
    .bind(digest(&token))
    .bind(INVITE_TTL.as_secs_f64())
    .bind(kind)
    .bind(created_by)
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::from)?;
    audit::record(
        conn,
        AuditEntry {
            actor,
            action: "member.invite",
            target_kind: INVITE_TARGET_KIND,
            target_id: Some(id.to_string()),
            tenant_id: Some(tenant_id),
            detail: json!({ "email": email, "role": role }),
        },
    )
    .await?;
    Ok((id, token, expires_at))
}

/// `{id, invite_token, invite_url?, expires_at}`. The link is given only
/// when `CP_PORTAL_URL` is set; the token rides in its fragment, which
/// browsers do not send to servers, so it stays out of access logs.
fn invite_answer(state: &AppState, id: Uuid, token: &str, expires_at: DateTime<Utc>) -> Value {
    let mut answer = json!({ "id": id, "invite_token": token, "expires_at": expires_at });
    if let Some(base) = state.cfg.portal_url.as_deref() {
        answer["invite_url"] = json!(format!(
            "{}/invite#token={token}",
            base.trim_end_matches('/')
        ));
    }
    answer
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAccountRequest {
    email: String,
    password: String,
    role: MemberRole,
    display_name: Option<String>,
}

/// `POST /api/members` → `201 Member`：仅当前组织的管理员可创建账号，
/// 仅所有者可授予所有者角色，不修改请求者的会话。
pub async fn create_account(
    auth: MemberAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<CreateAccountRequest>,
) -> Result<(StatusCode, Json<MemberView>), ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    if req.role == MemberRole::Owner && auth.role != MemberRole::Owner {
        return Err(ApiError::forbidden());
    }
    let mut problems = Vec::new();
    let email = normalize_email(&req.email);
    if email.is_none() {
        problems.push(detail("/email", "邮箱格式不正确"));
    }
    if !password::long_enough(&req.password) {
        problems.push(password_too_short("/password"));
    }
    let name = display_name(req.display_name.as_deref()).unwrap_or_else(|d| {
        problems.push(d);
        None
    });
    let (Some(email), true) = (email, problems.is_empty()) else {
        return Err(ApiError::validation(problems));
    };
    let hash = state.passwords.hash(&req.password).await?;
    let ctx = WriteCtx::of(&state);
    let member = state
        .db
        .tx(move |conn| {
            async move {
                // 在组织锁下重读权限，防止等待期间降级或停用后继续创建。
                let auth =
                    current(&mut *conn, &auth, &[MemberRole::Owner, MemberRole::Admin]).await?;
                if req.role == MemberRole::Owner && auth.role != MemberRole::Owner {
                    return Err(ApiError::forbidden());
                }
                let member = create(
                    &mut *conn,
                    &ctx,
                    auth.tenant_id,
                    &email,
                    &hash,
                    name.as_deref(),
                    req.role,
                )
                .await?;
                audit::record(
                    conn,
                    member_entry(
                        member_actor(&auth),
                        "member.create",
                        auth.tenant_id,
                        member.id,
                        json!({ "role": member.role }),
                    ),
                )
                .await?;
                Ok::<_, ApiError>(member.view())
            }
            .boxed()
        })
        .await?;
    Ok((StatusCode::CREATED, Json(member)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InviteRequest {
    email: String,
    role: MemberRole,
}

fn invite_email(raw: &str) -> Result<String, ApiError> {
    normalize_email(raw)
        .ok_or_else(|| ApiError::validation(vec![detail("/email", "邮箱格式不正确")]))
}

/// `POST /api/members/invites` with `{email, role}` → `201 {id,
/// invite_token, invite_url?, expires_at}`. Only an owner invites an owner.
pub async fn invite(
    auth: MemberAuth,
    State(state): State<AppState>,
    ApiJson(req): ApiJson<InviteRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    let email = invite_email(&req.email)?;
    let (id, token, expires_at) = state
        .db
        .tx(move |conn| {
            async move {
                let auth =
                    current(&mut *conn, &auth, &[MemberRole::Owner, MemberRole::Admin]).await?;
                if req.role == MemberRole::Owner && auth.role != MemberRole::Owner {
                    return Err(ApiError::forbidden());
                }
                create_invite(
                    conn,
                    auth.tenant_id,
                    &email,
                    req.role,
                    Inviter::Member(&auth),
                )
                .await
            }
            .boxed()
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(invite_answer(&state, id, &token, expires_at)),
    ))
}

/// `GET /api/members/invites` → `{items: [{id, email, role, expires_at,
/// created_at, invited_by}]}`: the invitations still open (neither
/// accepted, revoked nor expired), oldest first. `invited_by` is the
/// issuing member's `{id, display_name}`, or null for an operator's.
pub async fn list_invites(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    type Raw = (
        Uuid,
        String,
        String,
        DateTime<Utc>,
        DateTime<Utc>,
        Option<Uuid>,
        Option<String>,
    );
    let rows: Vec<Raw> = sqlx::query_as(
        "SELECT i.id, i.email, i.role, i.expires_at, i.created_at, m.id, m.display_name \
         FROM member_invites i \
         LEFT JOIN members m ON i.created_by_kind = 'member' AND m.id = i.created_by \
         WHERE i.tenant_id = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL \
           AND i.expires_at > now() \
         ORDER BY i.created_at, i.id",
    )
    .bind(auth.tenant_id)
    .fetch_all(state.db.pool())
    .await
    .map_err(DbError::from)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(id, email, role, expires_at, created_at, issuer, issuer_name)| {
                let invited_by = issuer.map(|id| json!({"id": id, "display_name": issuer_name}));
                json!({"id": id, "email": email, "role": role, "expires_at": expires_at,
                       "created_at": created_at, "invited_by": invited_by})
            },
        )
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `DELETE /api/members/invites/{id}` → `204`: an open invitation of the
/// caller's tenant can no longer be accepted. Only an owner revokes an
/// owner's invitation.
pub async fn revoke_invite(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    let id = parse_id(&id)?;
    state
        .db
        .tx(move |conn| {
            async move {
                let auth =
                    current(&mut *conn, &auth, &[MemberRole::Owner, MemberRole::Admin]).await?;
                let role: Option<String> = sqlx::query_scalar(
                    "SELECT role FROM member_invites \
                     WHERE id = $1 AND tenant_id = $2 AND accepted_at IS NULL \
                       AND revoked_at IS NULL AND expires_at > now() \
                     FOR UPDATE",
                )
                .bind(id)
                .bind(auth.tenant_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let role: MemberRole = role
                    .ok_or_else(ApiError::not_found)?
                    .parse()
                    .map_err(ApiError::internal)?;
                if role == MemberRole::Owner && auth.role != MemberRole::Owner {
                    return Err(ApiError::forbidden());
                }
                sqlx::query("UPDATE member_invites SET revoked_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                audit::record(
                    conn,
                    AuditEntry {
                        actor: member_actor(&auth),
                        action: "member.invite_revoke",
                        target_kind: INVITE_TARGET_KIND,
                        target_id: Some(id.to_string()),
                        tenant_id: Some(auth.tenant_id),
                        detail: json!({}),
                    },
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptInvite {
    token: String,
    password: String,
    display_name: Option<String>,
}

/// `POST /api/invites/accept` with `{token, password, display_name?}` →
/// `201 Session` and the session cookie: the invited email joins the
/// invitation's tenant with its role. A token that is invalid, expired,
/// revoked or used is always `404 not_found`; `409 email_taken` when the
/// email is a live member's by now.
///
/// That 409 tells whoever chose the email, the inviter, whether the address
/// is a live member somewhere: by inviting it and accepting their own
/// invitation, they probe it. Ruling 12 accepts that whether an email is
/// registered can be probed, as signup answers the same `email_taken`;
/// each probe here costs an invitation and an argon2 hash behind the gate.
pub async fn accept_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<AcceptInvite>,
) -> Result<Response, ApiError> {
    let mut problems = Vec::new();
    if !password::long_enough(&req.password) {
        problems.push(password_too_short("/password"));
    }
    let name = display_name(req.display_name.as_deref()).unwrap_or_else(|d| {
        problems.push(d);
        None
    });
    if !problems.is_empty() {
        return Err(ApiError::validation(problems));
    }
    if !well_formed(&req.token) {
        return Err(ApiError::not_found());
    }
    let token_sha256 = digest(&req.token);
    const OPEN: &str = "token_sha256 = $1 AND accepted_at IS NULL AND revoked_at IS NULL \
                        AND expires_at > now()";
    // Before any argon2 work: a made-up token must not get to spend it.
    let open: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM member_invites WHERE {OPEN})"
    ))
    .bind(&token_sha256)
    .fetch_one(state.db.pool())
    .await
    .map_err(DbError::from)?;
    if !open {
        return Err(ApiError::not_found());
    }
    let hash = state.passwords.hash(&req.password).await?;
    let ctx = WriteCtx::of(&state);
    let carried = carried_token(&headers);
    let (token, view) = state
        .db
        .tx(move |conn| {
            async move {
                // The tenant first, as every change to its members does: the
                // member row this adds takes a key-share lock on the tenant
                // row (its foreign key), after the resources lock `create`
                // holds, so a change holding the tenant while it waits for
                // the resources lock would otherwise deadlock with it.
                let tenant_id: Uuid = sqlx::query_scalar(&format!(
                    "SELECT tenant_id FROM member_invites WHERE {OPEN}"
                ))
                .bind(&token_sha256)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?
                .ok_or_else(ApiError::not_found)?;
                tenants::lock(&mut *conn, tenant_id).await?;
                // Locked, so two acceptances of one invitation cannot both
                // find it open: the second waits, then finds it used.
                let invite: Option<(Uuid, String, String)> = sqlx::query_as(&format!(
                    "SELECT id, email, role FROM member_invites WHERE {OPEN} AND tenant_id = $2 \
                     FOR UPDATE"
                ))
                .bind(&token_sha256)
                .bind(tenant_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?;
                let (invite_id, email, role) = invite.ok_or_else(ApiError::not_found)?;
                let role: MemberRole = role.parse().map_err(ApiError::internal)?;
                let member = create(
                    &mut *conn,
                    &ctx,
                    tenant_id,
                    &email,
                    &hash,
                    name.as_deref(),
                    role,
                )
                .await?;
                sqlx::query("UPDATE member_invites SET accepted_at = now() WHERE id = $1")
                    .bind(invite_id)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let token = start_session(&mut *conn, member.id, carried.as_deref()).await?;
                audit::record(
                    &mut *conn,
                    member_entry(
                        Actor::Member {
                            id: member.id,
                            email: &member.email,
                        },
                        "member.join",
                        tenant_id,
                        member.id,
                        json!({ "invite_id": invite_id, "role": role }),
                    ),
                )
                .await?;
                let view = session_view(conn, member.id).await?;
                Ok::<_, ApiError>((token, view))
            }
            .boxed()
        })
        .await?;
    Ok(with_cookie(StatusCode::CREATED, &token, view))
}

/// `GET /api/org` → `{id, name, status, limits: {key_concurrency_max?,
/// key_rpm_max?}}`, whatever the tenant's status: the caps a key of the
/// tenant may be given, and nothing about groups or upstreams.
pub async fn org(auth: MemberAuth, State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let (name, status, limits): (String, String, Value) =
        sqlx::query_as("SELECT name, status, limits FROM tenants WHERE id = $1")
            .bind(auth.tenant_id)
            .fetch_one(state.db.pool())
            .await
            .map_err(DbError::from)?;
    let limits: TenantLimits = serde_json::from_value(limits).map_err(ApiError::internal)?;
    let caps: Map<String, Value> = [
        ("key_concurrency_max", limits.key_concurrency_max),
        ("key_rpm_max", limits.key_rpm_max),
    ]
    .into_iter()
    .filter_map(|(field, value)| value.map(|v| (field.to_string(), json!(v))))
    .collect();
    Ok(Json(json!({
        "id": auth.tenant_id,
        "name": name,
        "status": status,
        "limits": caps,
    })))
}

/// `GET /api/models` → `{items: [{id, name}]}`: the live models the tenant
/// reaches through its model scope, by their platform name only, in name
/// order.
pub async fn models(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require_active()?;
    auth.require(&[
        MemberRole::Owner,
        MemberRole::Admin,
        MemberRole::Member,
        MemberRole::Billing,
    ])?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let tenant = tenants::find(&mut conn, auth.tenant_id).await?;
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT r.id, COALESCE(r.doc->>'display_name', '') FROM gw_resources r \
         WHERE r.kind = $2 AND NOT r.deleted AND r.id = ANY($1) \
         ORDER BY 2, 1",
    )
    .bind(&tenant.effective_model_ids)
    .bind(Kind::Models.segment())
    .fetch_all(&mut *conn)
    .await
    .map_err(DbError::from)?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `GET /api/members` → `{items: [Member]}`. Open while the tenant is
/// pending or suspended too, so that a member can be found to be removed
/// (`DELETE /api/members/{id}`). A read: the caller is judged as the
/// session's own read of them found them, and no tenant lock is waited for.
pub async fn list(
    auth: MemberAuth,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    Ok(Json(members_of(&mut conn, auth.tenant_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMember {
    role: MemberRole,
}

/// `PATCH /api/members/{id}` with `{role}` → `200 Member`. Only an owner
/// changes an owner or makes one; the last enabled owner keeps the role
/// (`409 last_owner`). A move to or from billing closes or reopens the
/// member's keys in the same transaction; a reopening the gateway-version
/// gate refuses is `503 unavailable`, as on every `/api` key write.
pub async fn update(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<UpdateMember>,
) -> Result<Json<MemberView>, ApiError> {
    auth.require_active()?;
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    let id = parse_id(&id)?;
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let auth =
                    current(&mut *conn, &auth, &[MemberRole::Owner, MemberRole::Admin]).await?;
                let target = live_member(&mut *conn, auth.tenant_id, id).await?;
                let owner_involved =
                    target.role == MemberRole::Owner || req.role == MemberRole::Owner;
                if owner_involved && auth.role != MemberRole::Owner {
                    return Err(ApiError::forbidden());
                }
                if req.role == target.role {
                    return Ok(target.view());
                }
                if target.role == MemberRole::Owner && !target.disabled {
                    refuse_last_owner(&mut *conn, auth.tenant_id, target.id).await?;
                }
                sqlx::query("UPDATE members SET role = $2 WHERE id = $1")
                    .bind(id)
                    .bind(req.role.as_str())
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                // What the new role can no longer invite, the member's open
                // invitations no longer offer.
                let lost = match (target.role, req.role) {
                    (_, MemberRole::Member | MemberRole::Billing) => Some(true),
                    (MemberRole::Owner, MemberRole::Admin) => Some(false),
                    _ => None,
                };
                let revoked = match lost {
                    Some(all) => revoke_issued_invites(&mut *conn, auth.tenant_id, id, all).await?,
                    None => 0,
                };
                // A billing member holds no working key: their keys close
                // with the demotion and open again with a promotion.
                if KEY_HOLDERS.contains(&target.role) != KEY_HOLDERS.contains(&req.role) {
                    keys::reproject_member(&mut *conn, &ctx, id).await?;
                }
                audit::record(
                    conn,
                    member_entry(
                        member_actor(&auth),
                        "member.update",
                        auth.tenant_id,
                        id,
                        json!({ "role": req.role, "invites_revoked": revoked }),
                    ),
                )
                .await?;
                Ok::<_, ApiError>(
                    MemberRow {
                        role: req.role,
                        ..target
                    }
                    .view(),
                )
            }
            .boxed()
        })
        .await
        .map_err(keys::on_api)?;
    Ok(Json(view))
}

/// `DELETE /api/members/{id}` → `204`. The member is marked removed (their
/// email can be invited again), their sessions end, their keys are revoked
/// and their `users` document is deleted, in one transaction. Only an owner
/// removes an owner; the last enabled owner stays (`409 last_owner`). Open
/// while the tenant is pending or suspended too: someone who left must not
/// get their access back with a reactivation.
pub async fn remove(
    auth: MemberAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    auth.require(&[MemberRole::Owner, MemberRole::Admin])?;
    let id = parse_id(&id)?;
    let ctx = WriteCtx::of(&state);
    state
        .db
        .tx(move |conn| {
            async move {
                let auth = current_in_any_state(
                    &mut *conn,
                    &auth,
                    &[MemberRole::Owner, MemberRole::Admin],
                )
                .await?;
                let target = live_member(&mut *conn, auth.tenant_id, id).await?;
                if target.role == MemberRole::Owner {
                    if auth.role != MemberRole::Owner {
                        return Err(ApiError::forbidden());
                    }
                    if !target.disabled {
                        refuse_last_owner(&mut *conn, auth.tenant_id, target.id).await?;
                    }
                }
                // The row first: a login in flight re-reads it under a share
                // lock before it starts a session (see `login`).
                sqlx::query("UPDATE members SET removed_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                session::revoke_all(&mut *conn, Principal::Member(id), None).await?;
                let revoked = revoke_issued_invites(&mut *conn, auth.tenant_id, id, true).await?;
                // Before their `users` document goes: a key document names
                // its user. A pending tenant's member has none yet.
                let keys_revoked = keys::revoke_member(&mut *conn, &ctx, id).await?;
                tenants::retire(&mut *conn, &ctx, Kind::Users, id, &owner(id)).await?;
                audit::record(
                    conn,
                    member_entry(
                        member_actor(&auth),
                        "member.remove",
                        auth.tenant_id,
                        id,
                        json!({ "invites_revoked": revoked, "keys_revoked": keys_revoked }),
                    ),
                )
                .await?;
                Ok::<_, ApiError>(())
            }
            .boxed()
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /admin/tenants/{id}/members` → `{items: [Member]}`.
pub async fn admin_list(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    auth.require(OperatorRole::Viewer)?;
    let id = parse_id(&id)?;
    let mut conn = state.db.pool().acquire().await.map_err(DbError::from)?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::from)?;
    if !exists {
        return Err(ApiError::not_found());
    }
    Ok(Json(members_of(&mut conn, id).await?))
}

/// `POST /admin/tenants/{id}/invites` with `{email, role}` → `201 {id,
/// invite_token, invite_url?, expires_at}`, in a tenant of any status. The
/// platform's own tenant has no members (`409 tenant_state`): its keys are
/// the operators', managed on this API.
pub async fn admin_invite(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<InviteRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    auth.require(OperatorRole::Operator)?;
    let tenant_id = parse_id(&id)?;
    let email = invite_email(&req.email)?;
    let (id, token, expires_at) = state
        .db
        .tx(move |conn| {
            async move {
                let tenant = tenants::lock(&mut *conn, tenant_id).await?;
                if tenant.is_internal {
                    return Err(ApiError::conflict(
                        "tenant_state",
                        "平台内部租户没有成员，不能邀请",
                    ));
                }
                create_invite(conn, tenant_id, &email, req.role, Inviter::Operator(&auth)).await
            }
            .boxed()
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(invite_answer(&state, id, &token, expires_at)),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminUpdateMember {
    disabled: bool,
}

/// `PATCH /admin/members/{id}` with `{disabled}` → `200 Member`. Disabling
/// ends the member's sessions, and the member's keys are disabled or
/// enabled again with them (a key switched off itself stays off), in the
/// same transaction; the last enabled owner of a tenant cannot be disabled
/// (`409 last_owner`).
pub async fn admin_update(
    auth: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<AdminUpdateMember>,
) -> Result<Json<MemberView>, ApiError> {
    auth.require(OperatorRole::Operator)?;
    let id = parse_id(&id)?;
    let ctx = WriteCtx::of(&state);
    let view = state
        .db
        .tx(move |conn| {
            async move {
                let tenant_id: Uuid = sqlx::query_scalar(
                    "SELECT tenant_id FROM members WHERE id = $1 AND removed_at IS NULL",
                )
                .bind(id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(DbError::from)?
                .ok_or_else(ApiError::not_found)?;
                tenants::lock(&mut *conn, tenant_id).await?;
                let target = live_member(&mut *conn, tenant_id, id).await?;
                if target.disabled == req.disabled {
                    return Ok(target.view());
                }
                if req.disabled && target.role == MemberRole::Owner {
                    refuse_last_owner(&mut *conn, tenant_id, id).await?;
                }
                // The row first: a login in flight re-reads it under a share
                // lock before it starts a session (see `login`).
                sqlx::query("UPDATE members SET disabled = $2 WHERE id = $1")
                    .bind(id)
                    .bind(req.disabled)
                    .execute(&mut *conn)
                    .await
                    .map_err(DbError::from)?;
                let detail = if req.disabled {
                    session::revoke_all(&mut *conn, Principal::Member(id), None).await?;
                    let revoked = revoke_issued_invites(&mut *conn, tenant_id, id, true).await?;
                    json!({ "invites_revoked": revoked })
                } else {
                    json!({})
                };
                // The member's keys follow, in the same transaction.
                keys::reproject_member(&mut *conn, &ctx, id).await?;
                let action = if req.disabled {
                    "member.disable"
                } else {
                    "member.enable"
                };
                audit::record(
                    conn,
                    member_entry(Actor::Operator(&auth), action, tenant_id, id, detail),
                )
                .await?;
                Ok::<_, ApiError>(
                    MemberRow {
                        disabled: req.disabled,
                        ..target
                    }
                    .view(),
                )
            }
            .boxed()
        })
        .await?;
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signups_are_bounded_per_minute_and_a_slot_can_be_handed_back() {
        let throttle = SignupThrottle::default();
        let t0 = Instant::now();
        let taken: Vec<Instant> = (0..SIGNUPS_PER_MINUTE as u64)
            .map(|i| {
                throttle
                    .try_take_at(t0 + Duration::from_millis(i))
                    .expect("a slot")
            })
            .collect();
        let full = t0 + Duration::from_secs(1);
        assert_eq!(throttle.try_take_at(full), None);
        // A handed-back slot can be taken again.
        throttle.give_back(taken[7]);
        assert!(throttle.try_take_at(full).is_some());
        assert_eq!(throttle.try_take_at(full), None);
        // A slot frees itself a minute after it was taken.
        assert!(throttle.try_take_at(t0 + SIGNUP_WINDOW).is_some());
        assert_eq!(throttle.try_take_at(t0 + SIGNUP_WINDOW), None);
        // Handing back a slot that is not held does nothing.
        throttle.give_back(t0 - Duration::from_secs(1));
        assert_eq!(throttle.try_take_at(t0 + SIGNUP_WINDOW), None);
    }

    #[test]
    fn a_user_document_is_named_after_the_display_name_or_the_id() {
        let id = Uuid::parse_str("0123abcd-4567-89ef-0123-456789abcdef").expect("a uuid");
        assert_eq!(user_doc(id, None), json!({"name": "member-0123abcd"}));
        assert_eq!(user_doc(id, Some("Ada")), json!({"name": "Ada"}));
    }

    #[test]
    fn display_names_are_trimmed_and_bounded() {
        assert_eq!(display_name(None), Ok(None));
        assert_eq!(display_name(Some("   ")), Ok(None));
        assert_eq!(display_name(Some(" Ada ")), Ok(Some("Ada".to_string())));
        assert!(display_name(Some(&"名".repeat(MAX_DISPLAY_NAME_CHARS))).is_ok());
        assert!(display_name(Some(&"名".repeat(MAX_DISPLAY_NAME_CHARS + 1))).is_err());
        assert!(display_name(Some("a\u{7}b")).is_err());
    }
}
